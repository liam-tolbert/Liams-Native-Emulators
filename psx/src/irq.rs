//! The interrupt controller (a bus device at 0x1F801070-0x1F801077).
//!
//! Every PS1 interrupt source — VBlank, GPU, CD-ROM, the timers, the controllers, DMA —
//! sets a bit in `I_STAT`. `I_MASK` enables the ones the kernel cares about. When any
//! enabled-and-pending bit is set, the controller pulls the CPU's single external
//! interrupt line (COP0 `Cause.IP2`) high. This is the exact "subsystems raise a line,
//! the CPU services it" shape the Game Boy used — just one indirection more, because the
//! PS1 funnels ~11 sources through one CPU line.
//!
//! Early on nothing *raises* an interrupt yet (`raise` is unused
//! until the GPU/timers land), so the line stays low — but the plumbing is here so the
//! CPU's interrupt check is real, not a stub.

/// `I_STAT`/`I_MASK` bit positions (only a few matter before the GPU exists).
pub mod source {
    pub const VBLANK: u16 = 0;
    pub const GPU: u16 = 1;
    pub const CDROM: u16 = 2;
    pub const DMA: u16 = 3;
    pub const TIMER0: u16 = 4;
    pub const TIMER1: u16 = 5;
    pub const TIMER2: u16 = 6;
    pub const CONTROLLER: u16 = 7;
}

pub struct Irq {
    stat: u16, // I_STAT (0x1F801070) — pending bits, set by devices, acked by the CPU
    mask: u16, // I_MASK (0x1F801074) — which pending bits are allowed through
}

impl Irq {
    pub fn new() -> Self {
        Self { stat: 0, mask: 0 }
    }

    /// True when an enabled interrupt is waiting — this is what drives `Cause.IP2`.
    pub fn pending(&self) -> bool {
        self.stat & self.mask != 0
    }

    /// A device requests an interrupt by setting its `I_STAT` bit. (Unused until the GPU
    /// and timers exist, but devices will call this through the bus.)
    pub fn raise(&mut self, source: u16) {
        self.stat |= 1 << source;
    }

    pub fn read_stat(&self) -> u16 {
        self.stat
    }
    pub fn read_mask(&self) -> u16 {
        self.mask
    }

    /// Writing `I_STAT` *acknowledges*: a 0 in any bit clears that pending interrupt, a 1
    /// leaves it set. (This is the opposite of "write 1 to clear" — easy to get backwards.)
    pub fn ack(&mut self, val: u16) {
        self.stat &= val;
    }

    pub fn write_mask(&mut self, val: u16) {
        self.mask = val;
    }
}
