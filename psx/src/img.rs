//! PNG read/write for the verify harness — **host-side test glue, not emulated hardware.**
//!
//! ## Why this exists
//!
//! The correctness discipline here is the same golden-file diffing the rest of the project lives by
//! (Blargg's serial output, the dmg-acid2 thumbnail): render into VRAM, then compare pixel-for-pixel
//! against a reference image. The reference images that ship with the JaCzekanski `ps1-tests` suite
//! are **PNGs** — and, crucially, *real DEFLATE-compressed* ones (a 1024x512 frame is ~15-25 KB, not
//! the ~1.5 MB it would be uncompressed). So the harness needs a genuine PNG decoder.
//!
//! An early hand-rolled *uncompressed* codec got away with it because it only ever round-tripped its
//! own dumps. Ingesting the suite's compressed references means a full DEFLATE inflate and
//! all five PNG scanline filters. Rather than hand-roll that, this module is now a thin wrapper over
//! the **`png` crate** (which pulls in `miniz_oxide`/`flate2` for the actual inflate/deflate). That's
//! the project's first non-`minifb` dependency, but it lives entirely on the **host/test side** — the
//! emulated machine never touches it — so the emulator core stays dependency-light and clean-room.
//!
//! The two public functions keep the exact signatures the harness already calls, so `main.rs`
//! and the self-test are untouched by the switch.

/// Encode an 8-bit RGB image (`rgb` is `width*height*3` bytes, row-major) as a PNG byte vector.
/// Uses the `png` crate's default (compressed) output, so dumps are small enough to keep around as
/// portfolio screenshots.
pub fn encode_rgb(width: u32, height: u32, rgb: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        // `write_header` -> `write_image_data` writes the whole frame; dropping the writer at the end
        // of this scope flushes the trailing IEND chunk and releases the borrow on `out`.
        let mut writer = encoder
            .write_header()
            .expect("PNG header write should not fail to an in-memory buffer");
        writer
            .write_image_data(rgb)
            .expect("PNG image-data write should not fail to an in-memory buffer");
    }
    out
}

/// Decode a PNG into `(width, height, rgb8)`. The suite's VRAM references come in two flavours —
/// 8-bit truecolour (e.g. `triangle`, `rectangles`) *and* palette/indexed (e.g. `clipping` is 4-bit
/// indexed, `quad`/`lines` are 8-bit indexed). We ask the `png` crate to **expand** both palette
/// indices and any sub-8-bit/16-bit samples to a flat 8-bit channel layout first, so a single RGB
/// (or RGBA, alpha dropped) path covers every reference the pixel-diff gates against. Anything that
/// still isn't 8-bit RGB(A) after that (true grayscale) returns `None`.
pub fn decode_rgb(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    // EXPAND: palette -> RGB and sub-8-bit -> 8-bit; STRIP_16: 16-bit -> 8-bit. Together they
    // normalize the reference to 8 bits/channel so the match below stays simple.
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;

    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());

    if info.bit_depth != png::BitDepth::Eight {
        return None; // only 8-bit samples; the VRAM references are all 8-bit RGB
    }

    let rgb = match info.color_type {
        png::ColorType::Rgb => buf,
        png::ColorType::Rgba => {
            // Drop the alpha byte of each pixel down to packed RGB.
            let mut out = Vec::with_capacity(buf.len() / 4 * 3);
            for px in buf.chunks_exact(4) {
                out.extend_from_slice(&px[0..3]);
            }
            out
        }
        _ => return None, // grayscale / indexed — not used by the RGB VRAM references
    };

    Some((info.width, info.height, rgb))
}
