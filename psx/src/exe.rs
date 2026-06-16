//! PS-EXE loader (used by the M3 test harness).
//!
//! A `.exe` (more precisely a "PS-X EXE") is the raw executable the BIOS would normally
//! copy off the CD and run. Homebrew and the amidog CPU/GTE test programs ship in this
//! format. Because we don't emulate the CD-ROM yet, the host sideloads one directly: it
//! lets the real BIOS boot to the point where it hands control to the game (PC reaches
//! 0x80030000), then injects the EXE's bytes into RAM and sets PC/GP/SP from the header —
//! exactly what the BIOS loader would have done.
//!
//! Header layout (the first 0x800 bytes of the file), all little-endian:
//! ```
//!   0x00  8 bytes  ASCII "PS-X EXE"
//!   0x10  u32      initial PC
//!   0x14  u32      initial GP (r28)
//!   0x18  u32      RAM load address
//!   0x1C  u32      file size (excludes this 0x800 header)
//!   0x30  u32      initial SP base (r29/r30)
//!   0x34  u32      initial SP offset   (real SP = base + offset, when base != 0)
//! ```
//! The program image itself follows the header, starting at file offset 0x800.

pub struct PsxExe {
    pub initial_pc: u32,
    pub initial_gp: u32,
    pub load_addr: u32,
    pub initial_sp: u32,
    pub data: Vec<u8>, // the program image (already stripped of the 0x800 header)
}

impl PsxExe {
    /// Parse a PS-EXE file image. Returns `None` if it isn't one (wrong magic / too short).
    pub fn parse(bytes: &[u8]) -> Option<PsxExe> {
        if bytes.len() < 0x800 || &bytes[0..8] != b"PS-X EXE" {
            return None;
        }
        let rd = |off: usize| -> u32 {
            u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
        };

        let initial_pc = rd(0x10);
        let initial_gp = rd(0x14);
        let load_addr = rd(0x18);
        let file_size = rd(0x1C) as usize;
        let sp_base = rd(0x30);
        let sp_off = rd(0x34);

        // Per the format: if the SP base is zero the loader leaves SP as the BIOS set it;
        // otherwise SP = base + offset.
        let initial_sp = if sp_base == 0 { 0 } else { sp_base.wrapping_add(sp_off) };

        // The image is `file_size` bytes starting right after the header. Clamp defensively
        // so a lying header can't slice past the end of the file.
        let end = (0x800 + file_size).min(bytes.len());
        let data = bytes[0x800..end].to_vec();

        Some(PsxExe { initial_pc, initial_gp, load_addr, initial_sp, data })
    }
}
