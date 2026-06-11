//! The joypad — the P1/JOYP register at 0xFF00.   *Implemented in milestone M6 (Liam).*
//!
//! Eight buttons share one register through a 2x4 matrix. The CPU writes bit 4 or
//! bit 5 to select either the direction pad or the action buttons, then reads the
//! low nibble. **Everything here is active-low: 0 = pressed, 1 = released.** Pressing
//! a button (a high->low transition on a selected line) raises the Joypad interrupt.

pub struct Joypad {
    /// The selector the CPU last wrote (bits 4-5).
    select: u8,
    // M6: per-button pressed state lives here (set by the host each frame).
}

impl Joypad {
    pub fn new() -> Self {
        Self { select: 0x30 }
    }

    pub fn read(&self) -> u8 {
        // TODO(M6, Liam): merge `select` with the live button state, low nibble
        // active-low. With nothing pressed the register reads 0xCF/0xFF-ish.
        0xFF
    }

    pub fn write(&mut self, val: u8) {
        // Only bits 4-5 (the line selects) are writable.
        self.select = val & 0x30;
    }
}
