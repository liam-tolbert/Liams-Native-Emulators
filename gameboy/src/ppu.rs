//! The Picture Processing Unit — a scanline renderer.
//!
//! The PPU walks 154 scanlines of 456 dots each (≈70,224 dots = one 59.7 Hz frame).
//! Per visible line it cycles through modes: OAM scan (2) -> drawing (3) -> HBlank
//! (0); lines 144-153 are VBlank (1). It renders the background + window and
//! sprites from OAM, and raises the VBlank and STAT interrupts.
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
    dots: u16,       // dot counter within the current scanline (0..456)
    mode: u8,        // current PPU mode: 2=OAM scan, 3=drawing, 0=HBlank, 1=VBlank
    window_line: u8, // the window's own row counter (advances only on lines it draws)
    stat_line: bool, // previous level of the STAT interrupt line (for rising-edge detect)
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
            window_line: 0,
            stat_line: false,
        }
    }

    /// Advance the PPU by the T-cycles the last instruction consumed (the catch-up seam).
    ///
    /// This is the mode state machine. A frame is 154 scanlines of 456 dots. Each visible
    /// line (0-143) runs OAM-scan (mode 2, 80 dots) -> drawing (mode 3, 172 dots) -> HBlank
    /// (mode 0, the rest); lines 144-153 are VBlank (mode 1). We emit a line's pixels the
    /// moment it enters HBlank, and raise the VBlank interrupt + set `frame_ready` when LY
    /// first reaches 144. (STAT-interrupt edges are handled in `refresh_stat_line`.)
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
                // The window's row counter restarts at the top of each new frame.
                self.window_line = 0;
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
        }

        // STAT is edge-triggered off the OR of its enabled sources; recompute after every
        // mode/LY change so it fires exactly once per rising edge.
        self.refresh_stat_line(ints);
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
        // (When STAT bit 6 — the LYC source — is enabled, an LY==LYC change is one of the
        // STAT-interrupt rising edges; `refresh_stat_line` picks it up.)
    }

    /// Recompute the STAT interrupt line and fire LCD_STAT on its rising edge.
    ///
    /// The line is the OR of the *enabled* STAT sources: PPU mode == 0 / 1 / 2 (selected by
    /// STAT bits 3 / 4 / 5) and LY == LYC (STAT bit 6). The interrupt is requested only on a
    /// false->true transition of that OR — "edge-triggered" — otherwise it would re-fire on
    /// every step a source stayed active and games would drown in interrupts. `stat_line`
    /// remembers the previous level so we can see the edge.
    fn refresh_stat_line(&mut self, ints: &mut Interrupts) {
        let line = (self.stat & 0x08 != 0 && self.mode == 0)
            || (self.stat & 0x10 != 0 && self.mode == 1)
            || (self.stat & 0x20 != 0 && self.mode == 2)
            || (self.stat & 0x40 != 0 && self.ly == self.lyc);
        if line && !self.stat_line {
            ints.request(interrupts::LCD_STAT);
        }
        self.stat_line = line;
    }

    /// Render the current scanline (`self.ly`) of the background into `framebuffer`.
    ///
    /// The per-scanline background tile fetch. The steps below map one-to-one onto the
    /// code in this method and `bg_color_id`. VRAM is indexed from 0, but its hardware
    /// addresses are 0x8000-based, so a VRAM *offset* = address - 0x8000.
    ///
    /// For each pixel x in 0..SCREEN_W, at line `self.ly`:
    ///  1. Scroll. The BG is a 256x256 space that WRAPS:
    ///       bg_y = (self.ly + self.scy) & 0xFF
    ///       bg_x = (x       + self.scx) & 0xFF
    ///  2. Tile map (which 32x32 map of tile indices). LCDC bit 3 selects the base:
    ///       0 -> VRAM offset 0x1800 (0x9800),  1 -> 0x1C00 (0x9C00)
    ///       tile_id = vram[map_base + (bg_y/8)*32 + (bg_x/8)]
    ///  3. Tile DATA address — THE classic gotcha, signed vs unsigned. LCDC bit 4:
    ///       1 -> base 0x0000, tile_id is UNSIGNED:  data = tile_id*16
    ///       0 -> base 0x1000, tile_id is SIGNED:    data = 0x1000 + (tile_id as i8 as i16)*16
    ///     Each tile = 16 bytes = 8 rows x 2 bytes (2 bits per pixel = "2bpp").
    ///  4. Decode the 2bpp pixel. row = bg_y % 8; that row is two bytes:
    ///       lo = vram[data + row*2]      hi = vram[data + row*2 + 1]
    ///     Bit 7 is the LEFTMOST pixel, so:  bit = 7 - (bg_x % 8)
    ///       color_id = (((hi >> bit) & 1) << 1) | ((lo >> bit) & 1)   // 0..3
    ///  5. Palette. The 4 entries of BGP map a color_id to a shade:
    ///       shade = (self.bgp >> (color_id * 2)) & 0b11
    ///  6. Store: framebuffer[self.ly * SCREEN_W + x] = shade
    ///
    /// The window layer (LCDC bit 5, positioned at WX-7 / WY) reuses steps 2-5 against
    /// the window map and is drawn over the background; the shared lookup lives in
    /// `bg_color_id`.
    fn render_scanline(&mut self) {
        let ly = self.ly as usize;
        // The BG/window color INDEX (0..3, pre-palette) at each pixel of this line. Sprites
        // need this for priority: an "OBJ-behind-BG" pixel only shows where the BG index 0.
        let mut bg_ids = [0u8; SCREEN_W];

        // DMG quirk: LCDC bit 0 clear blanks the background AND window. Sprites are gated
        // separately (LCDC bit 1), so we DON'T return — we still fall through to them.
        if self.lcdc & 0x01 != 0 {
            // Both layers share the same tile-data addressing mode (LCDC bit 4).
            let unsigned = self.lcdc & 0x10 != 0;

            // --- Background: a 256x256 space the screen scrolls around with SCX/SCY ---
            let bg_map: usize = if self.lcdc & 0x08 != 0 { 0x1C00 } else { 0x1800 };
            for x in 0..SCREEN_W {
                let bg_y = (ly + self.scy as usize) & 0xFF; // wraps at 256
                let bg_x = (x + self.scx as usize) & 0xFF;
                let id = self.bg_color_id(bg_map, bg_x, bg_y, unsigned);
                bg_ids[x] = id;
                self.framebuffer[ly * SCREEN_W + x] = (self.bgp >> (id * 2)) & 3;
            }

            // --- Window: a second layer drawn OVER the background ---
            // Ignores SCX/SCY; anchored on screen at (WX-7, WY) and walks its own row
            // counter. Drawn only if enabled (LCDC bit 5) and we've reached its top (LY>=WY).
            // The window only renders — and its internal line counter only advances — on
            // scanlines where it's actually ON-SCREEN. WX>=167 (win_start>=160) parks the
            // window past the right edge so it draws nothing; the counter MUST freeze there.
            // dmg-acid2 hides the window this way for a mid-frame band to test exactly this:
            // if the counter keeps ticking, the window content below the band shifts up and
            // (here) an eye lands on the chin. The counter tracks RENDERING, not just enable.
            let win_start = self.wx as i16 - 7; // screen x where the window's column 0 sits
            if self.lcdc & 0x20 != 0 && ly >= self.wy as usize && win_start < SCREEN_W as i16 {
                let win_map: usize = if self.lcdc & 0x40 != 0 { 0x1C00 } else { 0x1800 };
                let win_y = self.window_line as usize;
                for x in 0..SCREEN_W {
                    if (x as i16) < win_start {
                        continue; // this pixel is to the left of the window
                    }
                    let win_x = (x as i16 - win_start) as usize;
                    let id = self.bg_color_id(win_map, win_x, win_y, unsigned);
                    bg_ids[x] = id;
                    self.framebuffer[ly * SCREEN_W + x] = (self.bgp >> (id * 2)) & 3;
                }
                // Advance once per line the window is actually drawn — deliberately NOT
                // `ly - WY`, so hiding the window mid-frame can't desync its vertical position.
                self.window_line += 1;
            }
        } else {
            // BG/window off: blank the line. bg_ids stays all-0 (treated as transparent).
            for x in 0..SCREEN_W {
                self.framebuffer[ly * SCREEN_W + x] = 0;
            }
        }

        // Sprites draw on top, if enabled (LCDC bit 1), subject to per-sprite priority.
        if self.lcdc & 0x02 != 0 {
            self.render_sprites(ly, &bg_ids);
        }


    }

    /// Fetch one background/window pixel's COLOR INDEX (0..=3, *before* palette mapping).
    /// `(px, py)` are coordinates inside the chosen 256x256 tile-map space and `map_base`
    /// is that map's VRAM offset (0x1800 or 0x1C00). Returning the raw index (not the BGP
    /// shade) is what lets sprite priority ask "is the BG index 0 here?". The background and
    /// window share this because the lookup is identical — only the coords and map differ.
    fn bg_color_id(&self, map_base: usize, px: usize, py: usize, unsigned: bool) -> u8 {
        // 1. tile number from the 32-wide map
        let tile_id = self.vram[map_base + (py / 8) * 32 + (px / 8)];
        // 2. where that tile's 16 bytes live (the signed/unsigned addressing modes)
        let data: usize = if unsigned {
            (tile_id as usize) * 16
        } else {
            (0x1000 + (tile_id as i8 as i16) * 16) as usize
        };
        // 3. the two bytes (bit-planes) of this row of the tile
        let row = py % 8;
        let lo = self.vram[data + row * 2];
        let hi = self.vram[data + row * 2 + 1];
        // 4. recombine the planes; bit 7 is the leftmost pixel
        let bit = 7 - (px % 8);
        (((hi >> bit) & 1) << 1) | ((lo >> bit) & 1)
    }

    /// Draw the sprites (OBJ) intersecting the current scanline, on top of the BG/window.
    ///
    /// How it works (the steps below map onto the code in this method):
    /// OAM (`self.oam`) holds 40 sprites x 4 bytes:
    ///   byte 0: Y + 16   (screen_y = oam[0] - 16; the +16 lets sprites slide off the top)
    ///   byte 1: X + 8    (screen_x = oam[1] - 8)
    ///   byte 2: tile index   (in 8x16 mode the low bit is ignored: top tile = id & 0xFE)
    ///   byte 3: flags    bit7 = priority (0 = above BG; 1 = behind BG indices 1-3)
    ///                    bit6 = Y-flip   bit5 = X-flip   bit4 = palette (0=OBP0, 1=OBP1)
    ///
    /// Steps for line `ly`:
    ///  1. height = if LCDC bit 2 set { 16 } else { 8 }.
    ///  2. Walk OAM (0..40). A sprite covers this line if  screen_y <= ly < screen_y+height.
    ///     screen_y can be negative (sprite partly off the top) — compute it as i16.
    ///  3. The 10-per-line limit: keep only the FIRST 10 covering sprites (hardware cap).
    ///  4. Sprite-vs-sprite priority (DMG): smaller X wins; ties broken by lower OAM index.
    ///     Easiest correct trick: draw the chosen sprites from LOWEST priority to HIGHEST
    ///     (e.g. iterate them in reverse of (x, oam_index) order) so the winner lands on top.
    ///  5. Row within the sprite = ly - screen_y, then Y-flip if bit6: row = height-1 - row.
    ///     Sprites ALWAYS use unsigned 0x8000 addressing — tile bytes at tile_index*16, the
    ///     row's two bytes at +row*2 and +row*2+1 — the SAME 2bpp decode as `bg_color_id`.
    ///  6. For each column col in 0..8: bit = if x-flip { col } else { 7 - col }; decode the
    ///     2-bit color_id. color_id 0 = TRANSPARENT → skip (leave BG/other sprite showing).
    ///  7. Priority: if flags bit7 is set, only draw where `bg_ids[screen_x + col] == 0`.
    ///  8. Shade = ((if bit4 { self.obp1 } else { self.obp0 }) >> (color_id * 2)) & 3, then
    ///     write self.framebuffer[ly * SCREEN_W + (screen_x + col)]. Bounds-check screen_x+col.
    ///
    /// dmg-acid2 flags each of these (flips, priority, the 10-limit, 8x16, transparency)
    /// with a distinct visual artifact, so it's a precise per-feature debugger.
    fn render_sprites(&mut self, ly: usize, bg_ids: &[u8; SCREEN_W]) {
        // LCDC bit 2 sets the sprite height for the whole frame: 8x8 or 8x16.
        let height: i16 = if self.lcdc & 0x04 != 0 { 16 } else { 8 };

        // which sprites cover this scanline?
        // Hardware can fetch at most 10 sprites per line, chosen in OAM order (NOT by
        // X). We mirror that exactly: walk 0..40, keep the first 10 whose vertical
        // band includes `ly`. We store each chosen sprite's OAM byte-offset.
        let mut line_sprites = [0usize; 10];
        let mut count = 0;
        for i in 0..40 {
            let oam = i * 4;

            let screen_y = self.oam[oam] as i16 - 16;
            if (ly as i16) >= screen_y && (ly as i16) < screen_y + height {
                line_sprites[count] = oam;
                count += 1;
                if count == 10 {
                    break; // 10 sprites per line — hardware cap
                }
            }
        }

        let chosen = &mut line_sprites[..count];
        chosen.sort_unstable_by_key(|&oam| (self.oam[oam + 1], oam));

        for &oam in chosen.iter().rev(){
            let screen_x = self.oam[oam + 1] as i16 - 8;
            let tile = self.oam[oam + 2];
            let flags = self.oam[oam + 3];
            let behind_bg = flags & 0x80 != 0;
            let y_flip = flags & 0x40 != 0;
            let x_flip = flags & 0x20 != 0;
            let palette = if flags & 0x10 != 0 { self.obp1 } else { self.obp0 };

            let mut row = ly as i16 - (self.oam[oam] as i16 - 16);
            if y_flip {
                row = height - 1 - row;
            }

            let tile_index = if height == 16 { tile & 0xFE } else { tile } as usize;
            let data = tile_index * 16 + row as usize * 2;
            let lo = self.vram[data];
            let hi = self.vram[data + 1];

            for col in 0..8u8 {
                let bit = if x_flip { col } else { 7 - col };
                let color_id = (((hi >> bit) & 1) << 1) | ((lo >> bit) & 1);
                if color_id == 0 {
                    continue; // sprite color 0 is ALWAYS transparent (unlike the BG)
                }

                let px = screen_x + col as i16;
                if px < 0 || px >= SCREEN_W as i16 {
                    continue; // clipped off the left or right edge
                }
                let px = px as usize;

                if behind_bg && bg_ids[px] != 0 {
                    continue;
                }

                let shade = (palette >> (color_id * 2)) & 3;
                self.framebuffer[ly * SCREEN_W + px] = shade;

            }
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
            // 0xFF46 OAM DMA is handled by the bus (it copies 0xXX00-0xXX9F -> OAM).
            _ => {}
        }
    }
}
