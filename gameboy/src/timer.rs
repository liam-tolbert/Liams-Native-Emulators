//! The hardware timer — DIV / TIMA / TMA / TAC.   *Implemented in milestone M3 (Liam).*
//!
//! Register map (all in the 0xFF0_ I/O page):
//!   * 0xFF04 DIV  — increments at 16384 Hz; *any* write resets it to 0.
//!   * 0xFF05 TIMA — the counter; increments at the rate TAC selects.
//!   * 0xFF06 TMA  — TIMA is reloaded from here when it overflows.
//!   * 0xFF07 TAC  — bit 2 enables the timer; bits 0-1 pick the rate.
//!
//! The accurate model (M3) keeps an internal 16-bit counter: DIV is its high byte,
//! and TIMA ticks on the *falling edge* of a TAC-selected bit of that counter. That
//! falling-edge detail (and "writing DIV resets the counter, which can itself tick
//! TIMA") is what Blargg's `instr_timing` / the Mooneye timer tests check.

use crate::interrupts::{self, Interrupts};

pub struct Timer {
    pub div: u8,
    pub tima: u8,
    pub tma: u8,
    pub tac: u8,
    // M3: replace the bare `div` byte with a `u16` internal counter and derive DIV
    // from its high byte; add edge-detection state for TIMA.
}

impl Timer {
    pub fn new() -> Self {
        Self { div: 0, tima: 0, tma: 0, tac: 0xF8 }
    }

    /// Advance by the T-cycles the last instruction consumed (the catch-up seam).
    pub fn step(&mut self, _t_cycles: u8, _ints: &mut Interrupts) {
        // TODO(M3, Liam): advance the internal counter; increment TIMA on the falling
        // edge of the TAC-selected bit; on TIMA overflow reload from TMA and
        // `_ints.request(interrupts::TIMER)`. (The `interrupts` import is here ready
        // for you.)
        let _ = interrupts::TIMER;
    }

    pub fn read(&self, addr: u16) -> u8 {
        match addr {
            0xFF04 => self.div,
            0xFF05 => self.tima,
            0xFF06 => self.tma,
            0xFF07 => self.tac | 0xF8, // unused top bits read as 1
            _ => 0xFF,
        }
    }

    pub fn write(&mut self, addr: u16, val: u8) {
        match addr {
            0xFF04 => self.div = 0, // any write to DIV clears it (M3: clear the counter)
            0xFF05 => self.tima = val,
            0xFF06 => self.tma = val,
            0xFF07 => self.tac = val & 0x07,
            _ => {}
        }
    }
}
