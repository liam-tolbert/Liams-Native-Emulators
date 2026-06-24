//! The MIPS R3000A CPU core — the interpreter.
//!
//! `Cpu::step()` runs exactly one instruction and returns how many CPU cycles it took, then
//! ticks the bus by that much (the "catch-up" timing seam, identical in shape to the Game Boy
//! core). Three things about MIPS need careful handling and don't exist on the Game Boy — each
//! is the source of a whole class of bugs, so each is commented heavily at the spot it bites:
//!
//!  * **Branch delay slot.** On MIPS the instruction *physically after* a branch or jump always
//!    executes, even when the branch is taken. The hardware has already fetched it before it
//!    knows the branch outcome. We model this with two program counters one step apart
//!    (`pc` / `next_pc`): a branch never changes `pc` (that slot is committed to run); it only
//!    rewrites `next_pc`.
//!
//!  * **Load delay slot.** The R3000 has no "load interlock": the value a load pulls from memory
//!    is *not* yet in the register for the single instruction that follows the load. We model
//!    this by parking the loaded value in a one-entry queue (`load`) and only writing it into
//!    the register file at the start of the *next* instruction. This is the PS1's equivalent of
//!    the DMG half-carry — the bug you get wrong first.
//!
//!  * **COP0 exceptions.** Overflow, syscall, break, address errors and interrupts all stop the
//!    current instruction and vector to a BIOS handler. The bookkeeping lives in `Cop0`; this
//!    file decides *when* to raise one.

use crate::bus::Bus;
use crate::cop0::{Cop0, Exception};

pub struct Cpu {
    /// General-purpose registers as seen by the *currently executing* instruction (the read
    /// side). `regs[0]` is hardwired to 0 (reads 0, writes are discarded).
    ///
    /// The split into `regs` (read) and `out_regs` (write) is the trick that implements the
    /// load-delay slot. An instruction reads its operands from `regs`, but every write lands in
    /// `out_regs`; only at the very end of the instruction is `out_regs` copied back into
    /// `regs`. Because a load parks its result in `load` (below) rather than writing a register
    /// directly, the loaded value becomes visible exactly one instruction late — like the real
    /// hardware.
    pub regs: [u32; 32],
    pub out_regs: [u32; 32],

    /// The load-delay queue: `(destination register, value)` produced by a load (or `MFC0`) that
    /// must be written into the register file at the start of the *next* instruction. `(0, _)`
    /// means "nothing pending" (register 0 is the bit bucket, so writing it is a harmless no-op).
    load: (usize, u32),

    pub pc: u32,         // address of the instruction to fetch next
    pub next_pc: u32,    // the one after that — rewritten by branches (the delay-slot model)
    pub current_pc: u32, // address of the instruction currently executing (saved for EPC)

    /// Set true by a branch/jump while it executes, so the *following* step knows it is running
    /// in a delay slot. Needed because an exception taken in a delay slot must resume at the
    /// branch, not the slot.
    branch: bool,
    /// True while the instruction running *this* step sits in a branch's delay slot.
    delay_slot: bool,

    pub hi: u32, // high half of a multiply / remainder of a divide
    pub lo: u32, // low half of a multiply / quotient of a divide

    /// Cycles consumed by the instruction running this step. Most are a flat cost; multiply and
    /// divide take many more. Timing isn't load-bearing yet (nothing time-based runs yet),
    /// but the seam is here so it can tighten later without touching callers.
    cycles: u32,

    pub cop0: Cop0, // COP0 is part of the CPU, so the CPU owns it (unlike the DMG's bus-owned IRQs)

    /// When true, snoop BIOS `std_out_putchar` calls and copy the character into the captured TTY
    /// stream — the hook that lets the headless TTY harness read whatever a booting BIOS or a
    /// sideloaded test program prints. Left off for the `selftest` (whose tiny hand-written programs
    /// never call the BIOS), so the tap costs nothing there. See the snoop in `step()`.
    pub capture_tty: bool,

    /// Total hardware interrupts *taken* (exceptions vectored for an enabled IRQ) since reset. The
    /// `disc` watchdog uses this as a liveness signal: a machine still taking VBlank interrupts is
    /// alive and merely *waiting* (e.g. a game's multi-second frame-counter delay), not hung — so it
    /// must not be misreported as a stall. Cheap (one add per interrupt).
    pub irq_taken: u64,

    /// The CPU owns the bus; every memory access goes through it.
    pub bus: Bus,
}

impl Cpu {
    /// Construct at the R3000 reset state. The CPU comes up executing the BIOS at the reset
    /// vector 0xBFC00000 (KSEG1 — the uncached window, so it runs before the cache is set up).
    pub fn new(bus: Bus) -> Self {
        Self {
            regs: [0; 32],
            out_regs: [0; 32],
            load: (0, 0),
            pc: 0xBFC0_0000,
            next_pc: 0xBFC0_0004,
            current_pc: 0xBFC0_0000,
            branch: false,
            delay_slot: false,
            hi: 0,
            lo: 0,
            cycles: 0,
            cop0: Cop0::new(),
            capture_tty: false,
            irq_taken: 0,
            bus,
        }
    }

    // ===== register access ==============================================================
    // Reads come from `regs`, writes go to `out_regs`; see the struct comment for why.

    /// Read a general-purpose register.
    #[inline]
    fn reg(&self, idx: usize) -> u32 {
        self.regs[idx]
    }

    /// Write a general-purpose register (into the write side). Register 0 is hardwired to zero,
    /// so we force `out_regs[0]` back to 0 after every write rather than branch on `idx == 0`.
    #[inline]
    fn set_reg(&mut self, idx: usize, val: u32) {
        self.out_regs[idx] = val;
        self.out_regs[0] = 0;
    }

    // ===== the fetch/execute loop =======================================================

    /// Execute one instruction, return the cycles it consumed, and tick the bus by that much.
    pub fn step(&mut self) -> u32 {
        self.cycles = 2; // base cost; multiply/divide raise it in their arms

        // Remember where we are (the handler needs it for EPC) and whether this instruction is
        // a delay slot — that's true exactly when the instruction just before us branched.
        self.current_pc = self.pc;
        self.delay_slot = self.branch;
        self.branch = false; // the instruction we run now may set it again

        // ---- BIOS TTY tap ---------------------------------------------------------------
        // How does a PS1 program print text? There's no console by default, but the BIOS provides a
        // "kernel TTY" (a debug serial terminal) plus a putchar routine to write to it. The catch:
        // the PS1 kernel does NOT expose its functions as numbered SYSCALLs. Instead it publishes
        // three jump tables — "A", "B", "C" — at the *fixed* addresses 0xA0, 0xB0, 0xC0. To call
        // function N of table B you put N in register $t1 (r9) and `jal 0xB0`; a stub there indexes
        // the table and jumps to the real routine. Arguments use the normal MIPS convention, so the
        // first one sits in $a0 (r4).
        //
        // Those fixed addresses are a gift to a headless emulator: to capture everything the machine
        // prints we needn't emulate any serial hardware — we just watch for the CPU about to execute
        // a vector entry and read the arguments straight off the registers. `std_out_putchar` is
        // function 0x3C of table A (0xA0 with $t1 == 0x3C) and 0x3D of table B (0xB0 with $t1 ==
        // 0x3D); the character to print is the low byte of $a0. This is the same idea the Game Boy
        // build used to scrape Blargg's pass/fail text off the serial port.
        //
        // We only *observe* — control flow is untouched — so the real BIOS routine still runs
        // underneath (it writes the byte to the stubbed debug port, harmlessly). We mask the address
        // to its physical form (`& 0x1FFF_FFFF`) so the tap fires regardless of which KUSEG/KSEG0/
        // KSEG1 mirror window the caller jumped through (they all alias the same low addresses).
        if self.capture_tty {
            match (self.current_pc & 0x1FFF_FFFF, self.regs[9]) {
                (0xA0, 0x3C) | (0xB0, 0x3D) => self.bus.tty_push(self.regs[4] as u8),
                _ => {}
            }
        }

        // The IRQ controller aggregates every interrupt source onto one line; mirror that line
        // into COP0's Cause.IP2 so the pending-interrupt test below can see it.
        //
        // Note the TWO independent masks an interrupt must clear, and that they live in different
        // places on purpose: `Irq::pending()` ANDs I_STAT with I_MASK (the *controller* decides
        // which sources are allowed to pull the line at all), and `interrupt_pending()` then ANDs
        // Cause.IP against SR.IM (the *CPU* decides whether it is listening to line IP2). A real
        // interrupt has to survive both. (Early on there was no caller for `Irq::raise`, so this
        // line stayed low; the self-test now drives the whole chain end-to-end.)
        self.cop0.set_hw_irq(self.bus.irq.pending());

        if self.cop0.irq_enabled() && self.cop0.interrupt_pending() {
            self.irq_taken = self.irq_taken.wrapping_add(1); // liveness signal for the disc watchdog
            // An interrupt is recognised *between* instructions: we take it now and do NOT run
            // the instruction we were about to fetch — it resumes later via EPC.
            self.exception(Exception::Interrupt);
        } else if self.current_pc & 3 != 0 {
            // Instructions are 4 bytes, so the PC must be word-aligned. A misaligned PC (e.g.
            // after a jump to a bad address) faults on the fetch itself.
            self.cop0.bad_vaddr = self.current_pc;
            self.exception(Exception::AddrErrLoad);
        } else {
            let instr = self.bus.read32(self.current_pc);

            // Advance the two-PC delay-slot model: the instruction we'll run after this one is
            // whatever `next_pc` currently points at. A branch executed below will overwrite
            // `next_pc` (the slot after it — i.e. the instruction we just queued — still runs).
            self.pc = self.next_pc;
            self.next_pc = self.next_pc.wrapping_add(4);

            // Commit the previous instruction's pending load now — *after* this instruction will
            // read its operands from `regs`, *before* we run it. That ordering is exactly what
            // makes a loaded value invisible to the one instruction following the load.
            let (lreg, lval) = self.load;
            self.set_reg(lreg, lval);
            self.load = (0, 0);

            self.execute(instr);

            // Publish this instruction's writes so the next instruction can read them.
            self.regs = self.out_regs;
        }

        self.bus.tick(self.cycles);
        self.cycles
    }

    // ===== exceptions ===================================================================

    /// Raise an exception: hand off to COP0 (which records cause/EPC and picks the handler
    /// address) and redirect execution there.
    fn exception(&mut self, exc: Exception) {
        let handler = self
            .cop0
            .enter_exception(exc, self.current_pc, self.delay_slot);
        self.pc = handler;
        self.next_pc = handler.wrapping_add(4);
        // The jump to the handler is not a delayed branch, so the handler's first instruction
        // must not be treated as a delay slot.
        self.branch = false;
    }

    /// A COPn / LWCn / SWCn op for coprocessor `n`. If software has marked the coprocessor usable
    /// (its SR.CU bit, or kernel mode for COP0), we accept it — there's nothing to do yet for the
    /// absent COP1/COP3, and the GTE's (COP2) data moves arrive later — otherwise it raises
    /// Coprocessor Unusable. Gating on the usable bit, rather than always faulting, is what makes
    /// cpu/cop's "enabled" cases pass.
    fn cop_op(&mut self, n: u32) {
        // These are gated purely by the SR.CU "usable" bit. Unlike the COP0 *control* ops
        // (MFC0/MTC0/RFE in the 0x10 arm), the load/store-coprocessor forms do NOT inherit COP0's
        // kernel-mode exemption — cpu/cop's testSwc0Disabled shows SWC0 faulting in kernel mode when
        // CU0 is clear, even though a kernel-mode MFC0 right next to it would not. When the bit is
        // set there's nothing to do yet (no FPU; the GTE's COP2 moves arrive later); when it's clear
        // it's Coprocessor Unusable.
        if self.cop0.sr & (1 << (28 + n)) == 0 {
            self.cop_unusable(n);
        }
    }

    /// Raise a Coprocessor Unusable exception, first recording which coprocessor (0..3) was at
    /// fault in Cause.CE so the kernel's handler can tell which one was poked.
    fn cop_unusable(&mut self, n: u32) {
        self.cop0.set_coprocessor_error(n);
        self.exception(Exception::CoprocessorUnusable);
    }

    // ===== stores (gated by cache isolation) ============================================
    // While SR's cache-isolate bit is set, the hardware routes stores into the CPU data cache
    // instead of RAM. We don't model that cache, so the correct behaviour for an isolated store
    // is to drop it. The BIOS switches isolation on to scrub the cache early in boot; if we let
    // those writes reach RAM we would corrupt memory the BIOS is about to depend on.

    #[inline]
    fn store8(&mut self, addr: u32, val: u8) {
        if self.cop0.cache_isolated() {
            return;
        }
        self.bus.write8(addr, val);
    }
    #[inline]
    fn store16(&mut self, addr: u32, val: u16) {
        if self.cop0.cache_isolated() {
            return;
        }
        self.bus.write16(addr, val);
    }
    #[inline]
    fn store32(&mut self, addr: u32, val: u32) {
        if self.cop0.cache_isolated() {
            return;
        }
        self.bus.write32(addr, val);
    }

    // ===== branch helper ================================================================

    /// Take a relative branch. The 16-bit immediate is an offset in *instructions* (words), so
    /// it is shifted left by 2 to become a byte offset, and it is measured from the delay slot.
    /// At this point `self.pc` already points at the delay slot (we advanced it before executing),
    /// so the target is simply `pc + offset`.
    #[inline]
    fn branch(&mut self, offset_se: u32) {
        self.next_pc = self.pc.wrapping_add(offset_se << 2);
        self.branch = true;
    }

    // ===== decode + execute =============================================================

    /// Decode one 32-bit instruction word and run it. The primary opcode (top 6 bits) selects
    /// the broad family; `SPECIAL` (0) fans out on the low 6 bits (`funct`), `REGIMM` (1) on the
    /// `rt` field, and `COP0` (0x10) on the `rs` field — a direct `match`, no jump table, the
    /// same approach as the Game Boy core.
    fn execute(&mut self, instr: u32) {
        // ---- instruction field layout (fixed across all of MIPS I) ----
        let op = instr >> 26; //                  bits 31..26  primary opcode
        let rs = ((instr >> 21) & 0x1F) as usize; // bits 25..21  first source register
        let rt = ((instr >> 16) & 0x1F) as usize; // bits 20..16  second source / dest register
        let rd = ((instr >> 11) & 0x1F) as usize; // bits 15..11  destination (R-type)
        let shamt = (instr >> 6) & 0x1F; //          bits 10..6   shift amount
        let funct = instr & 0x3F; //                 bits 5..0    R-type function
        let imm16 = instr & 0xFFFF; //               the immediate, zero-extended (logical ops)
        let imm_se = imm16 as i16 as u32; //         the immediate, sign-extended (arithmetic/addressing)
        let imm26 = instr & 0x03FF_FFFF; //          the 26-bit jump target field

        match op {
            // ---- SPECIAL: R-type, decoded by the funct field ----
            0x00 => match funct {
                // Shifts. SLL r0,r0,0 (the all-zero word) is the canonical NOP.
                0x00 => self.set_reg(rd, self.reg(rt) << shamt), // SLL  — shift left logical
                0x02 => self.set_reg(rd, self.reg(rt) >> shamt), // SRL  — shift right logical (zero-fill)
                0x03 => self.set_reg(rd, (self.reg(rt) as i32 >> shamt) as u32), // SRA — arithmetic (sign-fill)
                // Variable shifts take their amount from the low 5 bits of rs.
                0x04 => self.set_reg(rd, self.reg(rt) << (self.reg(rs) & 0x1F)), // SLLV
                0x06 => self.set_reg(rd, self.reg(rt) >> (self.reg(rs) & 0x1F)), // SRLV
                0x07 => self.set_reg(rd, (self.reg(rt) as i32 >> (self.reg(rs) & 0x1F)) as u32), // SRAV

                // Register jumps. The branch-delay slot runs before control actually transfers.
                0x08 => {
                    // JR — jump to the address in rs.
                    self.next_pc = self.reg(rs);
                    self.branch = true;
                }
                0x09 => {
                    // JALR — jump to rs, leaving the return address (the instruction after the
                    // delay slot) in rd. `next_pc` is that return address right now, so capture
                    // it before overwriting it.
                    self.set_reg(rd, self.next_pc);
                    self.next_pc = self.reg(rs);
                    self.branch = true;
                }

                0x0C => self.exception(Exception::Syscall), // SYSCALL — deliberate trap into the kernel
                0x0D => self.exception(Exception::Break),   // BREAK   — debugger breakpoint trap

                // hi/lo moves (the multiply/divide result registers).
                0x10 => self.set_reg(rd, self.hi), // MFHI
                0x11 => self.hi = self.reg(rs),    // MTHI
                0x12 => self.set_reg(rd, self.lo), // MFLO
                0x13 => self.lo = self.reg(rs),    // MTLO

                0x18 => {
                    // MULT — signed 32×32 → 64-bit product, split across hi:lo.
                    let p = (self.reg(rs) as i32 as i64) * (self.reg(rt) as i32 as i64);
                    self.hi = (p >> 32) as u32;
                    self.lo = p as u32;
                    self.cycles = 6;
                }
                0x19 => {
                    // MULTU — unsigned 32×32 → 64-bit product.
                    let p = (self.reg(rs) as u64) * (self.reg(rt) as u64);
                    self.hi = (p >> 32) as u32;
                    self.lo = p as u32;
                    self.cycles = 6;
                }
                0x1A => {
                    // DIV — signed divide; quotient → lo, remainder → hi. The R3000 never faults
                    // on divide-by-zero (or on the lone signed-overflow case); it returns the
                    // documented sentinel values below, and software is expected to pre-check.
                    let n = self.reg(rs) as i32;
                    let d = self.reg(rt) as i32;
                    if d == 0 {
                        // quotient is all-ones or 1 depending on the sign of the dividend.
                        self.lo = if n >= 0 { 0xFFFF_FFFF } else { 1 };
                        self.hi = n as u32;
                    } else if n == i32::MIN && d == -1 {
                        // -2^31 / -1 overflows a 32-bit signed result; hardware yields lo = INT_MIN.
                        self.lo = 0x8000_0000;
                        self.hi = 0;
                    } else {
                        self.lo = (n / d) as u32;
                        self.hi = (n % d) as u32;
                    }
                    self.cycles = 36;
                }
                0x1B => {
                    // DIVU — unsigned divide.
                    let n = self.reg(rs);
                    let d = self.reg(rt);
                    if d == 0 {
                        self.lo = 0xFFFF_FFFF;
                        self.hi = n;
                    } else {
                        self.lo = n / d;
                        self.hi = n % d;
                    }
                    self.cycles = 36;
                }

                // Register arithmetic. The non-"U" forms trap on signed overflow and write
                // nothing when they do; the "U" forms wrap silently.
                0x20 => match checked_add(self.reg(rs), self.reg(rt)) {
                    Some(v) => self.set_reg(rd, v), // ADD
                    None => self.exception(Exception::Overflow),
                },
                0x21 => self.set_reg(rd, self.reg(rs).wrapping_add(self.reg(rt))), // ADDU
                0x22 => match checked_sub(self.reg(rs), self.reg(rt)) {
                    Some(v) => self.set_reg(rd, v), // SUB
                    None => self.exception(Exception::Overflow),
                },
                0x23 => self.set_reg(rd, self.reg(rs).wrapping_sub(self.reg(rt))), // SUBU

                // Bitwise logic.
                0x24 => self.set_reg(rd, self.reg(rs) & self.reg(rt)), // AND
                0x25 => self.set_reg(rd, self.reg(rs) | self.reg(rt)), // OR
                0x26 => self.set_reg(rd, self.reg(rs) ^ self.reg(rt)), // XOR
                0x27 => self.set_reg(rd, !(self.reg(rs) | self.reg(rt))), // NOR

                // Set-on-less-than: write 1 or 0. SLT signed, SLTU unsigned.
                0x2A => self.set_reg(rd, ((self.reg(rs) as i32) < (self.reg(rt) as i32)) as u32), // SLT
                0x2B => self.set_reg(rd, (self.reg(rs) < self.reg(rt)) as u32), // SLTU

                _ => self.exception(Exception::ReservedInstr), // unknown funct
            },

            // ---- REGIMM: the rt field picks BLTZ/BGEZ and their "and link" forms ----
            0x01 => {
                // rt bit 0 selects the test (0 → "< 0", 1 → "≥ 0"); the 0x10xx forms also drop
                // the return address into ra (whether or not the branch is taken).
                let is_ge = rt & 1 != 0;
                let link = (rt & 0x1E) == 0x10;
                let v = self.reg(rs) as i32;
                let take = if is_ge { v >= 0 } else { v < 0 };
                if link {
                    self.set_reg(31, self.next_pc); // BLTZAL / BGEZAL link to ra
                }
                if take {
                    self.branch(imm_se);
                }
            }

            // ---- absolute jumps (J-type) ----
            0x02 => {
                // J — replace the low 28 bits of the (delay-slot) PC with target<<2.
                self.next_pc = (self.pc & 0xF000_0000) | (imm26 << 2);
                self.branch = true;
            }
            0x03 => {
                // JAL — same jump, but leave the return address (after the delay slot) in ra.
                self.set_reg(31, self.next_pc);
                self.next_pc = (self.pc & 0xF000_0000) | (imm26 << 2);
                self.branch = true;
            }

            // ---- conditional branches (I-type) ----
            0x04 => {
                if self.reg(rs) == self.reg(rt) {
                    self.branch(imm_se); // BEQ
                }
            }
            0x05 => {
                if self.reg(rs) != self.reg(rt) {
                    self.branch(imm_se); // BNE
                }
            }
            0x06 => {
                if (self.reg(rs) as i32) <= 0 {
                    self.branch(imm_se); // BLEZ
                }
            }
            0x07 => {
                if (self.reg(rs) as i32) > 0 {
                    self.branch(imm_se); // BGTZ
                }
            }

            // ---- immediate arithmetic / logic ----
            0x08 => match checked_add(self.reg(rs), imm_se) {
                Some(v) => self.set_reg(rt, v), // ADDI — sign-extended add, traps on overflow
                None => self.exception(Exception::Overflow),
            },
            0x09 => self.set_reg(rt, self.reg(rs).wrapping_add(imm_se)), // ADDIU — never traps
            0x0A => self.set_reg(rt, ((self.reg(rs) as i32) < (imm_se as i32)) as u32), // SLTI
            0x0B => self.set_reg(rt, (self.reg(rs) < imm_se) as u32), // SLTIU — note: the immediate is
            //   sign-extended *then* compared unsigned (a genuine MIPS quirk).
            0x0C => self.set_reg(rt, self.reg(rs) & imm16), // ANDI — immediate is zero-extended
            0x0D => self.set_reg(rt, self.reg(rs) | imm16), // ORI
            0x0E => self.set_reg(rt, self.reg(rs) ^ imm16), // XORI
            0x0F => self.set_reg(rt, imm16 << 16),          // LUI — load upper immediate

            // ---- COP0: the system control coprocessor ----
            // The usability gate comes first (see `Cop0::cop_usable`): if COP0 isn't usable in the
            // current mode the whole instruction faults before we even decode it. Once it IS usable
            // we decode the move — and crucially, an *unrecognised* COP0 op is a silent NOP, not a
            // fault (real hardware just ignores it; cpu/cop's testCop0InvalidOpcode pins this down).
            0x10 => {
                if !self.cop0.cop_usable(0) {
                    self.cop_unusable(0);
                } else {
                    match rs {
                        0x00 => {
                            // MFC0 rt, rd — move *from* COP0. Like a memory load it has a delay
                            // slot, so it parks its value in `load` rather than writing rt now.
                            self.load = (rt, self.cop0.read(rd));
                        }
                        0x04 => self.cop0.write(rd, self.reg(rt)), // MTC0 — move *to* COP0
                        0x10..=0x1F if funct == 0x10 => self.cop0.return_from_exception(), // RFE
                        _ => {} // CFC0/CTC0 and any other COP0 op: a NOP on the PS1, not a fault
                    }
                }
            }

            // ---- loads (I-type). Each parks its result in the load-delay slot. ----
            0x20 => {
                // LB — load byte, sign-extended.
                let addr = self.reg(rs).wrapping_add(imm_se);
                let v = self.bus.read8(addr) as i8 as u32;
                self.load = (rt, v);
            }
            0x21 => {
                // LH — load half-word, sign-extended; must be 2-byte aligned.
                let addr = self.reg(rs).wrapping_add(imm_se);
                if addr & 1 != 0 {
                    self.cop0.bad_vaddr = addr;
                    self.exception(Exception::AddrErrLoad);
                } else {
                    let v = self.bus.read16(addr) as i16 as u32;
                    self.load = (rt, v);
                }
            }
            0x22 => {
                // LWL — load the *upper* bytes of an unaligned word, merging into rt. LWL/LWR are
                // designed to be used as a pair to read one unaligned word, and uniquely among
                // loads they have NO load-delay between each other: an LWR immediately followed by
                // LWL to the same register must see the LWR's half. We get that for free by reading
                // the merge base from the *write* side (`out_regs`) — the previous instruction's
                // pending load was already promoted there at the top of this step.
                let addr = self.reg(rs).wrapping_add(imm_se);
                let aligned = self.bus.read32(addr & !3);
                let cur = self.out_regs[rt];
                let merged = match addr & 3 {
                    0 => (cur & 0x00FF_FFFF) | (aligned << 24),
                    1 => (cur & 0x0000_FFFF) | (aligned << 16),
                    2 => (cur & 0x0000_00FF) | (aligned << 8),
                    _ => aligned,
                };
                self.load = (rt, merged);
            }
            0x23 => {
                // LW — load word; must be 4-byte aligned.
                let addr = self.reg(rs).wrapping_add(imm_se);
                if addr & 3 != 0 {
                    self.cop0.bad_vaddr = addr;
                    self.exception(Exception::AddrErrLoad);
                } else {
                    self.load = (rt, self.bus.read32(addr));
                }
            }
            0x24 => {
                // LBU — load byte, zero-extended.
                let addr = self.reg(rs).wrapping_add(imm_se);
                self.load = (rt, self.bus.read8(addr) as u32);
            }
            0x25 => {
                // LHU — load half-word, zero-extended; must be 2-byte aligned.
                let addr = self.reg(rs).wrapping_add(imm_se);
                if addr & 1 != 0 {
                    self.cop0.bad_vaddr = addr;
                    self.exception(Exception::AddrErrLoad);
                } else {
                    self.load = (rt, self.bus.read16(addr) as u32);
                }
            }
            0x26 => {
                // LWR — load the *lower* bytes of an unaligned word (the partner of LWL above).
                // Same out_regs read as LWL so the pair composes without a load-delay between them.
                let addr = self.reg(rs).wrapping_add(imm_se);
                let aligned = self.bus.read32(addr & !3);
                let cur = self.out_regs[rt];
                let merged = match addr & 3 {
                    0 => aligned,
                    1 => (cur & 0xFF00_0000) | (aligned >> 8),
                    2 => (cur & 0xFFFF_0000) | (aligned >> 16),
                    _ => (cur & 0xFFFF_FF00) | (aligned >> 24),
                };
                self.load = (rt, merged);
            }

            // ---- stores (I-type) ----
            0x28 => {
                // SB — store byte.
                let addr = self.reg(rs).wrapping_add(imm_se);
                self.store8(addr, self.reg(rt) as u8);
            }
            0x29 => {
                // SH — store half-word; must be 2-byte aligned.
                let addr = self.reg(rs).wrapping_add(imm_se);
                if addr & 1 != 0 {
                    self.cop0.bad_vaddr = addr;
                    self.exception(Exception::AddrErrStore);
                } else {
                    self.store16(addr, self.reg(rt) as u16);
                }
            }
            0x2A => {
                // SWL — store the upper bytes of rt to an unaligned address (read-modify-write
                // the aligned word it lands in).
                let addr = self.reg(rs).wrapping_add(imm_se);
                let aligned_addr = addr & !3;
                let cur = self.bus.read32(aligned_addr);
                let v = self.reg(rt);
                let merged = match addr & 3 {
                    0 => (cur & 0xFFFF_FF00) | (v >> 24),
                    1 => (cur & 0xFFFF_0000) | (v >> 16),
                    2 => (cur & 0xFF00_0000) | (v >> 8),
                    _ => v,
                };
                self.store32(aligned_addr, merged);
            }
            0x2B => {
                // SW — store word; must be 4-byte aligned.
                let addr = self.reg(rs).wrapping_add(imm_se);
                if addr & 3 != 0 {
                    self.cop0.bad_vaddr = addr;
                    self.exception(Exception::AddrErrStore);
                } else {
                    self.store32(addr, self.reg(rt));
                }
            }
            0x2E => {
                // SWR — store the lower bytes of rt to an unaligned address (partner of SWL).
                let addr = self.reg(rs).wrapping_add(imm_se);
                let aligned_addr = addr & !3;
                let cur = self.bus.read32(aligned_addr);
                let v = self.reg(rt);
                let merged = match addr & 3 {
                    0 => v,
                    1 => (cur & 0x0000_00FF) | (v << 8),
                    2 => (cur & 0x0000_FFFF) | (v << 16),
                    _ => (cur & 0x00FF_FFFF) | (v << 24),
                };
                self.store32(aligned_addr, merged);
            }

            // ---- the other coprocessors: COP1/COP2/COP3 and their load/store forms ----
            // The PS1 has no COP1 (FPU) or COP3, and COP2 is the GTE (the geometry engine — arrives
            // later). Whether one of these faults depends ONLY on the matching SR.CUn "usable" bit,
            // not on whether we've implemented it: if software enabled the coprocessor the op is
            // accepted (and, for now, does nothing); if not, it raises Coprocessor Unusable. The low
            // nibble of the primary opcode is the coprocessor number — COPn = 0x10+n, LWCn = 0x30+n,
            // SWCn = 0x38+n — so map each to its number and let `cop_op` apply the gate.
            0x11 => self.cop_op(1),
            0x12 => self.cop_op(2),
            0x13 => self.cop_op(3),
            0x30 => self.cop_op(0), // LWC0
            0x31 => self.cop_op(1), // LWC1
            0x32 => self.cop_op(2), // LWC2 (GTE)
            0x33 => self.cop_op(3), // LWC3
            0x38 => self.cop_op(0), // SWC0
            0x39 => self.cop_op(1), // SWC1
            0x3A => self.cop_op(2), // SWC2 (GTE)
            0x3B => self.cop_op(3), // SWC3

            _ => self.exception(Exception::ReservedInstr), // unknown primary opcode
        }
    }
}

// ===== checked signed arithmetic (the trap-on-overflow ALU rules, one place each) =========
// MIPS overflow is *signed* overflow: the 32-bit values are interpreted as i32, and a result
// that doesn't fit a signed 32-bit number traps. We lean on Rust's own checked i32 arithmetic
// and re-bitcast to u32 — the register file is untyped 32-bit words.

fn checked_add(a: u32, b: u32) -> Option<u32> {
    (a as i32).checked_add(b as i32).map(|v| v as u32)
}
fn checked_sub(a: u32, b: u32) -> Option<u32> {
    (a as i32).checked_sub(b as i32).map(|v| v as u32)
}
