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

    // ===== the four ports the bus routes to =============================================

    /// `GPUSTAT` (`0x1F801814` read) — assembled from the live register state above. Until M4a this
    /// was a constant `0x1C000000`; the BIOS now drives it through a real reset + poll path.
    pub fn status(&self) -> u32 {
        let mut s = 0u32;

        // Bits 0-10 come verbatim from the GP0(E1) draw-mode word (texpage / semi-transparency /
        // texture depth / dither / draw-to-display), and bit 15 is its texture-disable flag.
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

        // --- mid polyline: drain vertices/colours until the terminator word --------------------
        // A polyline (`48`-`5F` with bit 3 set) sends an arbitrary number of points and ends with a
        // sentinel word matching `0x5xxx_5xxx`. Its length isn't known up front, so it gets its own
        // drain mode instead of a `gp0_remaining` count. (M4a only drops it; M4d renders it. The
        // terminator test is intentionally loose here — good enough until a real polyline is drawn.)
        if self.gp0_polyline {
            if word & 0xF000_F000 == 0x5000_5000 {
                self.gp0_polyline = false;
            }
            return;
        }

        if self.gp0_remaining == 0 {
            // --- first word of a new command: decode it and learn the command's length ----------
            let cmd = (word >> 24) as u8;
            self.gp0_command = cmd;
            self.gp0_buffer[0] = word;
            self.gp0_len = 1;

            // Polylines are variable-length: switch to drain-until-terminator and stop here.
            if (0x40..=0x5F).contains(&cmd) && cmd & 0x08 != 0 {
                self.gp0_polyline = true;
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
            0x10 => self.gpu_info(param),             // stage a GPUREAD info response
            _ => {} // 0x09 (texture-disable enable), 0x20 (GPU type) etc. — not needed for M4a boot
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
            0xE1 => self.draw_mode = w0 & 0x3FFF, // texpage / semi-transp / depth / dither / flips
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

            // --- rendering primitives — parsed-and-dropped until M4d ----------------------------
            0x20..=0x3F => { /* polygon */ }
            0x40..=0x5F => { /* line (non-polyline; polylines drain in gp0) */ }
            0x60..=0x7F => { /* rectangle / sprite */ }

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
}

// ===== free helpers =====================================================================

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
