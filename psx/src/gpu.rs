//! The GPU — **M4a: VRAM + the GP0/GP1 register model + a real GPUSTAT.**
//!
//! ## What the PS1 GPU is (for someone who's never met it)
//!
//! The PlayStation has no framebuffer in main RAM. Instead the GPU owns a separate **1 MiB of
//! video RAM** — `1024 x 512` pixels, each a **16-bit** value — and *everything* the machine
//! displays or draws lives there: the visible picture, off-screen render targets, texture
//! images, and the colour lookup tables (CLUTs) textures index into. The CPU never touches VRAM
//! directly; it talks to the GPU through just **two 32-bit I/O ports**:
//!
//!   * **GP0** (`0x1F801810` write) — the *drawing / data* port. Rendering primitives (triangles,
//!     rectangles, lines), VRAM<->VRAM/CPU image transfers, and the draw-state settings all arrive
//!     here as a stream of 32-bit words.
//!   * **GP1** (`0x1F801814` write) — the *display-control* port. Reset, display on/off, where in
//!     VRAM the visible window sits, the video mode, the DMA direction. These are "knobs", not draw
//!     commands.
//!
//! And two *read* ports report back:
//!
//!   * **GPUREAD** (`0x1F801810` read) — returns VRAM pixels during a VRAM->CPU copy, or the answer
//!     to a "GPU info" request.
//!   * **GPUSTAT** (`0x1F801814` read) — a status word the CPU polls constantly: is the GPU ready
//!     for another command? for a DMA block? what video mode is set? This is the register a booting
//!     BIOS spins on, so getting it right is what lets the BIOS proceed.
//!
//! ## Why GP0 is a state machine, not a function
//!
//! A single GP0 *command* is several words long — a flat triangle is 4 words (a colour, then three
//! XY vertices), a Gouraud-shaded textured quad is 11. But the CPU writes the port **one 32-bit word
//! at a time**, and (as of M4c) the DMA engine dribbles those same words in independently. So
//! the GPU can't treat each write as a whole command: it must remember a *partial* command across
//! writes — read the first word to learn which command and how many words it needs, accumulate that
//! many, then execute. That accumulate-then-fire logic is the heart of this file (`gp0`).
//!
//! ## What M4a does (and deliberately doesn't)
//!
//! M4a builds the **skeleton**: the VRAM array, the GP0/GP1 command model, and a GPUSTAT computed
//! from real internal state (replacing the M0-M3 stub that just forced the "ready" bits on). It
//! **draws nothing** — every rendering and image-transfer command is *parsed* (so the FIFO stays in
//! sync) but its executor is a stub. The pixel work lands in later stages:
//!   * M4b — VRAM transfers & fill (`02`, `A0`, `C0`, `80`).
//!   * M4c — DMA channels 2 (GPU) + 6 (OTC) that feed GP0/VRAM without the CPU.
//!   * M4d — the software rasterizer (polygons, rectangles, lines).
//!   * M4e — display timing (VBlank) and the on-screen window.
//! The draw-*state* commands (`E1`-`E6`) and the GP1 knobs are fully modelled now, because GPUSTAT is
//! assembled from them and later stages will read them straight back.

use std::cell::Cell;

/// VRAM is 1024 wide x 512 tall, 16 bits per pixel = exactly 1 MiB.
const VRAM_W: usize = 1024;
const VRAM_H: usize = 512;

pub struct Gpu {
    /// The 1 MiB of video RAM, row-major (`y * 1024 + x`). Heap-allocated as a `Vec` rather than a
    /// `[u16; 524288]` so constructing the GPU never parks a 1 MiB array on the stack (a real
    /// stack-overflow risk in an unoptimised build — the same reason `Bus` heap-allocates RAM).
    vram: Vec<u16>,

    // ===== GP0 command FIFO state machine ===============================================
    // A command is gathered here word-by-word until it's complete, then dispatched. See `gp0`.
    /// Words gathered so far for the command in progress (max real length is 12, so 16 is safe).
    gp0_buffer: [u32; 16],
    /// How many words we've put in `gp0_buffer`.
    gp0_len: usize,
    /// How many *more* words this command still needs before it's complete. 0 = idle / done.
    gp0_remaining: usize,
    /// The command byte (top 8 bits of the first word) currently being assembled.
    gp0_command: u8,
    /// Words still to consume as raw pixel data during a CPU->VRAM image upload (`A0`). While this
    /// is non-zero, GP0 writes are image data, not commands. (M4a drops them; M4b writes VRAM.)
    gp0_img_words: usize,
    /// True while draining a variable-length polyline (`48`-`5F` with the polyline bit) until its
    /// terminator word arrives. Polylines have no fixed length, so they can't use `gp0_remaining`.
    gp0_polyline: bool,
    /// Polyline render state (M4d-1). A polyline streams an arbitrary number of points; we draw each
    /// segment the moment its far endpoint arrives, so we only need the *previous* point/colour rather
    /// than buffering the whole list. `poly_is_gouraud` selects the per-point colour↔vertex interleave;
    /// `poly_expect_color` tracks which half of that pair is next (Gouraud only). Colours are kept as
    /// raw 8-bit channels here so Gouraud interpolation and dithering can work at full precision.
    poly_is_gouraud: bool,
    poly_expect_color: bool,
    poly_have_prev: bool,
    poly_prev_pt: (i32, i32),
    poly_prev_col: (i32, i32, i32),
    poly_pending_col: (i32, i32, i32),

    // ===== draw state (set by GP0 E1-E6; read back by the rasterizer in M4d) =============
    /// GP0(E1) "draw mode": texture-page base, semi-transparency mode, texture colour depth,
    /// dither enable, draw-to-display-area flag, texture-disable, rectangle texture flips. We keep
    /// the low 14 bits verbatim because GPUSTAT bits 0-10 and 15 are taken straight from them.
    draw_mode: u32,
    /// GP0(E2) texture window (mask/offset, in 8-pixel units). Stored for M4d texture sampling.
    tex_window: u32,
    /// GP0(E3/E4) clipping rectangle — the rasterizer must not draw outside it.
    draw_area_left: u16,
    draw_area_top: u16,
    draw_area_right: u16,
    draw_area_bottom: u16,
    /// GP0(E5) drawing offset added to every vertex. **Signed 11-bit** — see the sign-extension
    /// note in `execute_gp0`.
    draw_offset_x: i16,
    draw_offset_y: i16,
    /// GP0(E6) mask-bit behaviour: force the mask bit (bit 15) set on every pixel drawn, and/or
    /// refuse to draw over a pixel whose mask bit is already set. Surface as GPUSTAT bits 11/12.
    force_set_mask: bool,
    check_mask: bool,

    // ===== display state (set by GP1; read back via GPUSTAT) =============================
    /// GP1(03): is the display output blanked? Power-on and reset leave it blanked until the BIOS
    /// turns it on. (GPUSTAT bit 23 — note the polarity: 1 = *disabled*.)
    display_disabled: bool,
    /// GP1(04): DMA direction (0 off, 1 FIFO, 2 CPU->GP0, 3 GPUREAD->CPU). Drives GPUSTAT bit 25 —
    /// see the gotcha in `status`.
    dma_direction: u8,
    /// GP1(09) "texture disable allowed": only while this is set does a primitive's texture-disable
    /// bit (E1/texpage bit 11) actually take effect and surface as GPUSTAT bit 15. The BIOS leaves it
    /// off, so a stray texture-disable bit is normally inert (see the `gpu/gp0-e1` test).
    texture_disable_allowed: bool,
    /// GP1(05): top-left of the visible window inside VRAM.
    display_vram_x: u16,
    display_vram_y: u16,
    /// GP1(06/07): the horizontal/vertical display range, in GPU clocks / scanlines. Stored for the
    /// M4e window sizing; not load-bearing yet.
    display_h1: u16,
    display_h2: u16,
    display_v1: u16,
    display_v2: u16,
    /// GP1(08) display mode, low 8 bits kept verbatim — horizontal/vertical resolution, NTSC/PAL,
    /// 15/24-bit colour, interlace, the "reverse" flag. Scattered into GPUSTAT bits 14 and 16-22.
    display_mode: u32,

    /// The staged GPUREAD response (answer to a GP1(10) "GPU info" request; VRAM read-back is M4b).
    gpuread: u32,
    /// The GPU's own interrupt request (GP0(1F) raises it, GP1(02) acknowledges it). GPUSTAT bit 24;
    /// it feeds IRQ source 1 once that's wired in a later stage.
    irq: bool,

    // ===== in-progress VRAM image transfers (M4b) =======================================
    /// CPU->VRAM upload (`A0`) cursor: the destination rectangle and how many of its pixels have been
    /// written so far. While `gp0_img_words` is non-zero, GP0 data words land here.
    up_x: u16,
    up_y: u16,
    up_w: u16,
    up_h: u16,
    up_px: u32,
    /// VRAM->CPU download (`C0`) cursor. `dl_px` is a `Cell` because GPUREAD advances the stream from
    /// `read(&self)` — a read that mutates, the ownership wrinkle flagged in M4a (see `read`).
    dl_x: u16,
    dl_y: u16,
    dl_w: u16,
    dl_h: u16,
    dl_px: Cell<u32>,
}

impl Gpu {
    /// Power-on / construction state. The real reset values are applied by the BIOS issuing
    /// GP1(00); we start from the same blanked, DMA-off baseline so a GP1(00) is idempotent.
    pub fn new() -> Self {
        Self {
            vram: vec![0; VRAM_W * VRAM_H],
            gp0_buffer: [0; 16],
            gp0_len: 0,
            gp0_remaining: 0,
            gp0_command: 0,
            gp0_img_words: 0,
            gp0_polyline: false,
            poly_is_gouraud: false,
            poly_expect_color: false,
            poly_have_prev: false,
            poly_prev_pt: (0, 0),
            poly_prev_col: (0, 0, 0),
            poly_pending_col: (0, 0, 0),
            draw_mode: 0,
            tex_window: 0,
            draw_area_left: 0,
            draw_area_top: 0,
            draw_area_right: 0,
            draw_area_bottom: 0,
            draw_offset_x: 0,
            draw_offset_y: 0,
            force_set_mask: false,
            check_mask: false,
            display_disabled: true,
            dma_direction: 0,
            texture_disable_allowed: false,
            display_vram_x: 0,
            display_vram_y: 0,
            display_h1: 0,
            display_h2: 0,
            display_v1: 0,
            display_v2: 0,
            display_mode: 0,
            gpuread: 0,
            irq: false,
            up_x: 0,
            up_y: 0,
            up_w: 0,
            up_h: 0,
            up_px: 0,
            dl_x: 0,
            dl_y: 0,
            dl_w: 0,
            dl_h: 0,
            dl_px: Cell::new(0),
        }
    }

    // ===== VRAM access ==================================================================
    /// Borrow VRAM for the host-side dump/diff harness (read-only).
    pub fn vram(&self) -> &[u16] {
        &self.vram
    }
    /// Mutable VRAM, used only by the M4a self-test to poke a known pattern for harness calibration.
    /// The drawing commands that fill VRAM for real arrive in M4b (transfers) and M4d (rasterizer).
    pub fn vram_mut(&mut self) -> &mut [u16] {
        &mut self.vram
    }

    /// Write one pixel. **Gotcha: VRAM coordinates wrap** — VRAM is a fixed 1024x512 torus, and the
    /// hardware silently masks X to 0..1023 and Y to 0..511 rather than faulting. A primitive whose
    /// offset pushes it off the right edge reappears on the left, so the wrap is behaviour to
    /// reproduce, not an error to guard against. (Used by M4b onward; here so the rule lives in one spot.)
    fn vram_set(&mut self, x: i32, y: i32, color: u16) {
        let x = (x as usize) & (VRAM_W - 1); // & 1023  (VRAM_W is a power of two)
        let y = (y as usize) & (VRAM_H - 1); // & 511
        self.vram[y * VRAM_W + x] = color;
    }

    /// Read one pixel, with the same wrap as `vram_set`.
    fn vram_get(&self, x: i32, y: i32) -> u16 {
        let x = (x as usize) & (VRAM_W - 1);
        let y = (y as usize) & (VRAM_H - 1);
        self.vram[y * VRAM_W + x]
    }

    // ===== the shared pixel pipeline (M4d) ==============================================

    /// Write one already-shaded pixel for a *drawn* primitive (polygon / line / rectangle). This is
    /// the single choke point every rasterizer routine funnels through, so the three rules that apply
    /// to *every* drawn pixel live in exactly one place:
    ///   1. **Scissor** — the GP0(E3/E4) drawing area is an *inclusive* box; pixels outside it are
    ///      dropped (this is the GPU's clip, distinct from the VRAM wrap in `vram_set`).
    ///   2. **Mask check** (GP0(E6) bit 1) — if the destination pixel already has its mask bit (15)
    ///      set, leave it alone (a write-protect the hardware honours per-pixel).
    ///   3. **Mask force** (GP0(E6) bit 0) — OR the mask bit into whatever we do write.
    /// (The `02` FILL deliberately bypasses all three — see `execute_gp0` — which is why it doesn't
    /// call here.) The seam marked below is where M4d-3 semi-transparency will blend against `dst`.
    fn plot(&mut self, x: i32, y: i32, color: u16) {
        if x < self.draw_area_left as i32
            || x > self.draw_area_right as i32
            || y < self.draw_area_top as i32
            || y > self.draw_area_bottom as i32
        {
            return;
        }
        let dst = self.vram_get(x, y);
        if self.check_mask && dst & 0x8000 != 0 {
            return;
        }
        // --- M4d-3 semi-transparency seam: out = blend(color, dst, semi_mode) goes here -----------
        let out = if self.force_set_mask { color | 0x8000 } else { color };
        self.vram_set(x, y, out);
    }

    /// Decode a primitive vertex/position word into screen coordinates. **Gotcha: X and Y are each a
    /// *signed* 11-bit field** (X in bits 0-10, Y in bits 16-26), and the GP0(E5) drawing offset is
    /// added to every vertex — so the same display list draws at different places just by changing the
    /// offset. Sign-extend before adding, or a vertex meant to sit slightly left of origin lands ~2048
    /// pixels to the right instead.
    fn decode_vertex(&self, word: u32) -> (i32, i32) {
        let x = sign_extend_11(word & 0x7FF) as i32 + self.draw_offset_x as i32;
        let y = sign_extend_11((word >> 16) & 0x7FF) as i32 + self.draw_offset_y as i32;
        (x, y)
    }

    /// Fetch one raw 16-bit texel for a textured primitive. First the GP0(E2) **texture window** folds
    /// U/V into a sub-tile (so a small texture can repeat); then VRAM is read per the colour depth.
    /// **`vram_get` already wraps the 1024x512 torus**, which *is* the hardware's texture-overflow
    /// behaviour, so no extra masking is needed. For 4/8bpp the fetched halfword is an *index* into the
    /// CLUT; for 15bpp it's the colour directly. Returns the raw texel — the caller does the
    /// black-is-transparent test and the colour modulation.
    fn sample_texel(&self, u: i32, v: i32, tex: &TexInfo) -> u16 {
        // U/V are **8-bit** texture coordinates that wrap within the 256x256 texture page — mask to a
        // byte first. This is what makes a flipped sprite (whose U/V step *downward* from the base,
        // i.e. negative) wrap around and read the page in reverse, instead of running off into the
        // VRAM to the left of the page.
        let (u, v) = (u & 0xFF, v & 0xFF);
        // Texture window: mask/offset in 8-pixel units. `u = (u & ~(mask*8)) | ((offset & mask)*8)`
        // clears the masked high bits then forces the offset in — i.e. wraps U/V within a sub-tile.
        let tw = self.tex_window;
        let (mask_x, mask_y) = ((tw & 0x1F) as i32, ((tw >> 5) & 0x1F) as i32);
        let (off_x, off_y) = (((tw >> 10) & 0x1F) as i32, ((tw >> 15) & 0x1F) as i32);
        let u = (u & !(mask_x << 3)) | ((off_x & mask_x) << 3);
        let v = (v & !(mask_y << 3)) | ((off_y & mask_y) << 3);

        match tex.depth {
            0 => {
                // 4bpp: 4 texels per halfword; the low 2 bits of U pick the nibble.
                let hw = self.vram_get(tex.tex_x + (u >> 2), tex.tex_y + v);
                let idx = (hw >> ((u & 3) * 4)) & 0xF;
                self.vram_get(tex.clut_x + idx as i32, tex.clut_y)
            }
            1 => {
                // 8bpp: 2 texels per halfword; bit 0 of U picks the byte.
                let hw = self.vram_get(tex.tex_x + (u >> 1), tex.tex_y + v);
                let idx = (hw >> ((u & 1) * 8)) & 0xFF;
                self.vram_get(tex.clut_x + idx as i32, tex.clut_y)
            }
            _ => self.vram_get(tex.tex_x + u, tex.tex_y + v), // 15bpp direct colour
        }
    }

    // ===== the four ports the bus routes to =============================================

    /// `GPUSTAT` (`0x1F801814` read) — assembled from the live register state above. Until M4a this
    /// was a constant `0x1C000000`; the BIOS now drives it through a real reset + poll path.
    pub fn status(&self) -> u32 {
        let mut s = 0u32;

        // Bits 0-10 come verbatim from the GP0(E1) draw-mode word (texpage / semi-transparency /
        // texture depth / dither / draw-to-display), and bit 15 is the texture-disable flag. The
        // GP1(09) "allowed" gate is applied when bit 11 is *written* (see `gp1`/`draw_polygon`), so a
        // value set while allowed survives a later un-allow — hence we surface it straight from here.
        s |= self.draw_mode & 0x7FF;
        s |= ((self.draw_mode >> 11) & 1) << 15;

        // Bits 11/12 — the GP0(E6) mask behaviour.
        s |= (self.force_set_mask as u32) << 11;
        s |= (self.check_mask as u32) << 12;

        // Bit 14 ("reverse") and bits 16-22 come from the GP1(08) display-mode byte, but scattered:
        // the hardware doesn't lay them out in the same order, so we re-place each field by hand.
        let dm = self.display_mode;
        s |= ((dm >> 7) & 1) << 14; // reverse flag
        s |= ((dm >> 6) & 1) << 16; // horizontal resolution 2 (368-pixel mode)
        s |= ((dm >> 0) & 3) << 17; // horizontal resolution 1 (256/320/512/640)
        s |= ((dm >> 2) & 1) << 19; // vertical resolution (240/480)
        s |= ((dm >> 3) & 1) << 20; // video mode (NTSC/PAL)
        s |= ((dm >> 4) & 1) << 21; // display colour depth (15/24-bit)
        s |= ((dm >> 5) & 1) << 22; // vertical interlace

        // Bit 23 — display blanked (1 = disabled). Bit 24 — the GPU's interrupt request.
        s |= (self.display_disabled as u32) << 23;
        s |= (self.irq as u32) << 24;

        // Bits 26/27/28 — "ready for a command word / to send VRAM / to receive a DMA block."
        // We execute every command synchronously the instant its last word arrives, so the GPU is
        // *always* ready; forcing these high is what lets a polling BIOS fall straight through (this
        // is the one behaviour we carry over from the M0-M3 stub's 0x1C000000).
        s |= 1 << 26;
        s |= 1 << 27;
        s |= 1 << 28;

        // Bit 25 — the DMA / data request line. **Gotcha:** its meaning depends on the GP1(04) DMA
        // direction, and the DMA controller (M4c) polls it to decide whether the GPU wants a block.
        // If we left it 0 the GPU-DMA channel would never fire. Mirror the matching ready bit per
        // direction: FIFO -> cmd-ready (26), CPU->GP0 -> dma-block-ready (28), GPUREAD->CPU ->
        // vram-send-ready (27); "off" requests nothing.
        let dma_req = match self.dma_direction {
            1 => (s >> 26) & 1,
            2 => (s >> 28) & 1,
            3 => (s >> 27) & 1,
            _ => 0,
        };
        s |= dma_req << 25;

        // Bits 29-30 echo the DMA direction back. Bit 31 (drawing even/odd interlace line) is driven
        // by the display timing in M4e; left 0 here, exactly as the M3 stub left it (boot doesn't
        // depend on it).
        s |= (self.dma_direction as u32) << 29;

        s
    }

    /// `GPUREAD` (`0x1F801810` read). While a VRAM->CPU (`C0`) transfer is active this streams the
    /// source rectangle back, two pixels per word; otherwise it returns the staged GP1(10) "GPU info"
    /// response.
    ///
    /// **Gotcha:** this read *mutates* — each call advances the transfer cursor — yet it has to stay
    /// `&self`, because the CPU reaches it through `bus.read32 -> io_read -> gpu.read`, all `&self`.
    /// So `dl_px` is a `Cell`: the cursor advances through a shared reference while VRAM itself is only
    /// *read* here (never written), which keeps the whole CPU read path untouched.
    pub fn read(&self) -> u32 {
        let total = self.dl_w as u32 * self.dl_h as u32;
        let px = self.dl_px.get();
        if px >= total {
            return self.gpuread; // no transfer in flight -> the GPU-info value
        }
        let mut word = 0u32;
        let mut sent = px;
        for half in 0..2 {
            if sent >= total {
                break; // odd final pixel: only the low half is valid
            }
            let x = self.dl_x as i32 + (sent % self.dl_w as u32) as i32;
            let y = self.dl_y as i32 + (sent / self.dl_w as u32) as i32;
            word |= (self.vram_get(x, y) as u32) << (half * 16);
            sent += 1;
        }
        self.dl_px.set(sent);
        word
    }

    /// GP0 (`0x1F801810` write) — the drawing / data port. This is the FIFO state machine described
    /// in the module header: accumulate a command's words, then dispatch it.
    pub fn gp0(&mut self, word: u32) {
        // --- mid image upload: this word is two pixels for the CPU->VRAM (`A0`) rectangle ----------
        // A `A0` command is a 3-word header followed by width*height pixels packed two to a word (low
        // half first). While that data stream is flowing these words are pixel data, not commands:
        // write them across the destination rectangle, left to right then top to bottom, advancing the
        // upload cursor. A final word with an odd pixel left over writes one and ignores its high half.
        // (Each pixel carries its own mask bit in data bit 15 — written as-is by `vram_set`.)
        if self.gp0_img_words > 0 {
            let total = self.up_w as u32 * self.up_h as u32;
            for half in 0..2 {
                if self.up_px >= total {
                    break;
                }
                let pixel = (word >> (half * 16)) as u16; // low half, then high half
                let x = self.up_x as i32 + (self.up_px % self.up_w as u32) as i32;
                let y = self.up_y as i32 + (self.up_px / self.up_w as u32) as i32;
                self.vram_set(x, y, pixel);
                self.up_px += 1;
            }
            self.gp0_img_words -= 1;
            return;
        }

        // --- mid polyline: draw each segment as its far endpoint arrives -----------------------
        // A polyline (`48`-`5F` with bit 3 set) sends an arbitrary number of points and ends with a
        // sentinel word matching `0x5xxx_5xxx`. Its length isn't known up front, so it gets its own
        // drain mode instead of a `gp0_remaining` count. We keep only the *previous* point/colour and
        // emit a line the moment the next point completes — no need to buffer the whole list.
        if self.gp0_polyline {
            // Gouraud polylines interleave a colour word then a vertex word per point, and the
            // terminator sits where the *next colour* would — so only test for it when a colour is
            // expected. Flat polylines are all vertices, any of which may be the terminator.
            if self.poly_is_gouraud && self.poly_expect_color {
                if word & 0xF000_F000 == 0x5000_5000 {
                    self.gp0_polyline = false;
                    return;
                }
                self.poly_pending_col = rgb_channels(word);
                self.poly_expect_color = false;
                return;
            }
            if word & 0xF000_F000 == 0x5000_5000 {
                self.gp0_polyline = false;
                return;
            }
            let cur = self.decode_vertex(word);
            let cur_col = self.poly_pending_col;
            if self.poly_have_prev {
                let dither = self.poly_is_gouraud && (self.draw_mode >> 9) & 1 != 0;
                self.draw_line(
                    self.poly_prev_pt,
                    cur,
                    self.poly_prev_col,
                    cur_col,
                    self.poly_is_gouraud,
                    dither,
                );
            }
            self.poly_prev_pt = cur;
            self.poly_prev_col = cur_col;
            self.poly_have_prev = true;
            if self.poly_is_gouraud {
                self.poly_expect_color = true; // next word is this point's successor's colour
            }
            return;
        }

        if self.gp0_remaining == 0 {
            // --- first word of a new command: decode it and learn the command's length ----------
            let cmd = (word >> 24) as u8;
            self.gp0_command = cmd;
            self.gp0_buffer[0] = word;
            self.gp0_len = 1;

            // Polylines are variable-length: switch to drain-until-terminator and stop here. Seed the
            // segment-render state from the command word — vertex 0's colour and the shading mode —
            // so the drain above can draw each segment as the next point arrives.
            if (0x40..=0x5F).contains(&cmd) && cmd & 0x08 != 0 {
                self.gp0_polyline = true;
                self.poly_is_gouraud = cmd & 0x10 != 0;
                self.poly_pending_col = rgb_channels(word);
                self.poly_prev_col = self.poly_pending_col;
                self.poly_have_prev = false;
                self.poly_expect_color = false; // the first drained word is vertex 0
                return;
            }

            // Otherwise the command has a fixed word count. Subtract the word we just took; if that
            // leaves nothing, it was a one-word command (e.g. a state setting) — run it now.
            self.gp0_remaining = gp0_command_len(cmd) - 1;
            if self.gp0_remaining == 0 {
                self.execute_gp0();
            }
        } else {
            // --- a continuation word: append, and fire once the command is complete -------------
            if self.gp0_len < self.gp0_buffer.len() {
                self.gp0_buffer[self.gp0_len] = word;
            }
            self.gp0_len += 1;
            self.gp0_remaining -= 1;
            if self.gp0_remaining == 0 {
                self.execute_gp0();
            }
        }
    }

    /// GP1 (`0x1F801814` write) — display control. Each command is a single word: the top byte
    /// selects the knob, the low 24 bits are its parameter.
    pub fn gp1(&mut self, word: u32) {
        let cmd = (word >> 24) as u8;
        let param = word & 0x00FF_FFFF;
        match cmd {
            0x00 => self.reset(),         // full GPU reset
            0x01 => self.reset_fifo(),    // reset the command FIFO only
            0x02 => self.irq = false,     // acknowledge (clear) the GPU IRQ
            0x03 => self.display_disabled = param & 1 != 0, // display on/off
            0x04 => self.dma_direction = (param & 3) as u8, // DMA direction
            0x05 => {
                // Start of the display area in VRAM.
                self.display_vram_x = (param & 0x3FF) as u16;
                self.display_vram_y = ((param >> 10) & 0x1FF) as u16;
            }
            0x06 => {
                // Horizontal display range (in GPU dot-clock units).
                self.display_h1 = (param & 0xFFF) as u16;
                self.display_h2 = ((param >> 12) & 0xFFF) as u16;
            }
            0x07 => {
                // Vertical display range (in scanlines).
                self.display_v1 = (param & 0x3FF) as u16;
                self.display_v2 = ((param >> 10) & 0x3FF) as u16;
            }
            0x08 => self.display_mode = param & 0xFF, // resolution / video mode / depth / interlace
            0x09 => self.texture_disable_allowed = param & 1 != 0, // allow the texture-disable bit
            0x10 => self.gpu_info(param),             // stage a GPUREAD info response
            _ => {} // 0x20 (GPU type) etc. — not needed yet
        }
    }

    // ===== GP1 helpers ==================================================================

    /// GP1(00): reset the GPU to its blanked, DMA-off, cleared-state baseline. The BIOS issues this
    /// early in boot, so it doubles as our "known good starting point".
    fn reset(&mut self) {
        self.reset_fifo();
        self.irq = false;
        self.display_disabled = true;
        self.dma_direction = 0;
        self.texture_disable_allowed = false;
        self.draw_mode = 0;
        self.tex_window = 0;
        self.draw_area_left = 0;
        self.draw_area_top = 0;
        self.draw_area_right = 0;
        self.draw_area_bottom = 0;
        self.draw_offset_x = 0;
        self.draw_offset_y = 0;
        self.force_set_mask = false;
        self.check_mask = false;
        self.display_vram_x = 0;
        self.display_vram_y = 0;
        self.display_h1 = 0;
        self.display_h2 = 0;
        self.display_v1 = 0;
        self.display_v2 = 0;
        self.display_mode = 0;
    }

    /// GP1(01): drop whatever half-finished command was in the FIFO. (Doesn't touch draw/display
    /// state, unlike the full reset above.)
    fn reset_fifo(&mut self) {
        self.gp0_remaining = 0;
        self.gp0_len = 0;
        self.gp0_img_words = 0;
        self.gp0_polyline = false;
    }

    /// GP1(10): stage the answer to a "GPU info" query into GPUREAD. Only the handful of requests a
    /// booting kernel actually issues are modelled; an unrecognised request leaves the last value in
    /// place, which is what the hardware does.
    fn gpu_info(&mut self, param: u32) {
        match param & 0xFF {
            0x02 => self.gpuread = self.tex_window,
            0x03 => {
                self.gpuread =
                    (self.draw_area_left as u32) | ((self.draw_area_top as u32) << 10);
            }
            0x04 => {
                self.gpuread =
                    (self.draw_area_right as u32) | ((self.draw_area_bottom as u32) << 10);
            }
            0x05 => {
                self.gpuread = ((self.draw_offset_x as u32) & 0x7FF)
                    | (((self.draw_offset_y as u32) & 0x7FF) << 11);
            }
            0x07 => self.gpuread = 2, // GPU type — the later CXD-series GPUs report 2
            0x08 => self.gpuread = 0,
            _ => {}
        }
    }

    // ===== GP0 dispatch =================================================================

    /// Run a fully-gathered GP0 command. M4a executes the *state* settings (E1-E6) and a couple of
    /// trivia commands; every drawing / transfer command is recognised and its parameters consumed
    /// (so the FIFO stays aligned) but its pixel work is deferred to the stage that owns it.
    fn execute_gp0(&mut self) {
        let cmd = self.gp0_command;
        let w0 = self.gp0_buffer[0];

        match cmd {
            0x00 => {}                  // NOP
            0x01 => {}                  // clear texture cache — nothing to model (no cache yet)
            0x02 => {
                // Fill rectangle: a fast block fill in a flat colour. **Gotcha: fill has its OWN
                // coordinate rules** — X and width snap to 16-pixel units (X &= 0x3F0, width rounds
                // *up* to a multiple of 16), Y/height mask to the VRAM height — and it **ignores** the
                // mask bit and semi-transparency, unlike a *drawn* rectangle. The 24-bit command
                // colour is truncated to 15-bit (mask bit left 0).
                let pos = self.gp0_buffer[1];
                let size = self.gp0_buffer[2];
                let fx = (pos & 0x3F0) as i32;
                let fy = ((pos >> 16) & 0x1FF) as i32;
                let fw = (((size & 0x3FF) + 0x0F) & !0x0F) as i32;
                let fh = ((size >> 16) & 0x1FF) as i32;
                let color = rgb24_to_15(w0);
                for yy in 0..fh {
                    for xx in 0..fw {
                        self.vram_set(fx + xx, fy + yy, color);
                    }
                }
            }
            0x1F => self.irq = true,    // request a GPU interrupt (GPUSTAT bit 24)

            // --- draw-state settings (fully modelled now; the rasterizer reads these in M4d) -----
            0xE1 => {
                // Draw mode: texpage / semi-transp / depth / dither / flips. **Bit 11 (texture-
                // disable) is special:** it can only be *set* while GP1(09) has allowed it; when not
                // allowed, a write can still clear it but not set it (and an existing 1 is preserved).
                let new = w0 & 0x3FFF;
                let b11 = if self.texture_disable_allowed { new & 0x800 } else { new & self.draw_mode & 0x800 };
                self.draw_mode = (new & !0x800) | b11;
            }
            0xE2 => self.tex_window = w0 & 0x000F_FFFF,
            0xE3 => {
                self.draw_area_left = (w0 & 0x3FF) as u16;
                self.draw_area_top = ((w0 >> 10) & 0x3FF) as u16;
            }
            0xE4 => {
                self.draw_area_right = (w0 & 0x3FF) as u16;
                self.draw_area_bottom = ((w0 >> 10) & 0x3FF) as u16;
            }
            0xE5 => {
                // Drawing offset. **Gotcha: each field is a *signed* 11-bit value**, so a primitive
                // can be nudged left/up (negative). Sign-extend from bit 10 — a plain mask would turn
                // every negative offset into a large positive one and shove primitives off-screen.
                self.draw_offset_x = sign_extend_11(w0 & 0x7FF);
                self.draw_offset_y = sign_extend_11((w0 >> 11) & 0x7FF);
            }
            0xE6 => {
                self.force_set_mask = w0 & 1 != 0;
                self.check_mask = w0 & 2 != 0;
            }

            // --- rendering primitives (M4d-1: untextured) --------------------------------------
            0x20..=0x3F => self.draw_polygon(cmd),
            0x40..=0x5F => self.draw_gp0_line(cmd), // non-polyline; polylines render in `gp0`'s drain
            0x60..=0x7F => self.draw_gp0_rect(cmd),

            // --- VRAM transfers (M4b) -----------------------------------------------------------
            0x80..=0x9F => {
                // VRAM -> VRAM block copy: source, destination, size. **Gotcha: source and
                // destination may overlap** (the `vram-to-vram-overlap` test exists for exactly this);
                // we copy top-to-bottom, left-to-right with VRAM coordinate wrap.
                let src = self.gp0_buffer[1];
                let dst = self.gp0_buffer[2];
                let (w, h) = transfer_size(self.gp0_buffer[3]);
                let (sx, sy) = ((src & 0x3FF) as i32, ((src >> 16) & 0x1FF) as i32);
                let (dx, dy) = ((dst & 0x3FF) as i32, ((dst >> 16) & 0x1FF) as i32);
                for yy in 0..h as i32 {
                    for xx in 0..w as i32 {
                        let p = self.vram_get(sx + xx, sy + yy);
                        self.vram_set(dx + xx, dy + yy, p);
                    }
                }
            }
            0xA0..=0xBF => {
                // CPU -> VRAM upload: the 3-word header (command, destination, size) is in. Latch the
                // destination rectangle + pixel cursor, and set `gp0_img_words` so the following data
                // words route into the image branch in `gp0` (two pixels per word, rounded up).
                let dst = self.gp0_buffer[1];
                let (w, h) = transfer_size(self.gp0_buffer[2]);
                self.up_x = (dst & 0x3FF) as u16;
                self.up_y = ((dst >> 16) & 0x1FF) as u16;
                self.up_w = w as u16;
                self.up_h = h as u16;
                self.up_px = 0;
                self.gp0_img_words = ((w * h + 1) / 2) as usize;
            }
            0xC0..=0xDF => {
                // VRAM -> CPU download: latch the source rectangle and reset the read cursor; the data
                // is pulled out word-by-word through GPUREAD (`read`), not pushed from here.
                let src = self.gp0_buffer[1];
                let (w, h) = transfer_size(self.gp0_buffer[2]);
                self.dl_x = (src & 0x3FF) as u16;
                self.dl_y = ((src >> 16) & 0x1FF) as u16;
                self.dl_w = w as u16;
                self.dl_h = h as u16;
                self.dl_px.set(0);
            }

            _ => {} // anything else: treated as a 1-word no-op
        }

        // Ready for the next command.
        self.gp0_remaining = 0;
        self.gp0_len = 0;
    }

    // ===== rendering primitives (M4d-1) =================================================

    /// Decode and draw a polygon command (0x20-0x3F). The command byte's bits pick the shape:
    /// bit4 = Gouraud (per-vertex colour), bit3 = quad (4 vertices, drawn as two triangles), bit2 =
    /// textured. Textured polygons interpolate U/V across the face and sample VRAM; the CLUT and
    /// texture page come from the first two vertices' U/V words.
    fn draw_polygon(&mut self, cmd: u8) {
        let gouraud = cmd & 0x10 != 0;
        let quad = cmd & 0x08 != 0;
        let textured = cmd & 0x04 != 0;
        let verts = if quad { 4 } else { 3 };

        // Walk `gp0_buffer` exactly the way `poly_len` counted it, pulling out each vertex's position,
        // colour, and (when textured) U/V. Vertex 0's colour is in the command word; later vertices
        // bring their own only when Gouraud. The U/V word's high half carries the CLUT on vertex 0 and
        // the texpage on vertex 1.
        let mut pts = [(0i32, 0i32); 4];
        let mut cols = [(0i32, 0i32, 0i32); 4];
        let mut uvs = [(0i32, 0i32); 4];
        let (mut clut, mut texpage) = (0u32, 0u32);
        let mut idx = 1usize;
        for v in 0..verts {
            if v == 0 {
                cols[0] = rgb_channels(self.gp0_buffer[0]);
            } else if gouraud {
                cols[v] = rgb_channels(self.gp0_buffer[idx]);
                idx += 1;
            } else {
                cols[v] = cols[0];
            }
            pts[v] = self.decode_vertex(self.gp0_buffer[idx]);
            idx += 1;
            if textured {
                let w = self.gp0_buffer[idx];
                uvs[v] = ((w & 0xFF) as i32, ((w >> 8) & 0xFF) as i32);
                match v {
                    0 => clut = w >> 16,
                    1 => texpage = w >> 16,
                    _ => {}
                }
                idx += 1;
            }
        }

        // A textured polygon always **latches** its texpage into the persistent draw mode — bits 0-8
        // (page/semi-transp/depth) and bit 11 (texture-disable), the bits the texpage word carries.
        // Bits 9-10 (dither, draw-to-display) and 12-13 (rect flips) stay E1-owned. Texturing is only
        // *disabled* when that bit 11 is set AND GP1(09) has allowed it; otherwise we texture normally.
        let tex = if textured {
            // Latch bits 0-8 (page/depth/semi-transp) always; bit 11 (texture-disable) is gated by
            // GP1(09) exactly like the E1 write. Bits 9-10/12-13 stay E1-owned.
            let b11 = if self.texture_disable_allowed {
                texpage & 0x800
            } else {
                texpage & self.draw_mode & 0x800
            };
            self.draw_mode = (self.draw_mode & !0x9FF) | (texpage & 0x1FF) | b11;
            if b11 != 0 {
                None
            } else {
                let (tex_x, tex_y) = texpage_base(texpage);
                let (clut_x, clut_y) = clut_base(clut);
                Some(TexInfo { tex_x, tex_y, clut_x, clut_y, depth: tex_depth(texpage) })
            }
        } else {
            None
        };

        // Dithering applies to Gouraud and to textured pixels when GP0(E1) bit 9 is set.
        let dither = (gouraud || textured) && (self.draw_mode >> 9) & 1 != 0;

        // A quad is two triangles sharing the v1-v2 edge; colour AND U/V split the same way, so the
        // texture is continuous across the diagonal.
        self.draw_triangle(
            [pts[0], pts[1], pts[2]],
            [cols[0], cols[1], cols[2]],
            [uvs[0], uvs[1], uvs[2]],
            tex,
            gouraud,
            dither,
        );
        if quad {
            self.draw_triangle(
                [pts[1], pts[2], pts[3]],
                [cols[1], cols[2], cols[3]],
                [uvs[1], uvs[2], uvs[3]],
                tex,
                gouraud,
                dither,
            );
        }
    }

    /// Decode and draw a non-polyline line (0x40-0x5F without the polyline bit): flat (3 words) or
    /// Gouraud (4 words). Polylines are open-ended and render in `gp0`'s drain instead.
    fn draw_gp0_line(&mut self, cmd: u8) {
        let gouraud = cmd & 0x10 != 0;
        let ca = rgb_channels(self.gp0_buffer[0]);
        let a = self.decode_vertex(self.gp0_buffer[1]);
        let (cb, b) = if gouraud {
            (rgb_channels(self.gp0_buffer[2]), self.decode_vertex(self.gp0_buffer[3]))
        } else {
            (ca, self.decode_vertex(self.gp0_buffer[2]))
        };
        let dither = gouraud && (self.draw_mode >> 9) & 1 != 0;
        self.draw_line(a, b, ca, cb, gouraud, dither);
    }

    /// Decode and draw a rectangle/sprite (0x60-0x7F). Flat untextured = a solid fill; textured =
    /// a sprite sampled from VRAM. Size comes from cmd bits 4-3: `00` = an explicit size word follows,
    /// `01` = 1x1, `10` = 8x8, `11` = 16x16.
    fn draw_gp0_rect(&mut self, cmd: u8) {
        let textured = cmd & 0x04 != 0;
        let (px, py) = self.decode_vertex(self.gp0_buffer[1]);
        // Layout after the position word: [U/V (+CLUT) if textured] then [size word if variable].
        // Width/height are taken **raw** — not via `transfer_size`, a VRAM-transfer-only rule.
        let size_word = self.gp0_buffer[2 + textured as usize];
        let (w, h) = match (cmd >> 3) & 3 {
            0 => ((size_word & 0xFFFF) as i32, (size_word >> 16) as i32),
            1 => (1, 1),
            2 => (8, 8),
            _ => (16, 16),
        };

        // draw_mode bit 11 already reflects the GP1(09) gate (applied at write time), so it alone
        // tells us whether texturing is disabled for this sprite.
        let tex_disabled = (self.draw_mode >> 11) & 1 != 0;
        if textured && !tex_disabled {
            // A sprite takes its texpage from the *current* GP0(E1) draw mode (no texpage word) and
            // its CLUT from the high half of the U/V word; the low half is the top-left texel.
            let uvword = self.gp0_buffer[2];
            let (clut_x, clut_y) = clut_base(uvword >> 16);
            let (tex_x, tex_y) = texpage_base(self.draw_mode);
            let tex = TexInfo { tex_x, tex_y, clut_x, clut_y, depth: tex_depth(self.draw_mode) };
            let base_u = (uvword & 0xFF) as i32;
            let base_v = ((uvword >> 8) & 0xFF) as i32;
            let xflip = (self.draw_mode >> 12) & 1 != 0; // GP0(E1) bit 12 — mirror horizontally
            let yflip = (self.draw_mode >> 13) & 1 != 0; // GP0(E1) bit 13 — mirror vertically
            let col = rgb_channels(self.gp0_buffer[0]);
            self.draw_tex_rect(px, py, w, h, base_u, base_v, &tex, xflip, yflip, col);
        } else {
            let color = rgb24_to_15(self.gp0_buffer[0]);
            self.draw_rect(px, py, w, h, color);
        }
    }

    /// Draw a textured sprite: U/V step **1:1 with screen pixels** (no interpolation — sprites aren't
    /// projected), reversed by the X/Y flip bits. Each texel is sampled, skipped if fully black,
    /// modulated by the command colour, and plotted (so scissor + mask apply). Sprites aren't dithered.
    #[allow(clippy::too_many_arguments)]
    fn draw_tex_rect(
        &mut self,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        base_u: i32,
        base_v: i32,
        tex: &TexInfo,
        xflip: bool,
        yflip: bool,
        col: (i32, i32, i32),
    ) {
        for dy in 0..h {
            // Flip walks the texel coords *downward* from the base; `sample_texel` masks U/V to 8 bits,
            // so they wrap and read the 256-texel page in reverse. **Gotcha:** X-flip carries a
            // 1-texel offset the Y axis doesn't — a real PS1 textured-rectangle quirk, pinned exactly
            // against the `gpu/texture-flip` reference.
            let v = base_v + if yflip { -dy } else { dy };
            for dx in 0..w {
                let u = base_u + if xflip { 1 - dx } else { dx };
                let texel = self.sample_texel(u, v, tex); // &self read into a local ...
                if texel == 0 {
                    continue; // fully-black texel is transparent
                }
                let out = modulate(texel, col.0, col.1, col.2, x + dx, y + dy, false);
                self.plot(x + dx, y + dy, out); // ... then the &mut self write
            }
        }
    }

    /// Rasterize one triangle (quads arrive as two of these). Flat primitives pass the same colour in
    /// all three slots with `gouraud=false`; Gouraud primitives interpolate the three vertex colours
    /// across the face. We use the **edge-function / half-space** method: `edge(a,b,p)` is twice the
    /// signed area of triangle (a,b,p) — positive on one side of the directed edge a->b, negative on
    /// the other — so a point is inside when it's on the correct side of all three edges. Those same
    /// edge values are the barycentric weights, so Gouraud shading falls out of the inside test.
    ///
    /// **Pixel-exactness (derived from the test ROM reference images, not primary text):** sample at
    /// integer pixel coordinates; apply a **top-left fill rule** so a shared edge (a quad's diagonal)
    /// is owned by exactly one triangle; draw **either winding** (no back-face culling); and drop any
    /// primitive spanning more than 1023 across or 511 down (the hardware's size limit).
    fn draw_triangle(
        &mut self,
        mut v: [(i32, i32); 3],
        mut col: [(i32, i32, i32); 3],
        mut uv: [(i32, i32); 3],
        tex: Option<TexInfo>,
        gouraud: bool,
        dither: bool,
    ) {
        let xs = [v[0].0, v[1].0, v[2].0];
        let ys = [v[0].1, v[1].1, v[2].1];
        let (min_x, max_x) = (*xs.iter().min().unwrap(), *xs.iter().max().unwrap());
        let (min_y, max_y) = (*ys.iter().min().unwrap(), *ys.iter().max().unwrap());
        if max_x - min_x > 1023 || max_y - min_y > 511 {
            return; // oversized primitive — the GPU drops it
        }

        // Signed doubled area: 0 = degenerate (a line or point), nothing to fill. Negative = the other
        // winding; swap two vertices (and their colours) so the area is positive and the fill rule has
        // a consistent orientation to reason about.
        let area2 = edge(v[0], v[1], v[2]);
        if area2 == 0 {
            return;
        }
        if area2 < 0 {
            v.swap(1, 2);
            col.swap(1, 2);
            uv.swap(1, 2); // U/V must follow the same swap as positions, or textures shear
        }
        let area2 = edge(v[0], v[1], v[2]); // now > 0

        // Iterate only the bounding box, already intersected with the scissor so we skip clipped rows.
        let lo_x = min_x.max(self.draw_area_left as i32);
        let hi_x = max_x.min(self.draw_area_right as i32);
        let lo_y = min_y.max(self.draw_area_top as i32);
        let hi_y = max_y.min(self.draw_area_bottom as i32);

        let flat = shade(0, 0, col[0].0, col[0].1, col[0].2, false); // constant for flat — compute once

        for y in lo_y..=hi_y {
            for x in lo_x..=hi_x {
                let p = (x, y);
                let w0 = edge(v[1], v[2], p);
                let w1 = edge(v[2], v[0], p);
                let w2 = edge(v[0], v[1], p);
                if !(inside_edge(w0, v[1], v[2])
                    && inside_edge(w1, v[2], v[0])
                    && inside_edge(w2, v[0], v[1]))
                {
                    continue;
                }
                // Barycentric blend (weights w0/w1/w2 sum to area2; i64 keeps `w*channel` safe;
                // round-to-nearest via the +half bias). Used for the Gouraud colour and, when
                // textured, for U/V — same weights, so the texture follows the shape exactly.
                let half = area2 / 2;
                let bary = |a0: i32, a1: i32, a2: i32| {
                    ((w0 * a0 as i64 + w1 * a1 as i64 + w2 * a2 as i64 + half) / area2) as i32
                };
                // The colour that *modulates* the texel (or is drawn directly when untextured):
                // flat = vertex-0 colour, Gouraud = the per-pixel blend.
                let (cr, cg, cb) = if gouraud {
                    (
                        bary(col[0].0, col[1].0, col[2].0),
                        bary(col[0].1, col[1].1, col[2].1),
                        bary(col[0].2, col[1].2, col[2].2),
                    )
                } else {
                    col[0]
                };

                if let Some(t) = tex {
                    let u = bary(uv[0].0, uv[1].0, uv[2].0);
                    let vv = bary(uv[0].1, uv[1].1, uv[2].1);
                    let texel = self.sample_texel(u, vv, &t); // &self read, into a local ...
                    if texel == 0 {
                        continue; // a fully-black texel is transparent — leave VRAM untouched
                    }
                    let out = modulate(texel, cr, cg, cb, x, y, dither);
                    self.plot(x, y, out); // ... then the &mut self write
                } else {
                    let color = if gouraud { shade(x, y, cr, cg, cb, dither) } else { flat };
                    self.plot(x, y, color);
                }
            }
        }
    }

    /// Fill an axis-aligned rectangle / sprite in a flat colour (no shading, no dither). Honours the
    /// scissor and mask via `plot`.
    fn draw_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: u16) {
        for yy in 0..h {
            for xx in 0..w {
                self.plot(x + xx, y + yy, color);
            }
        }
    }

    /// Draw a line segment with integer Bresenham. Flat lines use endpoint `a`'s colour throughout;
    /// Gouraud lines interpolate `ca`->`cb` along the segment (dithered when enabled). Endpoints arrive
    /// pre-offset; scissor + mask come from `plot`; the same 1023/511 span limit applies.
    fn draw_line(
        &mut self,
        a: (i32, i32),
        b: (i32, i32),
        ca: (i32, i32, i32),
        cb: (i32, i32, i32),
        gouraud: bool,
        dither: bool,
    ) {
        if (a.0 - b.0).abs() > 1023 || (a.1 - b.1).abs() > 511 {
            return;
        }
        let flat = shade(0, 0, ca.0, ca.1, ca.2, false);
        let dx = (b.0 - a.0).abs();
        let dy = -(b.1 - a.1).abs();
        let sx = if a.0 < b.0 { 1 } else { -1 };
        let sy = if a.1 < b.1 { 1 } else { -1 };
        let steps = dx.max(-dy).max(1) as i64; // dominant-axis length, for colour parametrization
        let mut err = dx + dy;
        let (mut x, mut y) = a;
        loop {
            let color = if gouraud {
                // Fraction along the segment by dominant-axis progress, round-to-nearest (same as the
                // triangle barycentric — the PS1 rounds rather than truncates). The numerator can be
                // negative (colour decreasing), so bias by half a step toward whichever way it points.
                let t = (x - a.0).abs().max((y - a.1).abs()) as i64;
                let lerp = |c0: i32, c1: i32| {
                    let num = (c1 - c0) as i64 * t;
                    let rounded = if num >= 0 {
                        (num + steps / 2) / steps
                    } else {
                        -((-num + steps / 2) / steps)
                    };
                    (c0 as i64 + rounded) as i32
                };
                shade(x, y, lerp(ca.0, cb.0), lerp(ca.1, cb.1), lerp(ca.2, cb.2), dither)
            } else {
                flat
            };
            self.plot(x, y, color);
            if x == b.0 && y == b.1 {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x += sx;
            }
            if e2 <= dx {
                err += dx;
                y += sy;
            }
        }
    }
}

// ===== free helpers =====================================================================

/// The PS1's ordered 4x4 dither matrix (signed). When dithering is enabled — GP0(E1) bit 9 — for a
/// Gouraud or textured pixel, the offset at `[y & 3][x & 3]` is added to each 8-bit colour channel
/// *before* it's truncated to 5 bits. That trades a little spatial noise for the *appearance* of more
/// colour depth, so smoothly-shaded surfaces don't band when squeezed from ~24-bit into 15-bit VRAM.
const DITHER: [[i32; 4]; 4] = [
    [-4, 0, -3, 1],
    [2, -2, 3, -1],
    [-3, 1, -4, 0],
    [3, -1, 2, -2],
];

/// Split a 24-bit command colour (`0x00BBGGRR`, red low) into raw 8-bit `(r, g, b)`. The rasterizer
/// carries colour as full-precision channels so Gouraud interpolation and dithering keep their low
/// bits; the squeeze to 15-bit happens once, at the end, in `shade`.
fn rgb_channels(c: u32) -> (i32, i32, i32) {
    ((c & 0xFF) as i32, ((c >> 8) & 0xFF) as i32, ((c >> 16) & 0xFF) as i32)
}

/// Pack 8-bit `(r, g, b)` channels into a 15-bit VRAM pixel (mask bit left 0). With `dither` set, the
/// ordered-dither offset for this pixel is added first; either way each channel is clamped to 0..=255
/// and its top 5 bits kept (`>> 3`) — the same top-aligned squeeze as `rgb24_to_15`/`expand5`, so flat
/// and shaded pixels land on the exact colour ramp the reference images use.
fn shade(x: i32, y: i32, r: i32, g: i32, b: i32, dither: bool) -> u16 {
    let d = if dither { DITHER[(y & 3) as usize][(x & 3) as usize] } else { 0 };
    let ch = |v: i32| ((v + d).clamp(0, 255) >> 3) as u16;
    ch(r) | (ch(g) << 5) | (ch(b) << 10)
}

/// Edge function: twice the signed area of triangle `(a, b, p)`. Its sign says which side of the
/// directed edge a->b the point p lies on; its magnitude is the opposite vertex's barycentric weight.
/// `i64` so the coordinate products never overflow.
fn edge(a: (i32, i32), b: (i32, i32), p: (i32, i32)) -> i64 {
    (b.0 - a.0) as i64 * (p.1 - a.1) as i64 - (b.1 - a.1) as i64 * (p.0 - a.0) as i64
}

/// The **top-left fill rule** for one edge a->b (triangle wound so its area is positive). A pixel
/// strictly inside (`w > 0`) always draws; a pixel exactly on the edge (`w == 0`) draws only if the
/// edge is a *top* or *left* edge. That tie-break makes two triangles sharing an edge — a quad's
/// diagonal — cover it exactly once (no seam cracks / double-draws), and matches the PS1's choice of
/// which boundary rows to keep (verified against the `gpu/clipping` reference).
///
/// **Orientation gotcha:** after winding-normalization our positive-area triangles are *clockwise in
/// screen space* (y grows downward), so a **top** edge runs left→right (`dy == 0 && dx > 0`) and a
/// **left** edge runs upward (`dy < 0`). Getting the horizontal sign backwards drops the top row of
/// every flat-topped primitive — exactly the bug `gpu/clipping` caught.
fn inside_edge(w: i64, a: (i32, i32), b: (i32, i32)) -> bool {
    if w != 0 {
        return w > 0;
    }
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    dy < 0 || (dy == 0 && dx > 0)
}

// ===== texture helpers (M4d-2) ==========================================================

/// Everything `sample_texel` needs to read one texel: where the texture page sits in VRAM, where the
/// CLUT (palette) sits, and the colour depth. Built once per textured primitive from the command's
/// texpage + CLUT words (polygons) or from GP0(E1) + the command (rectangles).
#[derive(Clone, Copy)]
struct TexInfo {
    tex_x: i32,  // texture-page base X in VRAM (halfword column)
    tex_y: i32,  // texture-page base Y in VRAM
    clut_x: i32, // palette base X
    clut_y: i32, // palette base Y
    depth: u8,   // 0 = 4bpp (16-colour CLUT), 1 = 8bpp (256-colour CLUT), 2/3 = 15bpp direct
}

/// Texture-page base in VRAM from a texpage value (the low bits of GP0(E1), or a textured polygon's
/// vertex-1 word): X in units of 64, Y in units of 256 (so Y is only ever 0 or 256).
fn texpage_base(tp: u32) -> (i32, i32) {
    (((tp & 0xF) * 64) as i32, (((tp >> 4) & 1) * 256) as i32)
}

/// Texture colour depth from a texpage value: bits 7-8 (2 and 3 both mean 15-bit direct colour).
fn tex_depth(tp: u32) -> u8 {
    ((tp >> 7) & 3) as u8
}

/// CLUT (palette) base in VRAM from a CLUT id: X in units of 16, Y in single rows.
fn clut_base(clut: u32) -> (i32, i32) {
    (((clut & 0x3F) * 16) as i32, ((clut >> 6) & 0x1FF) as i32)
}

/// Modulate a 15-bit texel by an 8-bit primitive colour and pack to a VRAM pixel. **Gotcha: the blend
/// is a 5-bit datapath** — `out5 = (tex5 * col8) >> 7`, clamped to 31 — and `0x80` (128) is the
/// neutral "leave the texel unchanged" colour (so an unshaded textured primitive uses `0x808080`).
/// The texel's mask/STP bit (15) is carried through; `plot` adds the force-mask. The dithered branch
/// (only when GP0(E1) bit 9 is set) re-expands to 8 bits and reuses `shade` — the shipped texture
/// references are all dither-off, so that path is best-effort.
fn modulate(texel: u16, r8: i32, g8: i32, b8: i32, x: i32, y: i32, dither: bool) -> u16 {
    let t = |shift: u32| ((texel >> shift) & 0x1F) as i32;
    let (tr, tg, tb) = (t(0), t(5), t(10));
    let packed = if dither {
        let m8 = |tc: i32, cc: i32| ((tc * cc) >> 7).min(31) << 3; // back to top-aligned 8-bit
        shade(x, y, m8(tr, r8), m8(tg, g8), m8(tb, b8), true)
    } else {
        let m = |tc: i32, cc: i32| ((tc * cc) >> 7).min(31) as u16;
        m(tr, r8) | (m(tg, g8) << 5) | (m(tb, b8) << 10)
    };
    packed | (texel & 0x8000)
}

/// Total number of 32-bit words in a fixed-length GP0 command, *including* the command word.
/// (Polylines are variable-length and handled separately in `gp0`, so they never reach here.)
fn gp0_command_len(cmd: u8) -> usize {
    match cmd {
        0x02 => 3,                  // fill rect: colour, top-left, size
        0x20..=0x3F => poly_len(cmd),
        0x40..=0x5F => line_len(cmd),
        0x60..=0x7F => rect_len(cmd),
        0x80..=0x9F => 4,           // VRAM->VRAM copy: cmd, src, dst, size
        0xA0..=0xBF => 3,           // CPU->VRAM header: cmd, dst, size (pixel data follows)
        0xC0..=0xDF => 3,           // VRAM->CPU header: cmd, src, size
        _ => 1,                     // 00/01/1F, the E1-E6 settings, and unknowns are one word
    }
}

/// Words in a polygon command. The command byte's bits select the shape, so the length is computed,
/// not table-looked-up: bit 4 = Gouraud (per-vertex colour), bit 3 = quad (4 vertices vs 3), bit 2 =
/// textured (each vertex also carries a U/V word).
fn poly_len(cmd: u8) -> usize {
    let gouraud = cmd & 0x10 != 0;
    let quad = cmd & 0x08 != 0;
    let textured = cmd & 0x04 != 0;
    let verts = if quad { 4 } else { 3 };

    // The command word itself carries the first colour, so it counts as vertex 0's colour.
    let mut n = 1;
    for v in 0..verts {
        if gouraud && v != 0 {
            n += 1; // later vertices each bring their own colour word
        }
        n += 1; // the XY word
        if textured {
            n += 1; // the U/V (+CLUT/texpage) word
        }
    }
    n
}

/// Words in a (non-polyline) line: 3 for flat (colour + 2 vertices), 4 for Gouraud (each endpoint
/// brings its own colour).
fn line_len(cmd: u8) -> usize {
    if cmd & 0x10 != 0 { 4 } else { 3 }
}

/// Words in a rectangle/sprite command: colour word, then the position word; +1 if textured (U/V +
/// CLUT), +1 more if it's the variable-size form (bits 4-3 == 00) that carries an explicit size word.
fn rect_len(cmd: u8) -> usize {
    let textured = cmd & 0x04 != 0;
    let variable_size = (cmd >> 3) & 3 == 0;
    1 + 1 + textured as usize + variable_size as usize
}

/// Sign-extend an 11-bit value (the GP0(E5) drawing-offset field) to a signed 16-bit integer.
/// Shifting left to put bit 10 in the sign position, then arithmetic-shifting back, does the
/// extension without a branch.
fn sign_extend_11(v: u32) -> i16 {
    (((v as i32) << 21) >> 21) as i16
}

/// Decode a VRAM-transfer size word into (width, height) in pixels. A field of 0 means the maximum
/// for that axis (1024 wide / 512 tall) and the value otherwise wraps within that range — the
/// canonical `((n - 1) & mask) + 1` folds both rules into one expression.
fn transfer_size(wh: u32) -> (u32, u32) {
    let w = (((wh & 0xFFFF).wrapping_sub(1)) & 0x3FF) + 1;
    let h = ((((wh >> 16) & 0xFFFF).wrapping_sub(1)) & 0x1FF) + 1;
    (w, h)
}

/// Truncate a 24-bit GP0 command colour (`0x00BBGGRR` — red in the low byte) to a 15-bit VRAM pixel
/// (`0bbbbbgggggrrrrr`, mask bit 0). Each 8-bit channel keeps its top 5 bits.
fn rgb24_to_15(c: u32) -> u16 {
    let r = (c & 0xFF) >> 3;
    let g = ((c >> 8) & 0xFF) >> 3;
    let b = ((c >> 16) & 0xFF) >> 3;
    (r | (g << 5) | (b << 10)) as u16
}
