//! The Sharp LR35902 / SM83 CPU core.
//!
//! `Cpu::step()` executes one instruction and returns the T-cycles it took — the
//! direct analog of CHIP-8's `Chip8::cycle()`, just for a real CPU. That return value
//! is the "catch-up" timing seam: `step()` ticks the bus (PPU + timer) by exactly that
//! many cycles. The same shape maps onto the MIPS interpreter we'll write for the PS1.
//!
//! NOTE: SM83 is *not* a Z80 and *not* an 8080. No IX/IY, no shadow registers, no
//! port I/O; it has its own `LD (HL+/-)`, `LDH (0xFF00+n)`, and `SWAP`. Always decode
//! against Pan Docs' opcode table, never a generic Z80 reference.

use crate::bus::Bus;

// Flag bits live in the high nibble of F. The low nibble is physically always 0.
pub const FLAG_Z: u8 = 1 << 7; // zero
pub const FLAG_N: u8 = 1 << 6; // subtract (used by DAA)
pub const FLAG_H: u8 = 1 << 5; // half-carry (carry out of bit 3 / 11)
pub const FLAG_C: u8 = 1 << 4; // carry

pub struct Cpu {
    // 8-bit registers. A is the accumulator; F holds the flags (high nibble only).
    pub a: u8,
    pub f: u8,
    pub b: u8,
    pub c: u8,
    pub d: u8,
    pub e: u8,
    pub h: u8,
    pub l: u8,
    pub sp: u16,
    pub pc: u16,

    pub ime: bool,         // interrupt master enable
    pub ime_pending: bool, // EI turns IME on AFTER the next instruction (the EI delay)
    pub halted: bool,
    pub halt_bug: bool, // HALT-bug latch: next opcode fetch won't advance PC

    /// The CPU owns the bus; every memory access goes through it.
    pub bus: Bus,
}

impl Cpu {
    /// Construct with the documented post-boot register state (HLE — we skip running
    /// Nintendo's boot ROM and just start where it would leave us, at PC=0x0100). The
    /// post-boot I/O register defaults are set in each device's `new()`.
    pub fn new(bus: Bus) -> Self {
        Self {
            a: 0x01,
            f: 0xB0,
            b: 0x00,
            c: 0x13,
            d: 0x00,
            e: 0xD8,
            h: 0x01,
            l: 0x4D,
            sp: 0xFFFE,
            pc: 0x0100,
            ime: false,
            ime_pending: false,
            halted: false,
            halt_bug: false,
            bus,
        }
    }

    // ===== 16-bit register pairs (from Liam's DMG-01 start) =====================
    pub fn af(&self) -> u16 {
        (self.a as u16) << 8 | self.f as u16
    }
    pub fn bc(&self) -> u16 {
        (self.b as u16) << 8 | self.c as u16
    }
    pub fn de(&self) -> u16 {
        (self.d as u16) << 8 | self.e as u16
    }
    pub fn hl(&self) -> u16 {
        (self.h as u16) << 8 | self.l as u16
    }
    pub fn set_af(&mut self, v: u16) {
        self.a = (v >> 8) as u8;
        self.f = (v as u8) & 0xF0; // low nibble of F is always 0 (mask on POP AF etc.)
    }
    pub fn set_bc(&mut self, v: u16) {
        self.b = (v >> 8) as u8;
        self.c = v as u8;
    }
    pub fn set_de(&mut self, v: u16) {
        self.d = (v >> 8) as u8;
        self.e = v as u8;
    }
    pub fn set_hl(&mut self, v: u16) {
        self.h = (v >> 8) as u8;
        self.l = v as u8;
    }

    // ===== Flag helpers ========================================================
    pub fn flag(&self, mask: u8) -> bool {
        self.f & mask != 0
    }
    pub fn set_flag(&mut self, mask: u8, on: bool) {
        if on {
            self.f |= mask;
        } else {
            self.f &= !mask;
        }
        self.f &= 0xF0; // the low nibble of F can never be set
    }

    // ===== Memory / operand / stack helpers ====================================
    // Read the byte at PC and advance — unless the HALT bug latch is set, in which
    // case we read the same byte again without advancing (then clear the latch).
    fn fetch8(&mut self) -> u8 {
        let byte = self.bus.read(self.pc);
        if self.halt_bug {
            self.halt_bug = false;
        } else {
            self.pc = self.pc.wrapping_add(1);
        }
        byte
    }

    // 16-bit immediates are stored little-endian (low byte first).
    fn fetch16(&mut self) -> u16 {
        let lo = self.fetch8() as u16;
        let hi = self.fetch8() as u16;
        (hi << 8) | lo
    }

    // The stack grows downward; SP points at the top item. Push high byte first so a
    // following pop reads low-then-high back into the right order.
    pub fn push16(&mut self, val: u16) {
        self.sp = self.sp.wrapping_sub(1);
        self.bus.write(self.sp, (val >> 8) as u8);
        self.sp = self.sp.wrapping_sub(1);
        self.bus.write(self.sp, val as u8);
    }
    pub fn pop16(&mut self) -> u16 {
        let lo = self.bus.read(self.sp) as u16;
        self.sp = self.sp.wrapping_add(1);
        let hi = self.bus.read(self.sp) as u16;
        self.sp = self.sp.wrapping_add(1);
        (hi << 8) | lo
    }

    // Map the 3-bit register field used all over the opcode map to a register value.
    // Encoding: 0=B 1=C 2=D 3=E 4=H 5=L 6=(HL) 7=A. The (HL) case touches memory.
    fn reg(&mut self, idx: u8) -> u8 {
        match idx & 7 {
            0 => self.b,
            1 => self.c,
            2 => self.d,
            3 => self.e,
            4 => self.h,
            5 => self.l,
            6 => self.bus.read(self.hl()),
            _ => self.a,
        }
    }
    fn set_reg(&mut self, idx: u8, val: u8) {
        match idx & 7 {
            0 => self.b = val,
            1 => self.c = val,
            2 => self.d = val,
            3 => self.e = val,
            4 => self.h = val,
            5 => self.l = val,
            6 => {
                let addr = self.hl();
                self.bus.write(addr, val);
            }
            _ => self.a = val,
        }
    }

    // ===== The main loop: interrupts, HALT, then one instruction ===============
    /// Execute one step of the machine and return the T-cycles it consumed. Also ticks
    /// the bus (timer + PPU) by that many cycles — the catch-up timing seam.
    pub fn step(&mut self) -> u8 {
        let cycles = self.run_one();
        self.bus.tick(cycles);
        cycles
    }

    fn run_one(&mut self) -> u8 {
        // 1. Interrupts are checked before fetching the next instruction.
        if let Some(cycles) = self.handle_interrupt() {
            return cycles;
        }
        // 2. If still halted (nothing woke us), the CPU idles for one M-cycle.
        if self.halted {
            return 4;
        }
        // 3. The EI delay: IME flips on AFTER the instruction that follows EI. We latch
        //    the pending state now and apply it only once this instruction has run.
        let enable_ime_after = self.ime_pending;

        // 4. Fetch + decode + execute one instruction.
        let op = self.fetch8();
        let cycles = self.dispatch(op);

        if enable_ime_after {
            self.ime = true;
            self.ime_pending = false;
        }
        cycles
    }

    /// If an interrupt is pending+enabled, wake from HALT and (when IME is on) service
    /// the highest-priority one: disable IME, clear its IF bit, push PC, jump to its
    /// vector. Returns the dispatch cost, or None if nothing was serviced.
    fn handle_interrupt(&mut self) -> Option<u8> {
        let pending = self.bus.ints.pending(); // IF & IE & 0x1F
        if pending == 0 {
            return None;
        }
        // A pending+enabled interrupt wakes the CPU even if IME is off.
        self.halted = false;
        if !self.ime {
            return None; // awake, but we don't dispatch while interrupts are disabled
        }

        // Lowest set bit = highest priority (VBlank=0 .. Joypad=4). Vector = 0x40+bit*8.
        let bit = pending.trailing_zeros() as u8;
        self.ime = false;
        self.bus.ints.clear(1 << bit);
        let return_addr = self.pc;
        self.push16(return_addr);
        self.pc = 0x40 + (bit as u16) * 8;
        Some(20) // interrupt dispatch costs 5 M-cycles
    }

    /// HALT: stop the CPU until an interrupt is pending.
    fn halt(&mut self) {
        // HALT bug: if IME is off but an interrupt is ALREADY pending, the CPU does not
        // halt — instead the byte after HALT is read twice (PC fails to advance once).
        // Real games rely on this; we model it with the `halt_bug` fetch latch.
        let pending = self.bus.ints.pending() != 0;
        if !self.ime && pending {
            self.halt_bug = true;
        } else {
            self.halted = true;
        }
    }

    // ===== ALU primitives ======================================================
    // Each operates on the accumulator the way the hardware does and sets F to match.
    // Implementing the flag rules ONCE here (instead of in every opcode arm) is what
    // keeps Blargg-level correctness tractable — the half-carry rules in particular are
    // the #1 source of subtle bugs, so they exist in exactly one place per operation.

    fn alu_add(&mut self, val: u8) {
        let (res, carry) = self.a.overflowing_add(val);
        self.set_flag(FLAG_Z, res == 0);
        self.set_flag(FLAG_N, false);
        self.set_flag(FLAG_H, hc_add(self.a, val));
        self.set_flag(FLAG_C, carry);
        self.a = res;
    }
    // ADC folds the incoming carry into BOTH the result and the half/full-carry tests,
    // so it can't reuse overflowing_add (that would miss the +1 spilling a nibble/byte).
    fn alu_adc(&mut self, val: u8) {
        let carry = self.flag(FLAG_C) as u8;
        let res = self.a.wrapping_add(val).wrapping_add(carry);
        self.set_flag(FLAG_Z, res == 0);
        self.set_flag(FLAG_N, false);
        self.set_flag(FLAG_H, (self.a & 0x0F) + (val & 0x0F) + carry > 0x0F);
        self.set_flag(FLAG_C, self.a as u16 + val as u16 + carry as u16 > 0xFF);
        self.a = res;
    }
    fn alu_sub(&mut self, val: u8) {
        let (res, borrow) = self.a.overflowing_sub(val);
        self.set_flag(FLAG_Z, res == 0);
        self.set_flag(FLAG_N, true);
        self.set_flag(FLAG_H, hc_sub(self.a, val)); // half-BORROW, not hc_add
        self.set_flag(FLAG_C, borrow); // overflowing_sub's overflow flag == borrow
        self.a = res;
    }
    fn alu_sbc(&mut self, val: u8) {
        let carry = self.flag(FLAG_C) as u8;
        let res = self.a.wrapping_sub(val).wrapping_sub(carry);
        self.set_flag(FLAG_Z, res == 0);
        self.set_flag(FLAG_N, true);
        self.set_flag(FLAG_H, (self.a & 0x0F) < (val & 0x0F) + carry);
        self.set_flag(FLAG_C, (self.a as u16) < val as u16 + carry as u16);
        self.a = res;
    }
    // Logical ops always clear C and N. The H asymmetry is a classic Blargg catch:
    // AND sets H=1, while OR and XOR set H=0.
    fn alu_and(&mut self, val: u8) {
        self.a &= val;
        self.set_flag(FLAG_Z, self.a == 0);
        self.set_flag(FLAG_N, false);
        self.set_flag(FLAG_H, true);
        self.set_flag(FLAG_C, false);
    }
    fn alu_or(&mut self, val: u8) {
        self.a |= val;
        self.set_flag(FLAG_Z, self.a == 0);
        self.set_flag(FLAG_N, false);
        self.set_flag(FLAG_H, false);
        self.set_flag(FLAG_C, false);
    }
    fn alu_xor(&mut self, val: u8) {
        self.a ^= val;
        self.set_flag(FLAG_Z, self.a == 0);
        self.set_flag(FLAG_N, false);
        self.set_flag(FLAG_H, false);
        self.set_flag(FLAG_C, false);
    }
    // CP is SUB that throws the result away — it only sets flags (used for comparisons).
    fn alu_cp(&mut self, val: u8) {
        let res = self.a.wrapping_sub(val);
        self.set_flag(FLAG_Z, res == 0);
        self.set_flag(FLAG_N, true);
        self.set_flag(FLAG_H, hc_sub(self.a, val));
        self.set_flag(FLAG_C, self.a < val);
    }
    // INC/DEC of an 8-bit value: they set Z/N/H but leave C UNTOUCHED (the one trap that
    // separates them from ADD/SUB). They return the new value so the caller can store it
    // back into a register or (HL).
    fn alu_inc(&mut self, val: u8) -> u8 {
        let res = val.wrapping_add(1);
        self.set_flag(FLAG_Z, res == 0);
        self.set_flag(FLAG_N, false);
        self.set_flag(FLAG_H, (val & 0x0F) == 0x0F); // nibble was full -> carries
        res
    }
    fn alu_dec(&mut self, val: u8) -> u8 {
        let res = val.wrapping_sub(1);
        self.set_flag(FLAG_Z, res == 0);
        self.set_flag(FLAG_N, true);
        self.set_flag(FLAG_H, (val & 0x0F) == 0x00); // nibble was empty -> borrows
        res
    }
    // 16-bit ADD HL,rr: N=0, H = carry out of bit 11, C = carry out of bit 15; Z is left
    // alone (a rare case of an arithmetic op not touching Z).
    fn add_hl(&mut self, val: u16) {
        let hl = self.hl();
        let (res, carry) = hl.overflowing_add(val);
        self.set_flag(FLAG_N, false);
        self.set_flag(FLAG_H, (hl & 0x0FFF) + (val & 0x0FFF) > 0x0FFF);
        self.set_flag(FLAG_C, carry);
        self.set_hl(res);
    }
    // ADD SP,e and LD HL,SP+e share this: the offset is SIGNED for the result, but H and
    // C are computed from the UNSIGNED low-byte addition, and Z and N are forced to 0.
    fn add_sp_e(&mut self, e: i8) -> u16 {
        let sp = self.sp;
        let off = e as i16 as u16;
        self.set_flag(FLAG_Z, false);
        self.set_flag(FLAG_N, false);
        self.set_flag(FLAG_H, (sp & 0x000F) + (off & 0x000F) > 0x000F);
        self.set_flag(FLAG_C, (sp & 0x00FF) + (off & 0x00FF) > 0x00FF);
        sp.wrapping_add(off)
    }
    // DAA: after a BCD add or subtract, nudge A back into packed-BCD form using N/H/C.
    // This is the single fiddliest non-CB op; the form below is the standard one that
    // passes Blargg. N is left unchanged, H is always cleared.
    fn daa(&mut self) {
        let mut a = self.a;
        let mut adjust: u8 = 0;
        let mut set_carry = false;
        if self.flag(FLAG_H) || (!self.flag(FLAG_N) && (a & 0x0F) > 0x09) {
            adjust |= 0x06;
        }
        if self.flag(FLAG_C) || (!self.flag(FLAG_N) && a > 0x99) {
            adjust |= 0x60;
            set_carry = true;
        }
        a = if self.flag(FLAG_N) {
            a.wrapping_sub(adjust)
        } else {
            a.wrapping_add(adjust)
        };
        self.a = a;
        self.set_flag(FLAG_Z, a == 0);
        self.set_flag(FLAG_H, false);
        self.set_flag(FLAG_C, set_carry);
    }

    // Decode the 2-bit condition field (NZ=0, Z=1, NC=2, C=3) used by JR/JP/CALL/RET cc.
    fn cond(&self, cc: u8) -> bool {
        match cc & 3 {
            0 => !self.flag(FLAG_Z),
            1 => self.flag(FLAG_Z),
            2 => !self.flag(FLAG_C),
            _ => self.flag(FLAG_C),
        }
    }

    // ===== CB-prefixed rotate/shift primitives =================================
    // Unlike the A-rotates (RLCA etc.), these set Z from the *result*. N and H are always
    // cleared; C takes the bit shifted out. set_shift_flags centralizes that rule.
    fn set_shift_flags(&mut self, res: u8, carry: u8) {
        self.set_flag(FLAG_Z, res == 0);
        self.set_flag(FLAG_N, false);
        self.set_flag(FLAG_H, false);
        self.set_flag(FLAG_C, carry != 0);
    }
    fn cb_rlc(&mut self, v: u8) -> u8 {
        let c = v >> 7;
        let r = (v << 1) | c; // bit 7 wraps into bit 0
        self.set_shift_flags(r, c);
        r
    }
    fn cb_rrc(&mut self, v: u8) -> u8 {
        let c = v & 1;
        let r = (v >> 1) | (c << 7); // bit 0 wraps into bit 7
        self.set_shift_flags(r, c);
        r
    }
    fn cb_rl(&mut self, v: u8) -> u8 {
        let old = self.flag(FLAG_C) as u8;
        let c = v >> 7;
        let r = (v << 1) | old; // old carry shifts into bit 0
        self.set_shift_flags(r, c);
        r
    }
    fn cb_rr(&mut self, v: u8) -> u8 {
        let old = self.flag(FLAG_C) as u8;
        let c = v & 1;
        let r = (v >> 1) | (old << 7); // old carry shifts into bit 7
        self.set_shift_flags(r, c);
        r
    }
    fn cb_sla(&mut self, v: u8) -> u8 {
        let c = v >> 7;
        let r = v << 1; // shift in a 0
        self.set_shift_flags(r, c);
        r
    }
    fn cb_sra(&mut self, v: u8) -> u8 {
        let c = v & 1;
        let r = (v >> 1) | (v & 0x80); // arithmetic: bit 7 is preserved (sign)
        self.set_shift_flags(r, c);
        r
    }
    fn cb_srl(&mut self, v: u8) -> u8 {
        let c = v & 1;
        let r = v >> 1; // logical: bit 7 becomes 0
        self.set_shift_flags(r, c);
        r
    }
    fn cb_swap(&mut self, v: u8) -> u8 {
        let r = (v >> 4) | (v << 4); // swap nibbles
        self.set_flag(FLAG_Z, r == 0);
        self.set_flag(FLAG_N, false);
        self.set_flag(FLAG_H, false);
        self.set_flag(FLAG_C, false); // SWAP always clears carry
        r
    }

    // ===== Decode + execute ====================================================
    // The full legal SM83 opcode set. The structure mirrors the opcode map's own layout:
    //   0x00-0x3F  misc, 16-bit loads, INC/DEC, the A-rotates, JR
    //   0x40-0x7F  LD r,r' (one range arm; 0x76 is HALT, carved out first)
    //   0x80-0xBF  ALU A,r (one range arm; the sub-op is bits 5-3)
    //   0xC0-0xFF  control flow, stack, immediate ALU, LDH, RST, CB prefix
    // Conditional control flow and the (HL) operand cost extra cycles; those arms return
    // the right count per branch. Illegal opcodes (0xD3, 0xDB, 0xDD, ...) fall through to
    // the catch-all, which now signals a runaway PC rather than "not yet implemented".
    fn dispatch(&mut self, op: u8) -> u8 {
        match op {
            0x00 => 4, // NOP
            0x10 => {
                // STOP — a 2-byte opcode (0x10 0x00) for low-power / CGB speed switch.
                // Out of scope here (no double-speed), so consume the pad byte and idle.
                let _ = self.fetch8();
                4
            }

            // --- 16-bit immediate loads: LD rr,nn ---
            0x01 => { let v = self.fetch16(); self.set_bc(v); 12 }
            0x11 => { let v = self.fetch16(); self.set_de(v); 12 }
            0x21 => { let v = self.fetch16(); self.set_hl(v); 12 }
            0x31 => { self.sp = self.fetch16(); 12 }

            // --- store A into memory at a 16-bit pointer (HL auto-adjusts on +/-) ---
            0x02 => { let a = self.bc(); self.bus.write(a, self.a); 8 }
            0x12 => { let a = self.de(); self.bus.write(a, self.a); 8 }
            0x22 => { let a = self.hl(); self.bus.write(a, self.a); self.set_hl(a.wrapping_add(1)); 8 }
            0x32 => { let a = self.hl(); self.bus.write(a, self.a); self.set_hl(a.wrapping_sub(1)); 8 }

            // --- load A from memory at a 16-bit pointer ---
            0x0A => { let a = self.bc(); self.a = self.bus.read(a); 8 }
            0x1A => { let a = self.de(); self.a = self.bus.read(a); 8 }
            0x2A => { let a = self.hl(); self.a = self.bus.read(a); self.set_hl(a.wrapping_add(1)); 8 }
            0x3A => { let a = self.hl(); self.a = self.bus.read(a); self.set_hl(a.wrapping_sub(1)); 8 }

            // --- LD (nn),SP : store SP to memory, little-endian ---
            0x08 => {
                let addr = self.fetch16();
                self.bus.write(addr, self.sp as u8);
                self.bus.write(addr.wrapping_add(1), (self.sp >> 8) as u8);
                20
            }

            // --- 8-bit immediate loads: LD r,n  (and LD (HL),n) ---
            0x06 => { self.b = self.fetch8(); 8 }
            0x0E => { self.c = self.fetch8(); 8 }
            0x16 => { self.d = self.fetch8(); 8 }
            0x1E => { self.e = self.fetch8(); 8 }
            0x26 => { self.h = self.fetch8(); 8 }
            0x2E => { self.l = self.fetch8(); 8 }
            0x36 => { let a = self.hl(); let v = self.fetch8(); self.bus.write(a, v); 12 }
            0x3E => { self.a = self.fetch8(); 8 }

            // --- 16-bit INC/DEC: no flags touched ---
            0x03 => { let v = self.bc().wrapping_add(1); self.set_bc(v); 8 }
            0x13 => { let v = self.de().wrapping_add(1); self.set_de(v); 8 }
            0x23 => { let v = self.hl().wrapping_add(1); self.set_hl(v); 8 }
            0x33 => { self.sp = self.sp.wrapping_add(1); 8 }
            0x0B => { let v = self.bc().wrapping_sub(1); self.set_bc(v); 8 }
            0x1B => { let v = self.de().wrapping_sub(1); self.set_de(v); 8 }
            0x2B => { let v = self.hl().wrapping_sub(1); self.set_hl(v); 8 }
            0x3B => { self.sp = self.sp.wrapping_sub(1); 8 }

            // --- ADD HL,rr ---
            0x09 => { self.add_hl(self.bc()); 8 }
            0x19 => { self.add_hl(self.de()); 8 }
            0x29 => { self.add_hl(self.hl()); 8 }
            0x39 => { self.add_hl(self.sp); 8 }

            // --- 8-bit INC r / DEC r (covers (HL) at idx 6, which costs 12) ---
            0x04 | 0x0C | 0x14 | 0x1C | 0x24 | 0x2C | 0x34 | 0x3C => {
                let idx = (op >> 3) & 7;
                let v = self.reg(idx);
                let r = self.alu_inc(v);
                self.set_reg(idx, r);
                if idx == 6 { 12 } else { 4 }
            }
            0x05 | 0x0D | 0x15 | 0x1D | 0x25 | 0x2D | 0x35 | 0x3D => {
                let idx = (op >> 3) & 7;
                let v = self.reg(idx);
                let r = self.alu_dec(v);
                self.set_reg(idx, r);
                if idx == 6 { 12 } else { 4 }
            }

            // --- the A-rotates: like the CB rotates but Z is FORCED to 0 ---
            0x07 => { let r = self.cb_rlc(self.a); self.a = r; self.set_flag(FLAG_Z, false); 4 } // RLCA
            0x0F => { let r = self.cb_rrc(self.a); self.a = r; self.set_flag(FLAG_Z, false); 4 } // RRCA
            0x17 => { let r = self.cb_rl(self.a);  self.a = r; self.set_flag(FLAG_Z, false); 4 } // RLA
            0x1F => { let r = self.cb_rr(self.a);  self.a = r; self.set_flag(FLAG_Z, false); 4 } // RRA

            // --- accumulator / flag misc ---
            0x27 => { self.daa(); 4 } // DAA
            0x2F => { self.a = !self.a; self.set_flag(FLAG_N, true); self.set_flag(FLAG_H, true); 4 } // CPL
            0x37 => { self.set_flag(FLAG_N, false); self.set_flag(FLAG_H, false); self.set_flag(FLAG_C, true); 4 } // SCF
            0x3F => { let c = self.flag(FLAG_C); self.set_flag(FLAG_N, false); self.set_flag(FLAG_H, false); self.set_flag(FLAG_C, !c); 4 } // CCF

            // --- relative jumps: JR e (always) and JR cc,e (taken=12 / not=8) ---
            0x18 => { let e = self.fetch8() as i8; self.pc = self.pc.wrapping_add(e as u16); 12 }
            0x20 | 0x28 | 0x30 | 0x38 => {
                let e = self.fetch8() as i8; // operand is consumed whether or not we branch
                if self.cond((op >> 3) & 3) {
                    self.pc = self.pc.wrapping_add(e as u16);
                    12
                } else {
                    8
                }
            }

            // HALT (0x76) must be matched before the LD r,r' block it sits inside.
            0x76 => { self.halt(); 4 }

            // --- LD r,r' : one arm for the whole 0x40-0x7F block ---
            0x40..=0x7F => {
                let dst = (op >> 3) & 7;
                let src = op & 7;
                let val = self.reg(src);
                self.set_reg(dst, val);
                if dst == 6 || src == 6 { 8 } else { 4 } // (HL) adds a memory cycle
            }

            // --- ALU A,r : one arm for the whole 0x80-0xBF block; sub-op is bits 5-3 ---
            0x80..=0xBF => {
                let val = self.reg(op & 7);
                match (op >> 3) & 7 {
                    0 => self.alu_add(val),
                    1 => self.alu_adc(val),
                    2 => self.alu_sub(val),
                    3 => self.alu_sbc(val),
                    4 => self.alu_and(val),
                    5 => self.alu_xor(val),
                    6 => self.alu_or(val),
                    _ => self.alu_cp(val),
                }
                if (op & 7) == 6 { 8 } else { 4 }
            }

            // --- ALU A,n : immediate forms reuse the same primitives ---
            0xC6 => { let n = self.fetch8(); self.alu_add(n); 8 }
            0xCE => { let n = self.fetch8(); self.alu_adc(n); 8 }
            0xD6 => { let n = self.fetch8(); self.alu_sub(n); 8 }
            0xDE => { let n = self.fetch8(); self.alu_sbc(n); 8 }
            0xE6 => { let n = self.fetch8(); self.alu_and(n); 8 }
            0xEE => { let n = self.fetch8(); self.alu_xor(n); 8 }
            0xF6 => { let n = self.fetch8(); self.alu_or(n); 8 }
            0xFE => { let n = self.fetch8(); self.alu_cp(n); 8 }

            // --- POP / PUSH (POP AF masks F's low nibble via set_af) ---
            0xC1 => { let v = self.pop16(); self.set_bc(v); 12 }
            0xD1 => { let v = self.pop16(); self.set_de(v); 12 }
            0xE1 => { let v = self.pop16(); self.set_hl(v); 12 }
            0xF1 => { let v = self.pop16(); self.set_af(v); 12 }
            0xC5 => { self.push16(self.bc()); 16 }
            0xD5 => { self.push16(self.de()); 16 }
            0xE5 => { self.push16(self.hl()); 16 }
            0xF5 => { self.push16(self.af()); 16 }

            // --- absolute jumps: JP nn, JP cc,nn (taken=16 / not=12), JP (HL) ---
            0xC3 => { let addr = self.fetch16(); self.pc = addr; 16 }
            0xC2 | 0xCA | 0xD2 | 0xDA => {
                let addr = self.fetch16();
                if self.cond((op >> 3) & 3) { self.pc = addr; 16 } else { 12 }
            }
            0xE9 => { self.pc = self.hl(); 4 } // JP (HL): despite the parens, it's pc=HL, no memory read

            // --- calls and returns ---
            0xCD => { let addr = self.fetch16(); self.push16(self.pc); self.pc = addr; 24 }
            0xC4 | 0xCC | 0xD4 | 0xDC => {
                let addr = self.fetch16();
                if self.cond((op >> 3) & 3) { self.push16(self.pc); self.pc = addr; 24 } else { 12 }
            }
            0xC9 => { self.pc = self.pop16(); 16 } // RET
            0xC0 | 0xC8 | 0xD0 | 0xD8 => {
                if self.cond((op >> 3) & 3) { self.pc = self.pop16(); 20 } else { 8 }
            }
            0xD9 => { self.pc = self.pop16(); self.ime = true; 16 } // RETI — IME on IMMEDIATELY (no EI delay)

            // --- RST n : push PC, jump to a fixed low vector (op & 0x38) ---
            0xC7 | 0xCF | 0xD7 | 0xDF | 0xE7 | 0xEF | 0xF7 | 0xFF => {
                self.push16(self.pc);
                self.pc = (op & 0x38) as u16;
                16
            }

            // --- high-page (0xFF00+offset) and absolute accumulator loads ---
            0xE0 => { let n = self.fetch8(); self.bus.write(0xFF00 + n as u16, self.a); 12 } // LDH (n),A
            0xF0 => { let n = self.fetch8(); self.a = self.bus.read(0xFF00 + n as u16); 12 } // LDH A,(n)
            0xE2 => { self.bus.write(0xFF00 + self.c as u16, self.a); 8 } // LD (C),A
            0xF2 => { self.a = self.bus.read(0xFF00 + self.c as u16); 8 } // LD A,(C)
            0xEA => { let addr = self.fetch16(); self.bus.write(addr, self.a); 16 } // LD (nn),A
            0xFA => { let addr = self.fetch16(); self.a = self.bus.read(addr); 16 } // LD A,(nn)

            // --- SP arithmetic / transfer ---
            0xE8 => { let e = self.fetch8() as i8; self.sp = self.add_sp_e(e); 16 } // ADD SP,e
            0xF8 => { let e = self.fetch8() as i8; let v = self.add_sp_e(e); self.set_hl(v); 12 } // LD HL,SP+e
            0xF9 => { self.sp = self.hl(); 8 } // LD SP,HL

            // --- interrupt master enable flags ---
            0xF3 => { self.ime = false; self.ime_pending = false; 4 } // DI (also cancels a pending EI)
            0xFB => { self.ime_pending = true; 4 } // EI — enable AFTER the next instruction

            // --- the 0xCB prefix opens the bit/rotate/shift page ---
            0xCB => { let cb = self.fetch8(); self.dispatch_cb(cb) }

            other => panic!(
                "illegal/unimplemented opcode 0x{:02X} at PC 0x{:04X} — the full legal SM83 \
                 set is implemented, so this is almost certainly a runaway PC (a control-flow \
                 or stack bug jumped into data)",
                other,
                self.pc.wrapping_sub(1)
            ),
        }
    }

    /// 0xCB-prefixed opcodes: a regular grid decoded by bit-fields rather than a giant
    /// table. Bits 7-6 pick the family, bits 5-3 the bit-index / shift-op, bits 2-0 the
    /// operand register (same 0=B..6=(HL)..7=A encoding as everywhere else).
    fn dispatch_cb(&mut self, cb: u8) -> u8 {
        let idx = cb & 7; // operand register
        let bit = (cb >> 3) & 7; // bit index (BIT/RES/SET) or rotate/shift selector
        let val = self.reg(idx);
        let is_hl = idx == 6;

        match cb >> 6 {
            // 0b00 — rotate / shift / SWAP, chosen by `bit`.
            0 => {
                let res = match bit {
                    0 => self.cb_rlc(val),
                    1 => self.cb_rrc(val),
                    2 => self.cb_rl(val),
                    3 => self.cb_rr(val),
                    4 => self.cb_sla(val),
                    5 => self.cb_sra(val),
                    6 => self.cb_swap(val),
                    _ => self.cb_srl(val),
                };
                self.set_reg(idx, res);
                if is_hl { 16 } else { 8 }
            }
            // 0b01 — BIT b,r : test bit -> Z, N=0, H=1, C untouched. No write-back, and the
            // (HL) form is only 12 cycles (it reads but never writes).
            1 => {
                self.set_flag(FLAG_Z, val & (1 << bit) == 0);
                self.set_flag(FLAG_N, false);
                self.set_flag(FLAG_H, true);
                if is_hl { 12 } else { 8 }
            }
            // 0b10 — RES b,r : clear a bit. No flags.
            2 => {
                self.set_reg(idx, val & !(1 << bit));
                if is_hl { 16 } else { 8 }
            }
            // 0b11 — SET b,r : set a bit. No flags.
            _ => {
                self.set_reg(idx, val | (1 << bit));
                if is_hl { 16 } else { 8 }
            }
        }
    }
}

// ===== Half-carry primitives (the #1 flag time-sink — implement once, reuse) =====
//
// The half-carry flag (H) reports a carry/borrow across the bit-3 / bit-4 boundary.
// For an 8-bit ADD: add only the low nibbles and see if the sum spills past 0x0F.
fn hc_add(a: u8, b: u8) -> bool {
    (a & 0x0F) + (b & 0x0F) > 0x0F
}
// For an 8-bit SUB it's a borrow: the low nibble of a is smaller than that of b.
// (ADC/SBC fold the incoming carry into the low-nibble math directly in alu_adc/alu_sbc.)
fn hc_sub(a: u8, b: u8) -> bool {
    (a & 0x0F) < (b & 0x0F)
}
