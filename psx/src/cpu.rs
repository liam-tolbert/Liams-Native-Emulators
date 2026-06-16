//! The MIPS R3000A CPU core.
//!
//! **Scaffold stub — the interpreter is milestone M1.** This file currently establishes the
//! ownership graph and the reset state so the rest of the crate compiles and the host shell
//! can build a machine; the fetch/decode/execute loop, the load-/branch-delay slots, and the
//! opcode dispatch all land in M1.
//!
//! The shape it will grow into mirrors the Game Boy CPU: `step()` runs one instruction and
//! returns the cycles it took, then ticks the bus by that much (the "catch-up" timing seam).
//! The MIPS-specific pieces that don't exist on the DMG and need careful handling in M1:
//!
//!  * **Branch delay slot** — the instruction *after* every branch/jump always runs; modelled
//!    with `pc`/`next_pc`.
//!  * **Load delay slot** — the R3000 has no load interlock, so a loaded register isn't
//!    visible to the *very next* instruction; modelled with a read-side/write-side register
//!    split. This is the #1 PS1 CPU bug source (the analog of the DMG half-carry).
//!  * **COP0 exceptions** — overflow/syscall/break/address-error/interrupt, vectoring through
//!    `Cop0`.

use crate::bus::Bus;
use crate::cop0::Cop0;

pub struct Cpu {
    /// General-purpose registers, read side. `regs[0]` is hardwired to 0 (reads 0, writes
    /// discarded). The split into `regs`/`out_regs` is what implements the load-delay slot
    /// in M1: an instruction reads `regs` but writes `out_regs`, and a load's result is only
    /// promoted into `regs` one instruction later.
    pub regs: [u32; 32],
    pub out_regs: [u32; 32],

    pub pc: u32,         // address of the instruction to fetch next
    pub next_pc: u32,    // address after that — rewritten by branches (delay-slot model)
    pub current_pc: u32, // address of the instruction currently executing (for EPC)

    pub hi: u32, // MULT/DIV high result
    pub lo: u32, // MULT/DIV low result / quotient

    pub cop0: Cop0, // COP0 is part of the CPU, so the CPU owns it (unlike the DMG's bus-owned IRQs)

    /// The CPU owns the bus; every memory access goes through it.
    pub bus: Bus,
}

impl Cpu {
    /// Construct at the R3000 reset state. The CPU comes up executing the BIOS at the reset
    /// vector 0xBFC00000 (KSEG1 — uncached, so it runs before the cache is set up). Unlike
    /// the DMG's HLE boot there's no register state to fake: a real BIOS runs from here, and
    /// the M3 harness either lets it boot or single-steps from this exact PC.
    pub fn new(bus: Bus) -> Self {
        Self {
            regs: [0; 32],
            out_regs: [0; 32],
            pc: 0xBFC0_0000,
            next_pc: 0xBFC0_0004,
            current_pc: 0xBFC0_0000,
            hi: 0,
            lo: 0,
            cop0: Cop0::new(),
            bus,
        }
    }

    /// Execute one instruction, returning the cycles it consumed, and tick the bus by that
    /// much (the catch-up seam). **Stub until M1** — the decoder lands then.
    pub fn step(&mut self) -> u32 {
        unimplemented!("the MIPS R3000A interpreter is milestone M1");
    }
}
