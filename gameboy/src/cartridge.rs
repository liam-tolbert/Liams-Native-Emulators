//! The game cartridge: the ROM image plus (later) a memory bank controller (MBC).
//!
//! For now this is a *NoMBC* cartridge — Tetris and other 32 KiB ROM-only games map
//! their whole ROM straight into 0x0000-0x7FFF with no banking. MBC1/MBC3 banking is
//! the optional M7/M8 stretch and slots in behind the same `read`/`write` interface.
//!
//! ## The cartridge header (0x0100-0x014F)
//! Every Game Boy ROM carries a fixed header. We only need a few fields:
//!   * 0x0134-0x0143  title (ASCII, zero-padded)
//!   * 0x0147         cartridge type (which MBC, plus RAM/battery)
//!   * 0x0148         ROM size code
//!   * 0x0149         RAM size code

/// Decoded, human-readable cartridge header.
pub struct Header {
    pub title: String,
    pub cart_type: u8,
    pub mapper: &'static str,
    pub rom_banks: u32, // number of 16 KiB banks
    pub ram_bytes: u32, // external cartridge RAM size, in bytes
}

pub struct Cartridge {
    rom: Vec<u8>,
    pub header: Header,

    // --- Banking state. A NoMBC cart never touches these. MBC1 carts use them to map a
    //     16 KiB bank into the 0x4000-0x7FFF window. This is a *minimal* MBC1: ROM-bank
    //     selection only, enough for the multi-bank Blargg test ROMs (and small MBC1
    //     games). Full MBC1 (RAM banking, the banking-mode flag, save RAM) is M7. ---
    mbc1: bool,
    rom_bank: usize,      // bank currently mapped at 0x4000-0x7FFF (forced >= 1)
    rom_bank_mask: usize, // (bank_count - 1); keeps a selected bank inside the real ROM
}

impl Cartridge {
    pub fn new(rom: Vec<u8>) -> Self {
        assert!(
            rom.len() >= 0x0150,
            "ROM is only {} bytes — too small to contain a 0x0100-0x014F header",
            rom.len()
        );
        let header = parse_header(&rom);
        // ROM bank counts are always powers of two (2 << code), so (count - 1) is a clean
        // wrap-around mask. The bus only routes MBC writes here when this is an MBC1 cart.
        let mbc1 = matches!(header.cart_type, 0x01..=0x03);
        let rom_bank_mask = (header.rom_banks.max(1) - 1) as usize;
        Cartridge { rom, header, mbc1, rom_bank: 1, rom_bank_mask }
    }

    /// The CPU's view of the cartridge address space (0x0000-0x7FFF ROM,
    /// 0xA000-0xBFFF external RAM). The bus routes those ranges here.
    pub fn read(&self, addr: u16) -> u8 {
        match addr {
            // 0x0000-0x3FFF is always ROM bank 0.
            0x0000..=0x3FFF => *self.rom.get(addr as usize).unwrap_or(&0xFF),
            // 0x4000-0x7FFF is the switchable window. NoMBC carts read on linearly (this
            // is just bank 1); MBC1 carts read whichever bank is currently selected.
            0x4000..=0x7FFF => {
                let bank = if self.mbc1 {
                    (self.rom_bank & self.rom_bank_mask).max(1)
                } else {
                    1
                };
                let off = bank * 0x4000 + (addr as usize - 0x4000);
                *self.rom.get(off).unwrap_or(&0xFF)
            }
            0xA000..=0xBFFF => 0xFF, // no external RAM wired up yet
            _ => 0xFF,
        }
    }

    pub fn write(&mut self, addr: u16, val: u8) {
        // NoMBC: ROM-region writes are ignored. On an MBC cart they're not memory writes
        // at all — they're commands to the mapper hardware selecting banks.
        if !self.mbc1 {
            return;
        }
        match addr {
            0x0000..=0x1FFF => {} // RAM enable — no cart RAM yet, so nothing to gate
            0x2000..=0x3FFF => {
                // Low 5 bits of the ROM bank number. MBC1 quirk: writing 0 here selects
                // bank 1 (the 0x4000 window can never show bank 0).
                let low = (val & 0x1F) as usize;
                let low = if low == 0 { 1 } else { low };
                self.rom_bank = (self.rom_bank & !0x1F) | low;
            }
            0x4000..=0x5FFF => {
                // Upper 2 bits (only relevant for ROMs > 512 KiB) — tracked for completeness.
                self.rom_bank = (self.rom_bank & 0x1F) | (((val & 0x03) as usize) << 5);
            }
            0x6000..=0x7FFF => {} // banking-mode select — irrelevant for these small ROMs
            _ => {}
        }
    }
}

/// Parse the header fields we care about out of a raw ROM image.
///
/// ─── PAIRING TASK (M0, Liam) ────────────────────────────────────────────────────
/// The `mapper` decode below is the *worked example* of the pattern you'll repeat:
/// **read a byte at a fixed offset, then interpret it.** Your job is to fill in the
/// three `TODO(Liam)` fields the same way. Check your work against Tetris's known
/// header — it should print:  title "TETRIS",  ROM 2 banks (32 KiB),  RAM 0 bytes.
///
/// Useful slices/refs:  `rom[0x0148]` (a single byte),  `&rom[0x0134..=0x0143]`
/// (a 16-byte slice).  Pan Docs "The Cartridge Header" has the full encodings.
/// ────────────────────────────────────────────────────────────────────────────────
fn parse_header(rom: &[u8]) -> Header {
    // --- WORKED EXAMPLE: cartridge type @ 0x0147 --------------------------------
    // One byte selects the mapper hardware. We only need a friendly name here; the
    // bus treats everything as NoMBC until real MBC support lands in M7.
    let cart_type = rom[0x0147];
    let mapper = match cart_type {
        0x00 => "ROM ONLY",
        0x01..=0x03 => "MBC1",
        0x05..=0x06 => "MBC2",
        0x0F..=0x13 => "MBC3",
        0x19..=0x1E => "MBC5",
        _ => "OTHER",
    };

    // --- TODO(Liam): title @ 0x0134..=0x0143 ------------------------------------
    // 16 bytes of ASCII, padded out with 0x00. Build a String from the printable
    // bytes (drop the trailing zeros). One clean way:
    //   take `&rom[0x0134..=0x0143]`, keep bytes that are non-zero, then
    //   `String::from_utf8_lossy(&kept).trim().to_string()`.
    let title : String = String::from_utf8_lossy(&rom[0x0134..=0x0143]).trim_end_matches('\0').to_string();

    // --- TODO(Liam): ROM size @ 0x0148 ------------------------------------------
    // The byte is a *code*, not a byte-count. Number of 16 KiB banks = `2 << code`.
    // (code 0x00 -> 2 banks = 32 KiB, which is Tetris.)
    let rom_banks: u32 = 2u32 << &rom[0x0148];

    // --- TODO(Liam): RAM size @ 0x0149 ------------------------------------------
    // Another code -> a byte count. Map it:
    //   0x00 -> 0,  0x02 -> 8 KiB,  0x03 -> 32 KiB,  0x04 -> 128 KiB,  0x05 -> 64 KiB.
    // (Tetris is 0x00 -> no RAM.)  A `match` returning the byte count works well.
    let ram_bytes: u32 = match &rom[0x0149] {
        0x00 => 0,
        0x02 => 8 * 1024,
        0x03 => 32 * 1024,
        0x04 => 128 * 1024,
        0x05 => 64 * 1024,
        _ => 0,
    };

    Header { title, cart_type, mapper, rom_banks, ram_bytes }
}
