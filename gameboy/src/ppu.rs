//! The Picture Processing Unit — a scanline renderer.   *Implemented in M4 / M5.*
//!
//! The PPU walks 154 scanlines of 456 dots each (≈70,224 dots = one 59.7 Hz frame).
//! Per visible line it cycles through modes: OAM scan (2) -> drawing (3) -> HBlank
//! (0); lines 144-153 are VBlank (1). It renders the background + window (M4) and
//! sprites from OAM (M5), and raises the VBlank and STAT interrupts.
//!
//! Output is `framebuffer`, one palette index (0..=3) per pixel — the host maps those
//! four shades to RGB, exactly as `chip8/src/main.rs` mapped its 1-bit display.

use crate::interrupts::{self, Interrupts};

pub const SCREEN_W: usize = 160;
pub const SCREEN_H: usize = 144;

const DOTS_PER_LINE: u16 = 456; // one scanline
const OAM_DOTS: u16 = 80; // mode 2 length
const DRAW_DOTS: u16 = 172; // mode 3 length (we use the fixed minimum)
const TOTAL_LINES: u8 = 154; // 144 visible + 10 VBlank

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

    // --- internal mode-FSM state (not visible to the CPU) ---
    dots: u16,    // dot counter within the current scanline (0..456)
    mode: u8,     // current PPU mode: 2=OAM scan, 3=drawing, 0=HBlank, 1=VBlank
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
            dots: 0,
            mode: 2,
        }
    }

    /// Advance the PPU by the T-cycles the last instruction consumed (the catch-up seam).
    ///
    /// This is the mode state machine. A frame is 154 scanlines of 456 dots. Each visible
    /// line (0-143) runs OAM-scan (mode 2, 80 dots) -> drawing (mode 3, 172 dots) -> HBlank
    /// (mode 0, the rest); lines 144-153 are VBlank (mode 1). We emit a line's pixels the
    /// moment it enters HBlank, and raise the VBlank interrupt + set `frame_ready` when LY
    /// first reaches 144. (STAT-interrupt edge logic is M5.)
    pub fn step(&mut self, t_cycles: u8, ints: &mut Interrupts) {
        // LCD disabled (LCDC bit 7 = 0): the PPU is halted — LY reads 0, no modes, no
        // interrupts, no rendering. Games briefly do this to get free VRAM access.
        if self.lcdc & 0x80 == 0 {
            self.dots = 0;
            self.ly = 0;
            self.mode = 0;
            self.sync_stat_mode();
            return;
        }

        self.dots += t_cycles as u16;
        if self.dots >= DOTS_PER_LINE {
            self.dots -= DOTS_PER_LINE;
            self.ly += 1;
            if self.ly >= TOTAL_LINES {
                self.ly = 0;
            }
            self.update_coincidence();
            if self.ly == SCREEN_H as u8 {
                // Just stepped onto line 144: the visible frame is done.
                self.frame_ready = true;
                ints.request(interrupts::VBLANK);
            }
        }

        // Derive the mode for the current (ly, dots).
        let new_mode = if self.ly >= SCREEN_H as u8 {
            1 // VBlank
        } else if self.dots < OAM_DOTS {
            2 // OAM scan
        } else if self.dots < OAM_DOTS + DRAW_DOTS {
            3 // drawing
        } else {
            0 // HBlank
        };

        if new_mode != self.mode {
            // Entering HBlank on a visible line is exactly when that line's pixels are
            // finalized, so that's where we render it.
            if new_mode == 0 {
                self.render_scanline();
            }
            self.mode = new_mode;
            self.sync_stat_mode();
            // TODO(M5): STAT interrupt. Fire on the RISING edge of the OR of the enabled
            // STAT sources (mode 0/1/2 select = STAT bits 3/4/5, LYC=LY = STAT bit 6).
            // Must be edge-detected or it double-fires. Lands with sprite work in M5.
        }
    }

    /// Mirror the current mode into STAT bits 0-1 (the CPU reads the mode there).
    fn sync_stat_mode(&mut self) {
        self.stat = (self.stat & !0b11) | (self.mode & 0b11);
    }

    /// Update the LY==LYC coincidence flag (STAT bit 2) after LY changes.
    fn update_coincidence(&mut self) {
        if self.ly == self.lyc {
            self.stat |= 1 << 2;
        } else {
            self.stat &= !(1 << 2);
        }
        // TODO(M5): when STAT bit 6 (LYC source) is enabled, an LY==LYC transition here
        // is one of the rising edges that can fire the STAT interrupt.
    }

    /// Render the current scanline (`self.ly`) of the background into `framebuffer`.
    ///
    /// ┌─ M4 HANDS-ON PIECE (Liam) ───────────────────────────────────────────────────┐
    /// │ This is the per-scanline background tile fetch — the visual payoff of M4. With │
    /// │ an empty body the screen stays blank; fill it in and Tetris's title/playfield  │
    /// │ appears. Index `self.vram` from 0 (its addresses are 0x8000-based, so a VRAM   │
    /// │ *offset* = address - 0x8000).                                                  │
    /// │                                                                                │
    /// │ For each pixel x in 0..SCREEN_W, at line `self.ly`:                            │
    /// │  1. Scroll. The BG is a 256x256 space that WRAPS:                              │
    /// │       bg_y = (self.ly as u16 + self.scy as u16) & 0xFF                         │
    /// │       bg_x = (x as u16 + self.scx as u16) & 0xFF                               │
    /// │  2. Tile map (which 32x32 map of tile indices). LCDC bit 3 selects the base:   │
    /// │       0 -> VRAM offset 0x1800 (0x9800),  1 -> 0x1C00 (0x9C00)                  │
    /// │       tile_id = vram[map_base + (bg_y/8)*32 + (bg_x/8)]                        │
    /// │  3. Tile DATA address — THE classic gotcha, signed vs unsigned. LCDC bit 4:    │
    /// │       1 -> base 0x0000, tile_id is UNSIGNED:  data = tile_id*16               │
    /// │       0 -> base 0x1000, tile_id is SIGNED:    data = 0x1000 + (tile_id as i8 │
    /// │                                                            as i16)*16          │
    /// │     Each tile = 16 bytes = 8 rows x 2 bytes (2 bits per pixel = "2bpp").       │
    /// │  4. Decode the 2bpp pixel. row = bg_y % 8; that row is two bytes:              │
    /// │       lo = vram[data + row*2]      hi = vram[data + row*2 + 1]                 │
    /// │     Bit 7 is the LEFTMOST pixel, so:  bit = 7 - (bg_x % 8)                     │
    /// │       color_id = (((hi >> bit) & 1) << 1) | ((lo >> bit) & 1)   // 0..3        │
    /// │  5. Palette. The 4 entries of BGP map a color_id to a shade:                   │
    /// │       shade = (self.bgp >> (color_id * 2)) & 0b11                              │
    /// │  6. Store: framebuffer[self.ly as usize * SCREEN_W + x] = shade                │
    /// │                                                                                │
    /// │ Do the background first — that's the checkpoint. The window layer (LCDC bit 5, │
    /// │ positioned at WX-7 / WY, reusing steps 2-5 against the window map) is the      │
    /// │ follow-on once the background is on screen.                                    │
    /// └────────────────────────────────────────────────────────────────────────────────┘
    fn render_scanline(&mut self) {
        // TODO(M4, Liam): implement the per-pixel background fetch described above.
        let ly = self.ly as usize;
        let map_base: usize = if self.lcdc & 0x08 != 0 { 0x1C00 } else { 0x1800 };
        let unsigned = self.lcdc & 0x10 != 0;

        for x in 0..SCREEN_W {
            let bg_y = (ly + self.scy as usize) & 0xFF;
            let bg_x = (x + self.scx as usize) & 0xFF;

            let tile_id = self.vram[map_base + bg_y/8 * 32 + bg_x/8];

            let data: usize = if unsigned {
                (tile_id as usize) * 16
            }else{
                (0x1000 + (tile_id as i8 as i16) * 16) as usize
            };

            let row = bg_y % 8;
            let lo = self.vram[data + row * 2];
            let hi = self.vram[data + row * 2 + 1];

            // the 2bpp decode
            let bit = 7 - (bg_x % 8);
            let color_id = (((hi >> bit) & 1) << 1) | ((lo >> bit) & 1);

            let shade = (self.bgp >> (color_id * 2)) & 3;
            self.framebuffer[ly * SCREEN_W + x] = shade;
        }
    }

    pub fn read(&self, addr: u16) -> u8 {
        match addr {
            0x8000..=0x9FFF => self.vram[(addr - 0x8000) as usize],
            0xFE00..=0xFE9F => self.oam[(addr - 0xFE00) as usize],
            0xFF40 => self.lcdc,
            0xFF41 => self.stat | 0x80, // bit 7 is unused and always reads 1
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
            // Only the interrupt-select bits 3-6 are writable; the PPU owns the mode
            // (bits 0-1) and the LY==LYC coincidence flag (bit 2).
            0xFF41 => self.stat = (self.stat & 0x07) | (val & 0x78),
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
