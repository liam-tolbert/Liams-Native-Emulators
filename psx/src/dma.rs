//! DMA controller — **stub for the foundation milestones (real device is M4+).**
//!
//! The PS1 has seven DMA channels (MDEC in/out, GPU, CD-ROM, SPU, PIO, OTC) that move
//! blocks between RAM and a device without the CPU. The first one we'll actually need is
//! channel 2 (GPU) and the OTC linked-list channel, alongside the GPU in M4.
//!
//! Until then this is a dumb register file: the BIOS configures `DPCR` (channel
//! enables/priorities, 0x1F8010F0) and `DICR` (interrupt control, 0x1F8010F4) early in
//! boot, so we let those reads/writes round-trip and otherwise ignore everything. No actual
//! transfer happens yet.

pub struct Dma {
    regs: [u32; 0x20], // 0x1F801080-0x1F8010FF as 32 words; channel regs + DPCR/DICR
}

impl Dma {
    pub fn new() -> Self {
        // DPCR's reset value (0x07654321) is what the BIOS expects to read back before it
        // writes its own; the rest start clear.
        let mut regs = [0u32; 0x20];
        regs[0x1C] = 0x0765_4321; // (0x1F8010F0 - 0x1F801080) / 4 == 0x1C  -> DPCR
        Self { regs }
    }

    /// `offset` is the byte offset within 0x1F801080-0x1F8010FF.
    pub fn read(&self, offset: u32) -> u32 {
        self.regs[(offset >> 2) as usize & 0x1F]
    }

    pub fn write(&mut self, offset: u32, val: u32) {
        // No channel actually runs yet; just remember what was written so reads are stable.
        self.regs[(offset >> 2) as usize & 0x1F] = val;
    }
}
