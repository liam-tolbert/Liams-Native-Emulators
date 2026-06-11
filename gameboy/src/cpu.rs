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

    // ===== Decode + execute ====================================================
    //
    // ┌─ PAIRING TASK (M1, Liam) ────────────────────────────────────────────────┐
    // │ Below is the opcode dispatch. I've implemented ONE worked example per       │
    // │ category — copy the pattern to fill in the rest. The catch-all `panic!`     │
    // │ prints the exact unimplemented opcode + PC, so the workflow is:             │
    // │   run `cargo run -- roms\tetris.gb 200`  ->  it panics on opcode 0xNN  ->   │
    // │   implement 0xNN here  ->  rerun. Repeat until it single-steps cleanly.     │
    // │                                                                             │
    // │ Worked examples to mirror:                                                  │
    // │   • LD r,r'   (0x40-0x7F)  -> reg()/set_reg() + the (HL) cycle bump          │
    // │   • ADD A,r   (0x80-0x87)  -> set_flag() + hc_add() for the half-carry       │
    // │   • LD A,n    (0x3E)       -> fetch8()                                       │
    // │   • LD BC,nn  (0x01)       -> fetch16()                                      │
    // │   • JP nn / JR e           -> absolute vs signed-relative control flow       │
    // │ Tools ready for your families: push16()/pop16() (PUSH/POP/CALL/RET/RST),     │
    // │   hc_add()/hc_sub() (ADC/SUB/SBC/CP), set_flag()/flag().                     │
    // │ Condition codes for the cc-variants (JR cc, JP cc, CALL cc, RET cc): the     │
    // │   2-bit field is NZ=0, Z=1, NC=2, C=3.                                       │
    // └─────────────────────────────────────────────────────────────────────────┘
    fn dispatch(&mut self, op: u8) -> u8 {
        match op {
            0x00 => 4, // NOP

            // --- worked: 16-bit immediate load (you add LD DE/HL/SP,nn: 0x11/0x21/0x31) ---
            0x01 => {
                let v = self.fetch16();
                self.set_bc(v);
                12
            }

            0x11 => {
                let v = self.fetch16();
                self.set_de(v);
                12
            }

            0x21 => {
                let v = self.fetch16();
                self.set_hl(v);
                12
            }

            0x31 => {
                self.sp = self.fetch16();
                12
            }


            // --- worked: 8-bit immediate load (you add LD B/C/D/E/H/L/(HL),n) ---
            0x02 => {
                let addr = self.bc();
                self.bus.write(addr, self.a);
                8
            }

            0x12 => {
                let addr = self.de();
                self.bus.write(addr, self.a);
                8
            }

            0x22 => {
                let addr = self.hl();
                self.bus.write(addr, self.a);
                self.set_hl(addr.wrapping_add(1));
                8
            }

            0x32 => {
                let addr = self.hl();
                self.bus.write(addr, self.a);
                self.set_hl(addr.wrapping_sub(1));
                8
            }

            0x06 => {
                self.b = self.fetch8();
                8
            }

            0x16 => {
                self.d = self.fetch8();
                8
            }

            0x26 => {
                self.h = self.fetch8();
                8
            }

            0x36 => {
                let addr = self.hl();
                self.bus.write(addr, self.fetch8());
                12
            }

            0x3E => {
                self.a = self.fetch8();
                8
            }

            0x2E => {
                self.l = self.fetch8();
                8
            }

            0x1E => {
                self.e = self.fetch8();
                8
            }

            0x0E => {
                self.c = self.fetch8();
                8
            }

            // HALT must be matched before the LD r,r' block (0x76 sits inside it).
            0x76 => {
                self.halt();
                4
            }

            // --- worked: LD r, r' (one arm covers the whole 0x40-0x7F block) ---
            0x40..=0x7F => {
                let dst = (op >> 3) & 7;
                let src = op & 7;
                let val = self.reg(src);
                self.set_reg(dst, val);
                if dst == 6 || src == 6 { 8 } else { 4 } // (HL) adds a memory cycle
            }

            // --- worked: ADD A, r (mirror this for ADC/SUB/SBC/AND/XOR/OR/CP) ---
            0x80..=0x87 => {
                let val = self.reg(op & 7);
                let (res, carry) = self.a.overflowing_add(val);
                self.set_flag(FLAG_Z, res == 0);
                self.set_flag(FLAG_N, false);
                self.set_flag(FLAG_H, hc_add(self.a, val));
                self.set_flag(FLAG_C, carry);
                self.a = res;
                if (op & 7) == 6 { 8 } else { 4 }
            }

            0xA8..=0xAF => {
                let val = self.reg(op & 7);
            }

            // --- worked: control flow ---
            0xC3 => {
                // JP nn — unconditional absolute jump.
                let addr = self.fetch16();
                self.pc = addr;
                16
            }
            0x18 => {
                // JR e — relative jump; the operand is a SIGNED offset from the PC
                // *after* the offset byte (which fetch8 has already stepped past).
                let offset = self.fetch8() as i8;
                self.pc = self.pc.wrapping_add(offset as u16);
                12
            }

            // --- system / interrupts ---
            0xF3 => {
                // DI — disable interrupts immediately (and cancel any pending EI).
                self.ime = false;
                self.ime_pending = false;
                4
            }
            0xFB => {
                // EI — enable interrupts AFTER the next instruction (the delay).
                self.ime_pending = true;
                4
            }

            // The 0xCB-prefixed bit/rotate/shift family is M2 (see dispatch_cb).
            0xCB => {
                let cb = self.fetch8();
                self.dispatch_cb(cb)
            }

            other => panic!(
                "unimplemented opcode 0x{:02X} at PC 0x{:04X} — implement it in cpu.rs dispatch()",
                other,
                self.pc.wrapping_sub(1)
            ),
        }
    }

    /// 0xCB-prefixed opcodes (rotate / shift / SWAP / BIT / RES / SET). Implemented in
    /// M2 — it's a very regular 8-registers × operation grid, ideal opcode practice.
    fn dispatch_cb(&mut self, cb: u8) -> u8 {
        panic!(
            "unimplemented CB opcode 0xCB 0x{:02X} at PC 0x{:04X} — the CB family lands in M2",
            cb,
            self.pc.wrapping_sub(2)
        );
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
// (For ADC/SBC, fold the incoming carry into the low-nibble math: `+ carry` / the
//  comparison `(a & 0xF) < (b & 0xF) + carry`.)
fn hc_sub(a: u8, b: u8) -> bool {
    (a & 0x0F) < (b & 0x0F)
}
