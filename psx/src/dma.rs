//! DMA controller — channels 2 (GPU) and 6 (OTC), plus the DMA interrupt.
//!
//! DMA ("direct memory access") is the hardware that moves blocks between RAM and a device
//! *without* the CPU copying word-by-word. The PS1 has seven channels (MDEC in/out, GPU, CD-ROM,
//! SPU, PIO, OTC); we implement the two that the GPU path needs:
//!
//!  * **Channel 2 (GPU).** Two shapes. In *block mode* it streams CPU↔VRAM image data through the
//!    GP0/GPUREAD ports (the `A0`/`C0` transfers). In *linked-list mode* it walks a chain of command
//!    packets the program built in RAM and feeds each word to GP0 — this is how real games **and the
//!    `ps1-tests` `gpu/` ROMs** submit every draw command (they never touch the GP0 port directly),
//!    which is exactly why DMA had to land before the rasterizer.
//!  * **Channel 6 (OTC — "ordering-table clear").** A hardwired helper that fills RAM with a
//!    backward-linked empty ordering table, the skeleton a game then hangs its GP0 packets off of.
//!
//! This file is the **register model** only — a flat word array plus typed accessors and the DICR
//! interrupt logic. The actual transfer *engine* lives on `Bus` (`Bus::run_dma`), because a transfer
//! has to touch RAM, the GPU, and the interrupt controller at once and only the bus owns all three.
//! A CHCR write that sets a channel's start bit is what kicks it off (see `Bus::io_write`).
//!
//! Register block layout (`0x1F801080-0x1F8010FF`), indexed here as 32-bit words:
//! ```
//!   channel n:  MADR = regs[n*4+0]   base address in RAM
//!               BCR  = regs[n*4+1]   block/word count
//!               CHCR = regs[n*4+2]   control: direction, step, sync mode, start bits
//!   DPCR = regs[0x1C]  (0x1F8010F0)  per-channel enable + priority
//!   DICR = regs[0x1D]  (0x1F8010F4)  DMA interrupt control
//! ```

pub struct Dma {
    regs: [u32; 0x20], // 0x1F801080-0x1F8010FF as 32 words; channel regs + DPCR/DICR
}

impl Dma {
    pub fn new() -> Self {
        // DPCR's reset value (0x07654321) is what the BIOS expects to read back before it
        // writes its own; the rest start clear. (Note: every channel's *enable* bit — bit 3 of its
        // nibble, the 0x8 weight — is clear here, so nothing runs until software programs DPCR.)
        let mut regs = [0u32; 0x20];
        regs[0x1C] = 0x0765_4321; // (0x1F8010F0 - 0x1F801080) / 4 == 0x1C  -> DPCR
        Self { regs }
    }

    /// `offset` is the byte offset within 0x1F801080-0x1F8010FF. Plain round-trip for MADR/BCR and
    /// DPCR; DICR has computed bits and is read through `dicr_read` instead (see `Bus::io_read`).
    pub fn read(&self, offset: u32) -> u32 {
        let val = self.regs[(offset >> 2) as usize & 0x1F];
        // A channel's CHCR sits at byte offset `ch*0x10 + 0x8`. **Gotcha:** most CHCR bits aren't
        // physically implemented — a read returns only the writable bits, with a couple hardwired —
        // so software reading the register back sees fixed values, not whatever it wrote. Apply the
        // per-channel read mask so that round-trip matches hardware (the OTC conformance ROM checks
        // exactly this).
        if offset & 0xF == 0x8 {
            return Self::chcr_read_mask((offset >> 4) as u8, val);
        }
        val
    }

    /// The subset of CHCR bits a channel actually exposes on read. Channels 0-5 implement the full
    /// control set — direction, address step, chopping enable + window sizes, sync mode, start/busy,
    /// and the manual trigger (`0x7177_0703`). The OTC channel (6) is a stripped-down special case:
    /// only the start/trigger pair is writable and bit 1 (step-down) is hardwired to 1; every other
    /// bit reads back 0.
    fn chcr_read_mask(ch: u8, val: u32) -> u32 {
        if ch == 6 {
            (val & 0x5000_0000) | 0x0000_0002
        } else {
            val & 0x7177_0703
        }
    }

    pub fn write(&mut self, offset: u32, val: u32) {
        // Just latch the value; the transfer (if this was a CHCR start) is driven from the bus.
        self.regs[(offset >> 2) as usize & 0x1F] = val;
    }

    // ===== typed channel accessors (keep the index math in one place) =======================
    pub fn chan_madr(&self, ch: u8) -> u32 {
        self.regs[ch as usize * 4]
    }
    pub fn chan_bcr(&self, ch: u8) -> u32 {
        self.regs[ch as usize * 4 + 1]
    }
    pub fn chan_chcr(&self, ch: u8) -> u32 {
        self.regs[ch as usize * 4 + 2]
    }
    pub fn set_chan_madr(&mut self, ch: u8, v: u32) {
        self.regs[ch as usize * 4] = v;
    }
    pub fn set_chan_bcr(&mut self, ch: u8, v: u32) {
        self.regs[ch as usize * 4 + 1] = v;
    }
    pub fn set_chan_chcr(&mut self, ch: u8, v: u32) {
        self.regs[ch as usize * 4 + 2] = v;
    }

    /// DPCR (0x1F8010F0). Each channel owns a 4-bit nibble; bit 3 of channel `ch`'s nibble
    /// (bit `ch*4+3` overall) is its master enable. A channel won't start unless this is set.
    pub fn dpcr(&self) -> u32 {
        self.regs[0x1C]
    }
    pub fn channel_enabled(&self, ch: u8) -> bool {
        (self.dpcr() >> (ch * 4 + 3)) & 1 != 0
    }

    // ===== DICR — the DMA interrupt control register (0x1F8010F4) ===========================
    // Bit layout (the parts that matter):
    //   bit 15     force IRQ (raise regardless of the per-channel logic)
    //   bits 16-22 per-channel IRQ *enable* (channels 0-6)
    //   bit 23     master IRQ enable
    //   bits 24-30 per-channel IRQ *flags* — set by hardware on completion, WRITE-1-TO-CLEAR
    //   bit 31     master flag (READ-ONLY, computed below)
    // **Gotcha:** the flag bits are *write-1-to-clear*, the exact opposite of `I_STAT` (write-0).
    fn dicr_raw(&self) -> u32 {
        self.regs[0x1D]
    }

    /// The master flag (bit 31): force, OR (master-enable AND any flagged-and-enabled channel).
    /// This is the line that, on a 0->1 edge, raises `I_STAT` bit 3 (DMA).
    fn dicr_master_flag(d: u32) -> bool {
        let force = (d >> 15) & 1 != 0;
        let master_en = (d >> 23) & 1 != 0;
        let enables = (d >> 16) & 0x7F;
        let flags = (d >> 24) & 0x7F;
        force || (master_en && (flags & enables) != 0)
    }

    /// DICR as the CPU reads it: the stored bits with the live master flag placed in bit 31.
    pub fn dicr_read(&self) -> u32 {
        let d = self.dicr_raw();
        (d & 0x7FFF_FFFF) | ((Self::dicr_master_flag(d) as u32) << 31)
    }

    /// Writing DICR: bits 0-23 latch as written (force/enables/master-enable); bits 24-30 are the
    /// flags, which *clear* wherever a 1 is written (acknowledge); bit 31 is read-only.
    pub fn dicr_write(&mut self, val: u32) {
        let old_flags = self.dicr_raw() & 0x7F00_0000; // bits 24-30
        let cleared = old_flags & !(val & 0x7F00_0000); // 1 in val clears the matching flag
        self.regs[0x1D] = (val & 0x00FF_FFFF) | cleared; // bit 31 left 0; computed on read
    }

    /// Record that channel `ch` finished a transfer and return whether the DMA interrupt line just
    /// went high (a 0->1 edge of the master flag) — the bus uses that to pulse `I_STAT` bit 3.
    /// The completion flag latches only if that channel's IRQ-enable bit is set, matching hardware.
    pub fn signal_completion(&mut self, ch: u8) -> bool {
        let before = Self::dicr_master_flag(self.dicr_raw());
        if (self.dicr_raw() >> (16 + ch)) & 1 != 0 {
            self.regs[0x1D] |= 1 << (24 + ch);
        }
        let after = Self::dicr_master_flag(self.dicr_raw());
        !before && after
    }
}
