//! The hardware timer — DIV / TIMA / TMA / TAC.
//!
//! Register map (all in the 0xFF0_ I/O page):
//!   * 0xFF04 DIV  — increments at 16384 Hz; *any* write resets it to 0.
//!   * 0xFF05 TIMA — the counter; increments at the rate TAC selects.
//!   * 0xFF06 TMA  — TIMA is reloaded from here when it overflows.
//!   * 0xFF07 TAC  — bit 2 enables the timer; bits 0-1 pick the rate.
//!
//! The accurate model keeps an internal 16-bit counter: DIV is its high byte,
//! and TIMA ticks on the *falling edge* of a TAC-selected bit of that counter. That
//! falling-edge detail (and "writing DIV resets the counter, which can itself tick
//! TIMA") is what Blargg's `instr_timing` / the Mooneye timer tests check.

use crate::interrupts::{self, Interrupts};

pub struct Timer {
    /// The 16-bit internal counter that drives everything. DIV (0xFF04) is just its
    /// high byte; TIMA increments off the falling edge of one of its bits (see `step`).
    pub div_counter: u16,
    /// Previous level of the TAC-selected counter bit, kept for falling-edge detection.
    pub last_signal: bool,
    pub tima: u8, // 0xFF05 — the counter itself
    pub tma: u8,  // 0xFF06 — reload value on overflow
    pub tac: u8,  // 0xFF07 — enable (bit 2) + rate select (bits 0-1)
}

impl Timer {
    pub fn new() -> Self {
        // Post-boot state (we HLE the boot ROM, so we start where it would leave us). TAC reads
        // back as 0xF8 — its three real bits clear, so the timer starts disabled at the slowest
        // rate; the unused top 5 bits always read 1. Everything else starts zeroed.
        Self { div_counter: 0, last_signal: false, tima: 0, tma: 0, tac: 0xF8 }
    }

    /// Advance by the T-cycles the last instruction consumed (the catch-up seam).
    pub fn step(&mut self, _t_cycles: u8, ints: &mut Interrupts) {
        // Step one T-cycle at a time so we never miss an edge. TIMA increments on the
        // FALLING edge of a selected counter bit — bit 9/3/5/7 for the four TAC rates
        // (4096 / 262144 / 65536 / 16384 Hz) — and only while the timer is enabled (TAC
        // bit 2). When TIMA overflows 0xFF -> 0x00 it reloads from TMA and requests the
        // TIMER interrupt. Catching the edge (not the level) is what the Blargg/Mooneye
        // timer tests pin down.
        for _ in 0.._t_cycles {
            self.div_counter = self.div_counter.wrapping_add(1);
            let bit = [9,3,5,7][(self.tac & 0b11) as usize];
            let signal = ((self.div_counter >> bit) & 1 == 1) && (self.tac & 0b100 != 0);
            if self.last_signal && !signal{ // falling edge
                let overflow: bool;
                (self.tima, overflow) = self.tima.overflowing_add(1);
                if overflow {
                    self.tima = self.tma;
                    ints.request(interrupts::TIMER)
                }
            }
            self.last_signal = signal;
        }
    }

    pub fn read(&self, addr: u16) -> u8 {
        match addr {
            0xFF04 => (self.div_counter >> 8) as u8,
            0xFF05 => self.tima,
            0xFF06 => self.tma,
            0xFF07 => self.tac | 0xF8, // unused top bits read as 1
            _ => 0xFF,
        }
    }

    pub fn write(&mut self, addr: u16, val: u8) {
        match addr {
            0xFF04 => self.div_counter = 0, // any write to DIV clears it
            0xFF05 => self.tima = val,
            0xFF06 => self.tma = val,
            0xFF07 => self.tac = val & 0x07,
            _ => {}
        }
    }
}
