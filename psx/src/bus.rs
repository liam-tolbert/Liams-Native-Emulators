//! The memory bus / MMU — the hub the CPU talks to.
//!
//! Same shape as the Game Boy's bus: the CPU sees the world *only* through `read*`/`write*`,
//! and the bus owns every device (RAM, scratchpad, BIOS, the GPU/DMA/IRQ stubs). The CPU,
//! the one active component, owns the bus — one-way ownership the borrow checker is happy
//! with. Two things are bigger than on the DMG:
//!
//!  * **32-bit, three widths.** The MIPS bus is 32 bits wide and little-endian, and software
//!    mixes byte/half/word accesses, so there's a `read8/16/32` + `write8/16/32` family
//!    instead of the DMG's single `read(u16) -> u8`.
//!  * **Segment mirroring.** A PS1 virtual address's top 3 bits select a "segment" (KUSEG,
//!    KSEG0, KSEG1, KSEG2) that all alias the *same* physical memory — KSEG0/KSEG1 are just
//!    cached/uncached windows onto it. We mask those bits off to a physical address first,
//!    then decode. This is the clean version of the DMG's echo-RAM mirror.
//!
//! Physical memory map (after masking):
//! ```
//!   0x00000000-0x007FFFFF  2 MiB main RAM (mirrored 4x)
//!   0x1F000000-0x1F7FFFFF  Expansion 1 (no cart -> open bus)
//!   0x1F800000-0x1F8003FF  1 KiB scratchpad (the D-cache used as fast RAM)
//!   0x1F801000-0x1F801FFF  hardware I/O registers (IRQ, DMA, GPU, timers, SPU, CD ...)
//!   0x1F802000-0x1F802FFF  Expansion 2 (debug/POST)
//!   0x1FC00000-0x1FC7FFFF  512 KiB BIOS ROM
//!   0xFFFE0130             cache-control register (KSEG2, never masked)
//! ```

use crate::dma::Dma;
use crate::gpu::Gpu;
use crate::irq::Irq;

const RAM_SIZE: usize = 2 * 1024 * 1024; // 2 MiB
const SCRATCH_SIZE: usize = 1024; // 1 KiB
const BIOS_SIZE: usize = 512 * 1024; // 512 KiB

/// Per-segment AND mask, indexed by the top 3 bits of the virtual address. KUSEG and KSEG2
/// pass through unchanged (KUSEG low addresses already equal their physical address; the PS1
/// has no TLB so high KUSEG is unused). KSEG0 strips bit 31; KSEG1 strips the top 3 bits —
/// both fold onto the same physical region.
const REGION_MASK: [u32; 8] = [
    0xFFFF_FFFF, 0xFFFF_FFFF, 0xFFFF_FFFF, 0xFFFF_FFFF, // KUSEG  (0x00000000-0x7FFFFFFF)
    0x7FFF_FFFF, // KSEG0  (0x80000000-0x9FFFFFFF)
    0x1FFF_FFFF, // KSEG1  (0xA0000000-0xBFFFFFFF)
    0xFFFF_FFFF, 0xFFFF_FFFF, // KSEG2  (0xC0000000-0xFFFFFFFF)
];

/// What a (masked) physical address decodes to. Returning a small enum keeps the three
/// read widths and three write widths from each repeating the address ranges.
enum Region {
    Ram(usize),     // offset into the 2 MiB RAM (already mirror-folded)
    Scratch(usize), // offset into the 1 KiB scratchpad
    Bios(usize),    // offset into the 512 KiB BIOS
    Io(u32),        // offset within 0x1F801000-0x1F801FFF
    Exp2(u32),      // offset within 0x1F802000 (debug TTY / POST lives here)
    Exp1,           // no expansion board -> reads float high
    CacheCtrl,      // 0xFFFE0130
    Unmapped,
}

pub struct Bus {
    // Heap-allocated (not `Box<[u8; N]>`) so building the bus never puts a 2 MiB array on
    // the stack — a real stack-overflow risk in unoptimised builds.
    ram: Vec<u8>,
    scratch: Vec<u8>,
    bios: Vec<u8>, // filled by `load_bios`; empty until then

    pub irq: Irq,
    pub gpu: Gpu,
    pub dma: Dma,

    mem_control: [u32; 0x24], // 0x1F801000-0x1F80105F + RAM_SIZE/cache regs, just stored
    cache_control: u32,       // 0xFFFE0130

    /// Everything the BIOS has printed over the kernel TTY. The M3 harness watches this for
    /// a "Passed"/"Failed" verdict — the direct analog of the Game Boy's `serial_out`.
    pub tty_out: String,
}

impl Bus {
    pub fn new() -> Self {
        Self {
            ram: vec![0; RAM_SIZE],
            scratch: vec![0; SCRATCH_SIZE],
            bios: Vec::new(),
            irq: Irq::new(),
            gpu: Gpu::new(),
            dma: Dma::new(),
            mem_control: [0; 0x24],
            cache_control: 0,
            tty_out: String::new(),
        }
    }

    /// Load the 512 KiB BIOS image (M3). Reset puts PC at 0xBFC00000, the start of this ROM.
    pub fn load_bios(&mut self, bytes: Vec<u8>) {
        self.bios = bytes;
    }

    pub fn bios_loaded(&self) -> bool {
        self.bios.len() >= BIOS_SIZE
    }

    /// Push one character onto the captured TTY stream (called by the CPU's BIOS-putchar
    /// hook). Also echoed live, like the DMG serial hook.
    pub fn tty_push(&mut self, ch: u8) {
        self.tty_out.push(ch as char);
        print!("{}", ch as char);
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }

    /// Advance the time-based subsystems (M4+: timers, GPU, DMA). The "catch-up" seam is
    /// here exactly as on the DMG; for the foundation milestones there's nothing to tick.
    pub fn tick(&mut self, _cycles: u32) {}

    // ===== Direct RAM access (used by the PS-EXE sideloader to inject an image) =========
    pub fn store_ram(&mut self, addr: u32, bytes: &[u8]) {
        let base = (addr & 0x1F_FFFF) as usize;
        for (i, &b) in bytes.iter().enumerate() {
            if base + i < RAM_SIZE {
                self.ram[base + i] = b;
            }
        }
    }

    // ===== Address decode ===============================================================
    fn decode(addr: u32) -> Region {
        let phys = addr & REGION_MASK[(addr >> 29) as usize];
        match phys {
            0x0000_0000..=0x007F_FFFF => Region::Ram((phys & 0x1F_FFFF) as usize),
            0x1F00_0000..=0x1F7F_FFFF => Region::Exp1,
            0x1F80_0000..=0x1F80_03FF => Region::Scratch((phys - 0x1F80_0000) as usize),
            0x1F80_1000..=0x1F80_1FFF => Region::Io(phys - 0x1F80_1000),
            0x1F80_2000..=0x1F80_2FFF => Region::Exp2(phys - 0x1F80_2000),
            0x1FC0_0000..=0x1FC7_FFFF => Region::Bios((phys - 0x1FC0_0000) as usize),
            0xFFFE_0130 => Region::CacheCtrl,
            _ => Region::Unmapped,
        }
    }

    // ===== Reads ========================================================================
    pub fn read32(&self, addr: u32) -> u32 {
        match Self::decode(addr) {
            Region::Ram(o) => le32(&self.ram[o..]),
            Region::Scratch(o) => le32(&self.scratch[o..]),
            Region::Bios(o) => self.bios.get(o..o + 4).map_or(0xFFFF_FFFF, le32),
            Region::Io(o) => self.io_read(o, 32),
            Region::CacheCtrl => self.cache_control,
            Region::Exp1 | Region::Exp2(_) | Region::Unmapped => 0xFFFF_FFFF,
        }
    }

    pub fn read16(&self, addr: u32) -> u16 {
        match Self::decode(addr) {
            Region::Ram(o) => le16(&self.ram[o..]),
            Region::Scratch(o) => le16(&self.scratch[o..]),
            Region::Bios(o) => self.bios.get(o..o + 2).map_or(0xFFFF, le16),
            Region::Io(o) => self.io_read(o, 16) as u16,
            Region::Exp1 | Region::Exp2(_) | Region::Unmapped | Region::CacheCtrl => 0xFFFF,
        }
    }

    pub fn read8(&self, addr: u32) -> u8 {
        match Self::decode(addr) {
            Region::Ram(o) => self.ram[o],
            Region::Scratch(o) => self.scratch[o],
            Region::Bios(o) => self.bios.get(o).copied().unwrap_or(0xFF),
            Region::Io(o) => self.io_read(o, 8) as u8,
            Region::Exp1 | Region::Exp2(_) | Region::Unmapped | Region::CacheCtrl => 0xFF,
        }
    }

    // ===== Writes =======================================================================
    pub fn write32(&mut self, addr: u32, val: u32) {
        match Self::decode(addr) {
            Region::Ram(o) => put_le32(&mut self.ram[o..], val),
            Region::Scratch(o) => put_le32(&mut self.scratch[o..], val),
            Region::Io(o) => self.io_write(o, val, 32),
            Region::CacheCtrl => self.cache_control = val,
            // The BIOS / ROM and the expansion regions ignore writes.
            Region::Bios(_) | Region::Exp1 | Region::Exp2(_) | Region::Unmapped => {}
        }
    }

    pub fn write16(&mut self, addr: u32, val: u16) {
        match Self::decode(addr) {
            Region::Ram(o) => put_le16(&mut self.ram[o..], val),
            Region::Scratch(o) => put_le16(&mut self.scratch[o..], val),
            Region::Io(o) => self.io_write(o, val as u32, 16),
            Region::Bios(_)
            | Region::Exp1
            | Region::Exp2(_)
            | Region::Unmapped
            | Region::CacheCtrl => {}
        }
    }

    pub fn write8(&mut self, addr: u32, val: u8) {
        match Self::decode(addr) {
            Region::Ram(o) => self.ram[o] = val,
            Region::Scratch(o) => self.scratch[o] = val,
            Region::Io(o) => self.io_write(o, val as u32, 8),
            // The debug TTY/POST register lives in Expansion 2; harmless to drop here since
            // we capture TTY at the BIOS-call level instead.
            Region::Bios(_)
            | Region::Exp1
            | Region::Exp2(_)
            | Region::Unmapped
            | Region::CacheCtrl => {}
        }
    }

    // ===== Hardware I/O registers (0x1F801000-0x1F801FFF) ===============================
    // Only the registers the BIOS prods during early boot are real; the rest are stubbed so
    // a poll loop sees a sane value and moves on. Each gets fleshed out in its own milestone.
    //
    // DOCUMENTED GAP (M2): `width` is currently ignored — every register is treated as a plain
    // 32-bit word, and the CPU's load opcode does any byte/half narrowing on the value we return.
    // A few real registers behave differently per access width (e.g. byte vs. word reads of some
    // device ports), and I_STAT/I_MASK are physically 16-bit so their upper half reads back 0
    // here rather than whatever the hardware floats. None of that matters to the BIOS boot or the
    // amidog CPU tests, so it's deferred until a test ROM actually depends on it.
    fn io_read(&self, offset: u32, _width: u8) -> u32 {
        match offset {
            0x000..=0x05F => self.mem_control[(offset >> 2) as usize], // memory-control 1
            0x060 => self.mem_control[0x18], // RAM_SIZE
            0x070 => self.irq.read_stat() as u32, // I_STAT
            0x074 => self.irq.read_mask() as u32, // I_MASK
            0x080..=0x0FF => self.dma.read(offset - 0x080),
            0x100..=0x12F => 0, // timers (M4)
            0x810 => self.gpu.read(),    // GPUREAD
            0x814 => self.gpu.status(),  // GPUSTAT
            0xC00..=0xFFF => 0, // SPU (deferred, like the DMG APU)
            _ => 0,
        }
    }

    fn io_write(&mut self, offset: u32, val: u32, _width: u8) {
        match offset {
            0x000..=0x05F => self.mem_control[(offset >> 2) as usize] = val,
            0x060 => self.mem_control[0x18] = val,
            0x070 => self.irq.ack(val as u16),
            0x074 => self.irq.write_mask(val as u16),
            0x080..=0x0FF => self.dma.write(offset - 0x080, val),
            0x100..=0x12F => {} // timers (M4)
            0x810 => self.gpu.gp0(val),
            0x814 => self.gpu.gp1(val),
            0xC00..=0xFFF => {} // SPU
            _ => {}
        }
    }
}

// ===== little-endian byte<->word helpers ===============================================
fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}
fn le16(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}
fn put_le32(b: &mut [u8], v: u32) {
    b[0..4].copy_from_slice(&v.to_le_bytes());
}
fn put_le16(b: &mut [u8], v: u16) {
    b[0..2].copy_from_slice(&v.to_le_bytes());
}
