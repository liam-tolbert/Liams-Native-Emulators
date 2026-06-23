//! A minimal, host-side ISO9660 reader — just enough to find and read the two files the PlayStation
//! boot needs off a data disc: `SYSTEM.CNF` and the game's boot executable (`SLUS_xxx.xx`).
//!
//! This is *not* part of the emulated machine. On real hardware the BIOS shell walks the disc's
//! filesystem itself, talking to the CD-ROM controller; our HLE ("high-level emulation") boot does
//! that walk on the host instead, then injects the executable exactly where the BIOS would have. So
//! this module reads the disc as **data** (an ISO9660 filesystem), where `cdrom.rs` models the
//! **hardware** (registers, FIFOs, interrupts). Keeping them apart keeps each one honest.
//!
//! ISO9660 layout (ECMA-119), only the slice we use:
//!   * The volume descriptors begin at **logical sector 16**. The first is the Primary Volume
//!     Descriptor (PVD): byte 0 = 1 (type "primary"), bytes 1..6 = the ASCII tag `"CD001"`.
//!   * Embedded in the PVD at **offset 156** is the *root directory record* — a directory record
//!     pointing at the root directory's extent (its starting sector) and length.
//!   * A directory is a run of directory records packed into its sectors; each names one child file
//!     or subdirectory and gives that child's extent + length. Records never straddle a sector — a
//!     zero length byte means "no more records here, skip to the next sector".
//!
//! **The sector-mapping gotcha.** This disc is one MODE2/2352 track starting at 00:00:00, so an ISO
//! "logical sector N" is just raw `.bin` sector N, and the 2048-byte user area sits at offset 24 of
//! the 2352-byte raw sector (12 sync + 4 header + 8 subheader). Crucially we index raw sectors
//! **directly by LBA here** — we do NOT apply the 150-frame pre-gap that `cdrom.rs`'s MSF<->LBA
//! conversion does. That pre-gap is a *disc-addressing* (minute:second:frame) convention the drive
//! commands use; a host-side read of "the 16th data sector" is just file offset 16 * 2352. Subtract
//! 150 here and every read lands 150 sectors early — the classic "boot reads garbage" mistake.

use crate::cdrom::CdImage;

/// Where the user data sits inside a raw 2352-byte MODE2/Form1 sector: 12 sync + 4 header + 8
/// subheader = 24, then 2048 bytes of payload. We always read the standard 2048 carve regardless of
/// the drive's current Setmode — the filesystem is laid out in 2048-byte logical sectors by spec.
const ISO_USER_OFF: usize = 24;
const ISO_USER_LEN: usize = 2048;
/// The volume descriptor set starts at logical sector 16; the PVD is the first one.
const PVD_LBA: u32 = 16;
/// Byte offset of the root directory record embedded in the PVD.
const ROOT_RECORD_OFF: usize = 156;

/// Read the 2048-byte user payload of one ISO logical sector (== raw `.bin` sector, see the module
/// note on the no-pre-gap mapping).
fn read_logical(cd: &CdImage, lba: u32) -> [u8; ISO_USER_LEN] {
    let raw = cd.read_sector(lba);
    let mut out = [0u8; ISO_USER_LEN];
    out.copy_from_slice(&raw[ISO_USER_OFF..ISO_USER_OFF + ISO_USER_LEN]);
    out
}

/// Little-endian u32 from the front of a byte slice (ISO9660 stores most numbers both-endian; we
/// read the LE copy). Callers slice so that at least 4 bytes remain.
fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// Does an ISO directory record's name identify the file we want? Handles the three wrinkles:
///   * The "." and ".." entries are stored as the single bytes 0x00 / 0x01 — never a real match.
///   * File identifiers carry a `;N` version suffix (`SYSTEM.CNF;1`) — compare up to the `;`.
///   * Level-1 names are uppercase and may keep a trailing `.` — match case-insensitively, and drop
///     a lone trailing dot.
fn iso_name_eq(rec_name: &[u8], target: &str) -> bool {
    if rec_name.len() == 1 && (rec_name[0] == 0 || rec_name[0] == 1) {
        return false;
    }
    let end = rec_name
        .iter()
        .position(|&b| b == b';')
        .unwrap_or(rec_name.len());
    let mut nm = &rec_name[..end];
    if nm.last() == Some(&b'.') {
        nm = &nm[..nm.len() - 1];
    }
    nm.eq_ignore_ascii_case(target.as_bytes())
}

/// A read-only view over a disc's ISO9660 filesystem. Borrows the `CdImage` (which stays inserted in
/// the drive) and reads sectors through its public `read_sector`.
pub struct IsoReader<'a> {
    cd: &'a CdImage,
    root_lba: u32,
    root_len: u32,
}

impl<'a> IsoReader<'a> {
    /// Open the filesystem: read the PVD at logical sector 16, verify the `CD001` tag, and pull the
    /// root directory record out of it. Returns `None` if it doesn't look like an ISO9660 volume.
    pub fn open(cd: &'a CdImage) -> Option<Self> {
        let pvd = read_logical(cd, PVD_LBA);
        if pvd[0] != 1 || &pvd[1..6] != b"CD001" {
            return None;
        }
        // The root record's own extent LBA (offset 2) and data length (offset 10) within the record.
        let root = &pvd[ROOT_RECORD_OFF..];
        let root_lba = le32(&root[2..]);
        let root_len = le32(&root[10..]);
        Some(Self {
            cd,
            root_lba,
            root_len,
        })
    }

    /// Find a child of the **root** directory by name, returning `(extent_lba, length_bytes)`. The
    /// PS1 boot files (`SYSTEM.CNF` and the boot executable) live in the root, so a flat root scan is
    /// all we need — no subdirectory walking.
    pub fn find_in_root(&self, name: &str) -> Option<(u32, u32)> {
        let sectors = (self.root_len as usize).div_ceil(ISO_USER_LEN);
        for i in 0..sectors {
            let sec = read_logical(self.cd, self.root_lba + i as u32);
            let mut pos = 0usize;
            while pos < ISO_USER_LEN {
                let reclen = sec[pos] as usize;
                if reclen == 0 {
                    break; // zero length: rest of this sector is padding -> next sector
                }
                // Defensive: a malformed record shouldn't read past the sector buffer.
                if pos + 33 > ISO_USER_LEN || pos + reclen > ISO_USER_LEN {
                    break;
                }
                let name_len = sec[pos + 32] as usize;
                let name_end = (pos + 33 + name_len).min(ISO_USER_LEN);
                if iso_name_eq(&sec[pos + 33..name_end], name) {
                    let extent = le32(&sec[pos + 2..]);
                    let data_len = le32(&sec[pos + 10..]);
                    return Some((extent, data_len));
                }
                pos += reclen;
            }
        }
        None
    }

    /// Read `len` bytes of a file starting at logical sector `lba`, concatenating the 2048-byte user
    /// areas of `ceil(len/2048)` sectors and trimming the tail to `len`.
    pub fn read_file(&self, lba: u32, len: u32) -> Vec<u8> {
        let mut out = Vec::with_capacity(len as usize);
        let sectors = (len as usize).div_ceil(ISO_USER_LEN);
        for i in 0..sectors {
            out.extend_from_slice(&read_logical(self.cd, lba + i as u32));
        }
        out.truncate(len as usize);
        out
    }
}

/// Pull the boot executable's filename out of a `SYSTEM.CNF`. The file is a few lines of `KEY = VALUE`
/// text; we want `BOOT = cdrom:\SLUS_005.71;1` -> `SLUS_005.71`. We read it from the disc rather than
/// hardcoding the id, since it differs per game. (`STACK`/`TCB`/`EVENT` are ignored: the real BIOS
/// kernel we boot first sets up the stack, and the PS-EXE header carries its own SP.)
pub fn parse_boot_filename(system_cnf: &str) -> Option<String> {
    for line in system_cnf.lines() {
        let (key, val) = match line.split_once('=') {
            Some(kv) => kv,
            None => continue,
        };
        if !key.trim().eq_ignore_ascii_case("BOOT") {
            continue;
        }
        // Strip the `cdrom:\` device prefix, any leading/remaining path separators, then the `;N`
        // version suffix.
        let val = val.trim().trim_start_matches("cdrom:");
        let val = val.trim_start_matches(|c| c == '\\' || c == '/');
        let val = val.rsplit(|c| c == '\\' || c == '/').next().unwrap_or(val);
        let val = val.split(';').next().unwrap_or(val).trim();
        if val.is_empty() {
            return None;
        }
        return Some(val.to_string());
    }
    None
}
