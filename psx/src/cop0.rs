//! COP0 — the System Control Coprocessor (the MIPS exception/status unit).
//!
//! On MIPS the "coprocessor 0" is part of the CPU itself, not a bus device — so unlike
//! the Game Boy's interrupt controller (which lived on the bus), `Cop0` is owned directly
//! by `Cpu`. It holds the handful of registers the kernel needs to take and return from
//! exceptions: the status register `SR`, the `CAUSE` of the last exception, the return
//! address `EPC`, and the faulting address `BadVaddr`.
//!
//! The R3000 has no TLB (the PS1 doesn't use virtual memory), so the MMU-related COP0
//! registers don't exist here — this is a much smaller unit than a full MIPS COP0.

/// COP0 register indices (the `rd` field of `MTC0`/`MFC0`).
pub mod reg {
    pub const BPC: usize = 3; // breakpoint PC (debug — stored, not acted on)
    pub const BDA: usize = 5; // breakpoint data address
    pub const JUMPDEST: usize = 6; // last jump destination (read-only-ish)
    pub const DCIC: usize = 7; // breakpoint control
    pub const BAD_VADDR: usize = 8; // bad virtual address (set on an address error)
    pub const BDAM: usize = 9; // data breakpoint mask
    pub const BPCM: usize = 11; // PC breakpoint mask
    pub const SR: usize = 12; // status register
    pub const CAUSE: usize = 13; // cause of the last exception
    pub const EPC: usize = 14; // exception program counter (return address)
    pub const PRID: usize = 15; // processor revision id (read-only)
}

/// The exception codes that land in `CAUSE` bits 6..2 (`ExcCode`). The interpreter raises
/// these; the kernel's handler reads `CAUSE` to find out what happened.
#[derive(Clone, Copy, Debug)]
pub enum Exception {
    Interrupt = 0x00,     // an enabled hardware/software interrupt
    AddrErrLoad = 0x04,   // misaligned (or bad) address on a load / instruction fetch
    AddrErrStore = 0x05,  // misaligned (or bad) address on a store
    Syscall = 0x08,       // the SYSCALL instruction
    Break = 0x09,         // the BREAK instruction
    ReservedInstr = 0x0A, // an illegal / reserved opcode
    CoprocessorUnusable = 0x0B, // a COPx op when that coprocessor is disabled
    Overflow = 0x0C,      // signed overflow in ADD/ADDI/SUB
}

pub struct Cop0 {
    pub sr: u32,        // r12 — status: interrupt-enable stack, BEV, cache-isolate (IsC) ...
    pub cause: u32,     // r13 — ExcCode + the pending-interrupt bits (IP)
    pub epc: u32,       // r14 — where to resume after the handler
    pub bad_vaddr: u32, // r8  — the address that triggered an address-error exception

    // Debug/breakpoint registers. The BIOS pokes some of these; we store them so reads
    // round-trip, but nothing here acts on them (no debugger).
    misc: [u32; 16],
}

impl Cop0 {
    pub fn new() -> Self {
        // Post-reset the kernel runs with BEV=1 (exception vectors in the BIOS ROM at
        // 0xBFC00180) until it relocates them; everything else clear.
        Self {
            sr: 0,
            cause: 0,
            epc: 0,
            bad_vaddr: 0,
            misc: [0; 16],
        }
    }

    // ----- the status-register bits the interpreter cares about -----------------------
    /// IEc — bit 0: the *current* interrupt-enable. Interrupts only fire when this is set.
    pub fn irq_enabled(&self) -> bool {
        self.sr & 1 != 0
    }
    /// IsC — bit 16: "isolate cache". While set, stores hit the (unemulated) scratch cache
    /// instead of RAM. The BIOS sets it to scrub the cache during boot; if we *didn't*
    /// honour it we'd corrupt RAM with those throwaway writes. This is the PS1 analog of a
    /// "writes silently go nowhere" mode — comment it where the store path checks it.
    pub fn cache_isolated(&self) -> bool {
        self.sr & 0x0001_0000 != 0
    }
    /// BEV — bit 22: boot-exception-vectors. Selects the exception entry point.
    pub fn boot_exception_vectors(&self) -> bool {
        self.sr & 0x0040_0000 != 0
    }
    /// Is coprocessor `n` (0..3) usable right now? Each coprocessor has an "enable" flag in SR —
    /// the CU bits, bits 31..28 = CU3..CU0 — and software must set it before using that coprocessor
    /// or the access faults with "coprocessor unusable". COP0 is the exception to the rule: it is
    /// *also* always usable while the CPU is in kernel mode (SR.KUc, bit 1, == 0). That's how the
    /// BIOS runs its exception handlers — which are full of COP0 moves — without ever bothering to
    /// set CU0. Honouring this gate (instead of always faulting) is exactly what the cpu/cop test
    /// ROM checks: an *enabled* coprocessor must NOT throw.
    pub fn cop_usable(&self, n: u32) -> bool {
        let cu_set = self.sr & (1 << (28 + n)) != 0;
        let kernel_mode = self.sr & (1 << 1) == 0;
        cu_set || (n == 0 && kernel_mode)
    }
    /// The interrupt mask (IM, bits 15..8) ANDed against the pending bits (IP) tells us
    /// whether any *enabled* interrupt is waiting.
    pub fn interrupt_pending(&self) -> bool {
        (self.cause & self.sr & 0x0000_FF00) != 0
    }

    /// Set/clear the external-hardware interrupt line, COP0 Cause bit 10 (IP2). The IRQ
    /// controller (a bus device) aggregates every PS1 interrupt source onto this one line.
    pub fn set_hw_irq(&mut self, active: bool) {
        if active {
            self.cause |= 1 << 10;
        } else {
            self.cause &= !(1 << 10);
        }
    }

    /// Record which coprocessor (0..3) triggered a Coprocessor Unusable exception, in Cause.CE (the
    /// "coprocessor error" field, bits 29..28). The kernel's handler reads it to learn which
    /// coprocessor the faulting instruction was for. (`enter_exception` only rewrites the ExcCode
    /// and BD fields, so a CE set here survives into the handler.)
    pub fn set_coprocessor_error(&mut self, n: u32) {
        self.cause = (self.cause & !0x3000_0000) | ((n & 3) << 28);
    }

    // ----- taking and returning from an exception -------------------------------------
    //
    // When the CPU hits something it can't handle inline (a SYSCALL, an overflow, an
    // enabled interrupt...), it "takes an exception": it stops, records *why* and *where*,
    // disables interrupts, and jumps to a fixed handler address in the BIOS. COP0 holds all
    // the bookkeeping for that, so the entry/exit logic lives here.

    /// Take an exception. Records the cause and return address, pushes the interrupt-enable /
    /// mode stack, and returns the **handler address** the CPU should jump to.
    ///
    /// `pc` is the address of the instruction that faulted; `in_delay_slot` is true when that
    /// instruction was sitting in a branch's delay slot (the slot right after a branch, which
    /// always runs — see the CPU core for why).
    pub fn enter_exception(&mut self, exc: Exception, pc: u32, in_delay_slot: bool) -> u32 {
        // SR's low 6 bits are a tiny 3-deep stack of (kernel-mode, interrupt-enable) pairs:
        //   bits 1..0 = "current", bits 3..2 = "previous", bits 5..4 = "old".
        // Taking an exception pushes that stack up by one pair and sets the new "current" pair
        // to 0 — i.e. interrupts off and kernel mode — so the handler runs uninterrupted. The
        // oldest pair (bits 5..4) just falls off the top.
        let stack = self.sr & 0x3F;
        self.sr = (self.sr & !0x3F) | ((stack << 2) & 0x3F);

        // CAUSE.ExcCode (bits 6..2) tells the handler *what* happened; it reads this to decide
        // how to respond. We overwrite just those 5 bits.
        self.cause = (self.cause & !0x0000_007C) | ((exc as u32) << 2);

        // CAUSE.BD (bit 31, "branch delay") records whether the faulting instruction was in a
        // delay slot. If it was, the address we must resume at is the *branch*, not the slot —
        // re-running the branch re-runs the slot too — so EPC points one instruction earlier.
        if in_delay_slot {
            self.cause |= 1 << 31;
            self.epc = pc.wrapping_sub(4);
        } else {
            self.cause &= !(1 << 31);
            self.epc = pc;
        }

        // BEV (SR bit 22) selects which copy of the exception vectors to use. At reset it's 1,
        // pointing at the ROM vectors in the BIOS; the kernel later clears it to use the RAM
        // copies it installs. (General exceptions sit at +0x80 within each vector page.)
        if self.boot_exception_vectors() {
            0xBFC0_0180
        } else {
            0x8000_0080
        }
    }

    /// `RFE` (restore-from-exception) — the tail of a handler. Pops the SR mode stack so the
    /// interrupted code's interrupt-enable / mode bits come back: "previous" -> "current" and
    /// "old" -> "previous". (The hardware leaves the "old" pair as-is.)
    pub fn return_from_exception(&mut self) {
        let stack = self.sr & 0x3F;
        self.sr = (self.sr & !0x0F) | (stack >> 2);
    }

    /// `MFC0 rt, rd` reads a COP0 register.
    pub fn read(&self, rd: usize) -> u32 {
        match rd {
            reg::BAD_VADDR => self.bad_vaddr,
            reg::SR => self.sr,
            reg::CAUSE => self.cause,
            reg::EPC => self.epc,
            // R3000A as fitted to the PS1 reports this revision id.
            reg::PRID => 0x0000_0002,
            _ => self.misc[rd & 0xF],
        }
    }

    /// `MTC0 rt, rd` writes a COP0 register.
    pub fn write(&mut self, rd: usize, val: u32) {
        match rd {
            reg::SR => self.sr = val,
            // Only the two *software* interrupt bits (IP1..0, bits 9..8) of CAUSE are
            // writable; the rest (ExcCode, the hardware IP bits) are set by hardware.
            reg::CAUSE => self.cause = (self.cause & !0x0000_0300) | (val & 0x0000_0300),
            reg::EPC => self.epc = val,
            reg::BAD_VADDR => self.bad_vaddr = val,
            reg::PRID | reg::JUMPDEST => {} // read-only
            _ => self.misc[rd & 0xF] = val,
        }
    }
}
