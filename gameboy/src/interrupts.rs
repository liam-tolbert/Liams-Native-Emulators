//! Interrupt state: the IF (request) and IE (enable) registers.
//!
//! The Game Boy has five interrupt sources. Each owns one bit, in priority order
//! (bit 0 = highest). A source *requests* an interrupt by setting its bit in IF
//! (0xFF0F); the program *enables* a source by setting its bit in IE (0xFFFF). The
//! CPU services an interrupt only when `IME && (IF & IE & 0x1F) != 0` — that check
//! and the push-PC/jump-to-vector sequence live in the CPU.

pub const VBLANK: u8 = 1 << 0; // PPU entered VBlank          -> vector 0x40
pub const LCD_STAT: u8 = 1 << 1; // configurable PPU/STAT event -> vector 0x48
pub const TIMER: u8 = 1 << 2; // TIMA overflowed             -> vector 0x50
pub const SERIAL: u8 = 1 << 3; // serial transfer complete    -> vector 0x58
pub const JOYPAD: u8 = 1 << 4; // a button went down          -> vector 0x60

pub struct Interrupts {
    /// IF @ 0xFF0F — a bit is set while that interrupt is *pending*.
    flag: u8,
    /// IE @ 0xFFFF — a bit is set while that interrupt is *enabled*.
    enable: u8,
}

impl Interrupts {
    pub fn new() -> Self {
        // Post-boot, IF reads back as 0xE1. Only the low 5 bits are real; the top 3
        // are unused and always read as 1.
        Self { flag: 0xE1, enable: 0x00 }
    }

    /// Raise an interrupt line. Called by the PPU / timer / joypad.
    pub fn request(&mut self, which: u8) {
        self.flag |= which;
    }

    /// Clear pending bit(s) — the CPU clears the line it's about to service.
    pub fn clear(&mut self, which: u8) {
        self.flag &= !which;
    }

    // The unused top 3 bits of IF always read as 1; mask writes to the low 5.
    pub fn read_flag(&self) -> u8 {
        self.flag | 0xE0
    }
    pub fn write_flag(&mut self, v: u8) {
        self.flag = (v & 0x1F) | 0xE0;
    }
    pub fn read_enable(&self) -> u8 {
        self.enable
    }
    pub fn write_enable(&mut self, v: u8) {
        self.enable = v;
    }

    /// Bits that are both pending and enabled (what the CPU acts on). The CPU uses this.
    pub fn pending(&self) -> u8 {
        self.flag & self.enable & 0x1F
    }
}
