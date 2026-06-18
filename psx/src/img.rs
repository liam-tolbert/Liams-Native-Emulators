//! A tiny, from-scratch PNG reader/writer — **host-side test harness, not emulated hardware.**
//!
//! ## Why this exists
//!
//! M4's correctness discipline is the same golden-file diffing the rest of the project lives by
//! (Blargg's serial output, the dmg-acid2 thumbnail): render into VRAM, then compare the result
//! pixel-for-pixel against a reference image. The reference images that ship with the JaCzekanski
//! `ps1-tests` suite are **PNGs**, so the harness needs to read PNG — and to write it, so a frame
//! can be dumped for eyeballing or saved as a portfolio screenshot.
//!
//! Rather than pull in an image crate (the project is deliberately `minifb`-only, and "I wrote it
//! from scratch" is the whole point of the piece), this is a minimal PNG codec. It is **not**
//! general: it reads/writes only **8-bit truecolour RGB** with the **None** scanline filter, and on
//! the read side it currently only decodes **uncompressed** DEFLATE — which is exactly what the
//! writer below produces. That's enough for M4a, whose only diffing is round-tripping our own
//! dumps. **M4b extends the reader with a full DEFLATE inflate + the four other filter types** so it
//! can ingest the suite's real (compressed) reference PNGs.
//!
//! ## PNG in one paragraph
//!
//! A PNG is an 8-byte signature followed by a sequence of *chunks*. Each chunk is `[length:4 big-
//! endian][type:4 ascii][data:length][crc:4]`, the CRC covering the type+data. We emit three: IHDR
//! (image header: width, height, bit depth, colour type), IDAT (the pixel data), and IEND
//! (terminator). The pixel data inside IDAT is **zlib-compressed**: each scanline is prefixed with a
//! one-byte filter tag, all scanlines are concatenated, and the result is wrapped in a zlib stream.
//! We sidestep real compression by using DEFLATE's "stored" (literal) block type — valid zlib that
//! any decoder accepts, at the cost of files being ~the raw size. Two checksums are involved: a
//! **CRC-32** per chunk (PNG's own integrity check) and an **Adler-32** over the uncompressed data
//! (zlib's check). Both are implemented below.

const SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

// ===== encode ==========================================================================

/// Encode an 8-bit RGB image (`rgb` is `width*height*3` bytes, row-major) as PNG bytes.
pub fn encode_rgb(width: u32, height: u32, rgb: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&SIGNATURE);

    // IHDR: width, height, bit depth 8, colour type 2 (truecolour RGB), no interlace.
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.push(8); // bit depth per channel
    ihdr.push(2); // colour type 2 = RGB
    ihdr.push(0); // compression method (0 = zlib/deflate, the only defined one)
    ihdr.push(0); // filter method   (0 = the standard adaptive set; we only use "None")
    ihdr.push(0); // interlace       (0 = none)
    chunk(&mut out, b"IHDR", &ihdr);

    // Build the "raw" image: every scanline gets a leading filter byte (0 = None, i.e. store the
    // row verbatim). This filtered stream is what zlib wraps and what Adler-32 is computed over.
    let stride = width as usize * 3;
    let mut raw = Vec::with_capacity((stride + 1) * height as usize);
    for y in 0..height as usize {
        raw.push(0); // filter type: None
        raw.extend_from_slice(&rgb[y * stride..y * stride + stride]);
    }

    chunk(&mut out, b"IDAT", &zlib_store(&raw));
    chunk(&mut out, b"IEND", &[]);
    out
}

/// Wrap `data` in a zlib stream built entirely from DEFLATE "stored" (uncompressed) blocks. A stored
/// block is `[1 header byte: BFINAL in bit 0, BTYPE=00 in bits 1-2][LEN:2 LE][~LEN:2 LE][LEN bytes]`.
/// DEFLATE's spec says a stored block's length fields start on a byte boundary, and because the 3-bit
/// block header sits at the top of its own byte here (the previous block's data ended byte-aligned),
/// writing the header as a whole byte and following it immediately with LEN is standards-correct.
/// Blocks cap at 0xFFFF bytes, so large images split across several.
fn zlib_store(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    // zlib header: CMF=0x78 (deflate, 32 KiB window), FLG=0x01 chosen so (CMF<<8 | FLG) % 31 == 0.
    out.push(0x78);
    out.push(0x01);

    let mut i = 0;
    loop {
        let remaining = data.len() - i;
        let block = remaining.min(0xFFFF);
        let is_final = i + block >= data.len();
        out.push(is_final as u8); // BFINAL bit 0; BTYPE 00 (stored) in bits 1-2; rest padding
        let len = block as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes()); // NLEN = one's-complement of LEN
        out.extend_from_slice(&data[i..i + block]);
        i += block;
        if is_final {
            break;
        }
    }

    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

/// Append a PNG chunk: length, 4-byte type, data, then the CRC-32 of (type ++ data).
fn chunk(out: &mut Vec<u8>, ctype: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let crc_from = out.len(); // CRC covers the type bytes and the data, but not the length
    out.extend_from_slice(ctype);
    out.extend_from_slice(data);
    let crc = crc32(&out[crc_from..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

// ===== decode ==========================================================================

/// Decode a PNG produced in the narrow subset this module writes (8-bit RGB, None filter, stored
/// DEFLATE). Returns `(width, height, rgb)` or `None` if the file isn't something we can read.
/// (M4b widens this to real inflate + all filter types for the `ps1-tests` reference images.)
pub fn decode_rgb(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    if bytes.len() < 8 || bytes[..8] != SIGNATURE {
        return None;
    }

    let mut pos = 8;
    let mut width = 0u32;
    let mut height = 0u32;
    let mut bit_depth = 0u8;
    let mut colour_type = 0u8;
    let mut idat = Vec::new();

    // Walk the chunk list.
    while pos + 8 <= bytes.len() {
        let len = u32::from_be_bytes(bytes[pos..pos + 4].try_into().ok()?) as usize;
        let ctype = &bytes[pos + 4..pos + 8];
        let data_start = pos + 8;
        if data_start + len + 4 > bytes.len() {
            return None; // truncated chunk
        }
        let data = &bytes[data_start..data_start + len];
        match ctype {
            b"IHDR" => {
                width = u32::from_be_bytes(data[0..4].try_into().ok()?);
                height = u32::from_be_bytes(data[4..8].try_into().ok()?);
                bit_depth = data[8];
                colour_type = data[9];
            }
            b"IDAT" => idat.extend_from_slice(data),
            b"IEND" => break,
            _ => {} // ignore ancillary chunks
        }
        pos = data_start + len + 4; // step past data + the 4-byte CRC
    }

    if bit_depth != 8 || colour_type != 2 {
        return None; // only 8-bit RGB is supported here
    }

    let raw = zlib_inflate_stored(&idat)?;

    // Strip the per-scanline filter byte (must be 0 = None in our subset).
    let stride = width as usize * 3;
    let mut out = Vec::with_capacity(stride * height as usize);
    let mut p = 0;
    for _ in 0..height {
        if p + 1 + stride > raw.len() {
            return None;
        }
        if raw[p] != 0 {
            return None; // a non-None filter -> needs the M4b de-filter path
        }
        p += 1;
        out.extend_from_slice(&raw[p..p + stride]);
        p += stride;
    }
    Some((width, height, out))
}

/// Inverse of `zlib_store`: read a zlib stream made only of stored blocks. Returns `None` on a
/// compressed block (BTYPE != 0), which is the signal that the full inflate of M4b is needed.
fn zlib_inflate_stored(zlib: &[u8]) -> Option<Vec<u8>> {
    if zlib.len() < 2 {
        return None;
    }
    let mut p = 2; // skip the 2-byte zlib header (CMF/FLG)
    let mut out = Vec::new();
    loop {
        if p >= zlib.len() {
            return None;
        }
        let header = zlib[p];
        p += 1;
        let bfinal = header & 1;
        let btype = (header >> 1) & 3;
        if btype != 0 {
            return None; // compressed block — not supported until M4b
        }
        if p + 4 > zlib.len() {
            return None;
        }
        let len = u16::from_le_bytes([zlib[p], zlib[p + 1]]) as usize;
        p += 4; // LEN (2) + NLEN (2), NLEN unchecked
        if p + len > zlib.len() {
            return None;
        }
        out.extend_from_slice(&zlib[p..p + len]);
        p += len;
        if bfinal == 1 {
            break;
        }
    }
    Some(out)
}

// ===== checksums =======================================================================

/// CRC-32 as PNG defines it (reflected, polynomial 0xEDB88320, init/final XOR 0xFFFFFFFF). Computed
/// bytewise without a lookup table — speed is irrelevant for a test harness.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in bytes {
        crc ^= b as u32;
        for _ in 0..8 {
            // mask = 0xFFFFFFFF when the low bit is set, else 0 — branch-free conditional XOR.
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Adler-32, zlib's checksum: two running sums mod 65521, packed `(high << 16) | low`.
fn adler32(bytes: &[u8]) -> u32 {
    let mut a = 1u32;
    let mut b = 0u32;
    for &x in bytes {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}
