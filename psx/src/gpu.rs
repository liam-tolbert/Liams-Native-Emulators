//! The GPU — **stub for the foundation milestones (real device is M4).**
//!
//! The full GPU is a big chunk of work (1 MiB VRAM, the GP0/GP1 command FIFO, a software
//! rasterizer for flat/Gouraud/textured triangles). None of that is needed to validate the
//! CPU, so for now this is the thinnest thing that keeps the BIOS from hanging:
//!
//!   * `GPUSTAT` (read at 0x1F801814) reports "ready for command / DMA / to send VRAM" so
//!     the BIOS's `while (!(GPUSTAT & ready)) {}` poll loops fall straight through instead
//!     of spinning forever.
//!   * GP0 (draw/command) and GP1 (display control) writes are accepted and dropped.
//!
//! Everything here gets replaced wholesale in M4; the register addresses are the only part
//! that will survive.

pub struct Gpu {
    // Real VRAM/FIFO/rasterizer state lands in M4.
}

impl Gpu {
    pub fn new() -> Self {
        Self {}
    }

    /// `GPUSTAT` (0x1F801814 read). Bits 26 (ready for command word), 27 (ready to send a
    /// VRAM read), and 28 (ready to receive a DMA block) are forced on so BIOS poll loops
    /// proceed. The real status register is computed from FIFO/transfer state in M4.
    pub fn status(&self) -> u32 {
        0x1C00_0000
    }

    /// `GPUREAD` (0x1F801810 read) — VRAM/-register read-back. Nothing to return yet.
    pub fn read(&self) -> u32 {
        0
    }

    /// GP0 (0x1F801810 write) — rendering & VRAM-transfer commands. Dropped until M4.
    pub fn gp0(&mut self, _word: u32) {}

    /// GP1 (0x1F801814 write) — display control (reset, DMA mode, display area ...).
    pub fn gp1(&mut self, _word: u32) {}
}
