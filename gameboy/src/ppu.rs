//! The Picture Processing Unit — a scanline renderer.   *Implemented in M4 / M5.*
//!
//! The PPU walks 154 scanlines of 456 dots each (≈70,224 dots = one 59.7 Hz frame).
//! Per visible line it cycles through modes: OAM scan (2) -> drawing (3) -> HBlank
//! (0); lines 144-153 are VBlank (1). It renders the background + window (M4) and
//! sprites from OAM (M5), and raises the VBlank and STAT interrupts.
//!
//! Output is `framebuffer`, one palette index (0..=3) per pixel — the host maps those
//! four shades to RGB, exactly as `chip8/src/main.rs` mapped its 1-bit display.

use crate::interrupts::Interrupts;

pub const SCREEN_W: usize = 160;
pub const SCREEN_H: usize = 144;

pub struct Ppu {
    pub vram: [u8; 0x2000], // 0x8000-0x9FFF: tile data + tile maps
    pub oam: [u8; 0xA0],    // 0xFE00-0xFE9F: 40 sprites x 4 bytes

    // LCD registers (0xFF40-0xFF4B).
    pub lcdc: u8, // LCD control
    pub stat: u8, // LCD status / mode + interrupt selects
    pub scy: u8,
    pub scx: u8, // background scroll
    pub ly: u8,  // current scanline (read-only to the CPU)
    pub lyc: u8, // LY compare (for the STAT LYC=LY interrupt)
    pub bgp: u8, // background palette
    pub obp0: u8,
    pub obp1: u8, // the two sprite palettes
    pub wy: u8,
    pub wx: u8, // window position

    /// One palette index (0..=3) per pixel; read by the host to draw a frame.
    pub framebuffer: [u8; SCREEN_W * SCREEN_H],
    /// Set true when a frame finishes (entering VBlank); the host blits, then clears it.
    pub frame_ready: bool,
}

impl Ppu {
    pub fn new() -> Self {
        // Post-boot register values (Pan Docs). LCDC=0x91 => LCD on, BG on, etc.
        Self {
            vram: [0; 0x2000],
            oam: [0; 0xA0],
            lcdc: 0x91,
            stat: 0x85,
            scy: 0,
            scx: 0,
            ly: 0,
            lyc: 0,
            bgp: 0xFC,
            obp0: 0xFF,
            obp1: 0xFF,
            wy: 0,
            wx: 0,
            framebuffer: [0; SCREEN_W * SCREEN_H],
            frame_ready: false,
        }
    }

    /// Advance by the T-cycles the last instruction consumed (the catch-up seam).
    pub fn step(&mut self, _t_cycles: u8, _ints: &mut Interrupts) {
        // TODO(M4): the mode state machine (dot counter, mode transitions, LY/LYC),
        // raise VBlank + STAT interrupts, set `frame_ready` at the start of VBlank,
        // and render each scanline. M4 = background+window, M5 = sprites.
    }

    pub fn read(&self, addr: u16) -> u8 {
        match addr {
            0x8000..=0x9FFF => self.vram[(addr - 0x8000) as usize],
            0xFE00..=0xFE9F => self.oam[(addr - 0xFE00) as usize],
            0xFF40 => self.lcdc,
            0xFF41 => self.stat,
            0xFF42 => self.scy,
            0xFF43 => self.scx,
            0xFF44 => self.ly,
            0xFF45 => self.lyc,
            0xFF47 => self.bgp,
            0xFF48 => self.obp0,
            0xFF49 => self.obp1,
            0xFF4A => self.wy,
            0xFF4B => self.wx,
            _ => 0xFF, // 0xFF46 (DMA) is write-only; 0xFF4C-0xFF4F unused on DMG
        }
    }

    pub fn write(&mut self, addr: u16, val: u8) {
        match addr {
            0x8000..=0x9FFF => self.vram[(addr - 0x8000) as usize] = val,
            0xFE00..=0xFE9F => self.oam[(addr - 0xFE00) as usize] = val,
            0xFF40 => self.lcdc = val,
            0xFF41 => self.stat = val,
            0xFF42 => self.scy = val,
            0xFF43 => self.scx = val,
            0xFF44 => {} // LY is read-only
            0xFF45 => self.lyc = val,
            0xFF47 => self.bgp = val,
            0xFF48 => self.obp0 = val,
            0xFF49 => self.obp1 = val,
            0xFF4A => self.wy = val,
            0xFF4B => self.wx = val,
            // 0xFF46 OAM DMA is handled by the bus in M5 (it copies 0xXX00-0xXX9F -> OAM).
            _ => {}
        }
    }
}
