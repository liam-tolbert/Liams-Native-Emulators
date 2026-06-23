//! The three root counters (hardware timers), a bus device at 0x1F801100-0x1F80112F.
//!
//! The PS1 has **three identical 16-bit counters**, TIMER0/1/2. Each freely counts up at a
//! programmable rate and can raise an interrupt when it hits a target value or wraps past
//! 0xFFFF — the machinery games lean on for frame pacing, delays, and music timing. This is the
//! exact role the Game Boy's DIV/TIMA timer played (`gameboy/src/timer.rs`); the shape is the
//! same — a counter advanced from the CPU's "catch-up" tick that raises an IRQ on a boundary —
//! just three of them, with a richer control register.
//!
//! Register map — each timer `n` lives at base `0x1F801100 + n*0x10`, three 32-bit-addressable
//! registers (only the low 16 bits are real):
//! ```
//!   base + 0x0   counter value   (R/W)
//!   base + 0x4   counter mode    (R/W control + read-only status latches)
//!   base + 0x8   counter target  (R/W)
//! ```
//!
//! What each counter counts is selectable (mode bits 8-9):
//!   * TIMER0: system clock, or the GPU **dot clock**
//!   * TIMER1: system clock, or the GPU **hblank** (one tick per scanline)
//!   * TIMER2: system clock, or **system clock / 8**
//!
//! The GPU-derived sources (dot clock, hblank) and the blanking signals the sync modes gate on come
//! in through the `VideoTiming` the GPU returns each `bus.tick` (see `gpu.rs`); the bus threads it
//! to `Timers::tick`. The **sync modes** (mode bits 0-2) pause or reset a counter around the
//! horizontal/vertical blank — TIMER0 syncs to hblank, TIMER1 to vblank, and TIMER2's two bits only
//! choose stop vs free-run (`apply_sync`/`gate_blank`). Their exact counts are approximate under our
//! coarse cycle model; the *shapes* (pause = flat, reset = sawtooth, stop = 0) are what they pin.

use std::cell::Cell;

use crate::gpu::VideoTiming;
use crate::irq::source;

/// One root counter. The three differ only in how mode bits 8-9 decode (the `n` field branches
/// that), so the state and the counting logic are shared.
struct Timer {
    /// Which counter this is (0/1/2) — selects the clock-source meaning of mode bits 8-9.
    n: u8,

    /// The live count (register base+0x0). Reads zero-extend to 32 bits.
    value: u16,
    /// Compare value (base+0x8). The counter can reset and/or raise an IRQ when `value == target`.
    target: u16,
    /// Mode/control (base+0x4) — but we keep only the **written** bits (0-9: the sync config and
    /// clock-source select). Bits 10-12 are status that hardware drives, not storage, so they are
    /// *assembled* in `read`, never stored here (see `irq_flag`/`reached_*`). Masking the stored
    /// word to 0x03FF is the whole reason a status bit can't accidentally be written by software.
    mode: u16,

    /// Bit 11 status latch: "the counter reached its target since you last looked". Set by the
    /// counting logic, and **cleared by the act of reading the mode register** — a genuine
    /// read-side-effect, which is why it's a `Cell`: the bus read path is all `&self`, and confining
    /// the interior mutability to exactly the two fields hardware clears on read keeps every CPU
    /// load/fetch call site untouched (the alternative, a `&mut self` read path, churns the whole
    /// fetch chain and invites double-borrows).
    reached_target: Cell<bool>,
    /// Bit 12 status latch: "the counter wrapped past 0xFFFF since you last looked". Same
    /// cleared-on-mode-read rule as `reached_target`.
    reached_max: Cell<bool>,

    /// Bit 10, the interrupt flag — but its *value* is state, not the bit position. It reads **high**
    /// normally; a write to the mode register forces it high; firing in "pulse" mode drives it low;
    /// firing in "toggle" mode flips it. Assembled into the read at bit 10.
    irq_flag: bool,
    /// One-shot guard. In one-shot mode (mode bit 6 = 0) a counter raises its interrupt only **once**
    /// until the mode register is rewritten; this latches that "already fired" so repeats are
    /// suppressed. Repeat mode (bit 6 = 1) ignores it.
    irq_fired: bool,

    /// Prescaler remainder for the system-clock/8 source (TIMER2 only). Cycles arrive in arbitrary
    /// chunk sizes from `bus.tick`, so the leftover (0..7) after dividing by 8 must **carry** into the
    /// next call or the /8 rate drifts.
    div8_acc: u8,

    /// Sync-mode-3 latch. Modes 0-2 re-decide every step, but mode 3 ("pause until the FIRST blank
    /// event, then free-run forever") needs one bit of memory: has that first hblank/vblank arrived
    /// since the last mode write? Reset on a mode write (re-arms the wait). Unused by TIMER2.
    sync_started: bool,
}

impl Timer {
    fn new(n: u8) -> Self {
        // Power-on: everything zero except the interrupt flag, which reads high (no IRQ pending).
        // Unlike the Game Boy timer there's no boot-ROM HLE state to bake in — the PS1 BIOS programs
        // the timers itself during kernel init.
        Self {
            n,
            value: 0,
            target: 0,
            mode: 0,
            reached_target: Cell::new(false),
            reached_max: Cell::new(false),
            irq_flag: true,
            irq_fired: false,
            div8_acc: 0,
            sync_started: false,
        }
    }

    /// Advance this counter for one `bus.tick`. First pick the **raw** number of source ticks (the
    /// selected clock produced), then run it through the sync gate (which may pause the count or reset
    /// the counter around h/v-blank), then step. `v` carries the GPU's per-step video timing. Returns
    /// whether an enabled IRQ condition fired.
    fn advance(&mut self, cycles: u32, v: &VideoTiming) -> bool {
        // Mode bits 8-9 select the clock source; the meaning differs per counter (see the module head).
        let src = (self.mode >> 8) & 3;
        let raw = match self.n {
            // TIMER0: src 0/2 = system clock, 1/3 = the GPU dot clock.
            0 => if src & 1 == 0 { cycles } else { v.dotclocks },
            // TIMER1: src 0/2 = system clock, 1/3 = hblank (one tick per scanline).
            1 => if src & 1 == 0 { cycles } else { v.hblanks },
            // TIMER2: src 0/1 = system clock, 2/3 = system clock / 8.
            2 => if src < 2 { cycles } else { self.div8(cycles) },
            _ => 0,
        };
        let ticks = self.apply_sync(raw, v);
        self.step(ticks)
    }

    /// Apply the sync gate. Mode bit 0 enables sync; bits 1-2 pick the mode. When disabled the counter
    /// free-runs (the raw ticks pass straight through). The behaviour differs per counter: TIMER0
    /// syncs to **hblank**, TIMER1 to **vblank**, and TIMER2 only "stops" or "free-runs". Returns the
    /// number of ticks to actually advance this step (0 = paused), after any reset-to-0 the mode calls
    /// for.
    fn apply_sync(&mut self, raw: u32, v: &VideoTiming) -> u32 {
        if self.mode & 1 == 0 {
            return raw; // sync disabled -> free-run, bits 1-2 ignored
        }
        let sync = (self.mode >> 1) & 3;
        match self.n {
            0 => self.gate_blank(raw, sync, v.in_hblank, v.hblank_edge), // syncs to hblank
            1 => self.gate_blank(raw, sync, v.in_vblank, v.vblank_edge), // syncs to vblank
            // TIMER2's two sync bits only choose stop vs free-run — and the polarity is INVERTED from
            // TIMER0/1: modes 0 and 3 *stop* the counter dead, 1 and 2 *free-run* (sync has no effect).
            2 => if sync == 0 || sync == 3 { 0 } else { raw },
            _ => 0,
        }
    }

    /// The shared TIMER0/TIMER1 hblank/vblank sync gate — the four modes from psx-spx. `blank` is the
    /// beam's in-blank level at the end of the step; `edge` is whether a blank period *started* during
    /// it. Reset modes use the edge; pause modes use the level (see the "counts vs flags" note on
    /// `VideoTiming`).
    fn gate_blank(&mut self, raw: u32, sync: u16, blank: bool, edge: bool) -> u32 {
        match sync {
            // 0: pause *during* blank, count the rest of the time.
            0 => if blank { 0 } else { raw },
            // 1: reset the counter to 0 at each blank start; otherwise count freely.
            1 => {
                if edge {
                    self.value = 0;
                }
                raw
            }
            // 2: reset at the blank start AND only count *while* in blank (paused outside it).
            2 => {
                if edge {
                    self.value = 0;
                }
                if blank { raw } else { 0 }
            }
            // 3: stay paused until the first blank occurs, then free-run forever.
            _ => {
                if !self.sync_started {
                    if edge {
                        self.sync_started = true;
                    } else {
                        return 0; // still waiting for the first blank
                    }
                }
                raw
            }
        }
    }

    /// System-clock/8 prescaler: divide incoming cycles by 8, carrying the remainder across calls.
    fn div8(&mut self, cycles: u32) -> u32 {
        let total = self.div8_acc as u32 + cycles;
        self.div8_acc = (total % 8) as u8;
        total / 8
    }

    /// Advance the counter by `ticks`, one tick at a time. Stepping singly (rather than adding
    /// `ticks` in one go) is what lets us catch a target hit *and* a 0xFFFF wrap as the distinct
    /// per-tick events they are — a coarse "+= ticks" would skip straight over a boundary and miss the
    /// latch/IRQ. Returns whether an enabled IRQ condition fired during the run.
    fn step(&mut self, ticks: u32) -> bool {
        // Mode bit 3 picks *when* the counter resets to 0: 0 = on the 0xFFFF wrap, 1 = on the target.
        let reset_on_target = self.mode & (1 << 3) != 0;
        let mut want_irq = false;
        for _ in 0..ticks {
            let prev = self.value;
            self.value = self.value.wrapping_add(1);

            if self.value == self.target {
                self.reached_target.set(true); // bit 11 latch
                if self.mode & (1 << 4) != 0 {
                    want_irq |= self.fire_irq(); // bit 4: IRQ on target
                }
                if reset_on_target {
                    self.value = 0;
                }
            }
            // The 0xFFFF -> 0x0000 wrap. (When resetting on target with target < 0xFFFF the counter
            // never gets here, so `reached_max` correctly stays clear.)
            if prev == 0xFFFF && self.value == 0x0000 {
                self.reached_max.set(true); // bit 12 latch
                if self.mode & (1 << 5) != 0 {
                    want_irq |= self.fire_irq(); // bit 5: IRQ on 0xFFFF
                }
            }
        }
        want_irq
    }

    /// Apply the IRQ once a condition is met, honouring one-shot/repeat (bit 6) and pulse/toggle
    /// (bit 7). Returns `true` when the interrupt line should actually be pulled this time — the bus
    /// turns that into the I_STAT bit (the cross-device raise stays on the bus, the same rule DMA
    /// follows). The `I_STAT` bit itself latches there until software acknowledges it, so "pulse" need
    /// only signal the asserting edge here.
    fn fire_irq(&mut self) -> bool {
        let repeat = self.mode & (1 << 6) != 0;
        if !repeat && self.irq_fired {
            return false; // one-shot already fired: silent until the mode register is rewritten
        }
        self.irq_fired = true;
        if self.mode & (1 << 7) != 0 {
            // Toggle mode: flip bit 10; on hardware the interrupt asserts on the high->low edge, so
            // we only pull the line on the tick that drove it low.
            self.irq_flag = !self.irq_flag;
            !self.irq_flag
        } else {
            // Pulse mode: bit 10 momentarily reads low — that drop is the asserting edge. (We leave it
            // low until the next mode write restores it; a few-clock auto-restore isn't worth modelling
            // until a test needs it.)
            self.irq_flag = false;
            true
        }
    }
}

/// The three counters as one bus-owned device — mirrors how `Dma` owns all its channels. Keeping the
/// `offset -> (channel, register)` decode in one place (and the cross-device IRQ raise on the bus)
/// matches the established device pattern.
pub struct Timers {
    ch: [Timer; 3],
}

impl Timers {
    pub fn new() -> Self {
        Self { ch: [Timer::new(0), Timer::new(1), Timer::new(2)] }
    }

    /// Advance all three counters by one `bus.tick` (`cycles` system-clock cycles, plus the GPU video
    /// timing `v` that feeds the dot-clock/hblank sources and the sync gates). Returns a 3-bit mask of
    /// which timers fired an IRQ this step (bit n => TIMERn); the bus raises the matching I_STAT sources.
    pub fn tick(&mut self, cycles: u32, v: &VideoTiming) -> u8 {
        let mut fired = 0u8;
        for n in 0..3 {
            if self.ch[n].advance(cycles, v) {
                fired |= 1 << n;
            }
        }
        fired
    }

    /// Map a fired-mask bit back to its interrupt source constant — used by the bus to raise IRQs.
    pub fn irq_source(n: u8) -> u16 {
        [source::TIMER0, source::TIMER1, source::TIMER2][n as usize]
    }

    /// Read a timer register. `offset` is relative to the timer block base (0x00..0x2F): bits 4-5
    /// pick the counter (0x10 stride), the low nibble picks value/mode/target. The whole path is
    /// `&self` — the only mutation is clearing the two status latches when the mode register is read,
    /// done through their `Cell`s.
    pub fn read(&self, offset: u32) -> u32 {
        let t = &self.ch[((offset >> 4) & 3) as usize];
        match offset & 0xF {
            0x0 => t.value as u32,
            0x4 => {
                // Assemble the mode word: the stored config bits (0-9) plus the live status bits.
                let mut m = t.mode & 0x03FF;
                m |= (t.irq_flag as u16) << 10;
                m |= (t.reached_target.get() as u16) << 11;
                m |= (t.reached_max.get() as u16) << 12;
                // Reading the mode register clears both reached-* latches (a real read side effect).
                t.reached_target.set(false);
                t.reached_max.set(false);
                m as u32
            }
            0x8 => t.target as u32,
            _ => 0,
        }
    }

    /// Write a timer register. Writing the **mode** register has side effects beyond storing it:
    /// hardware resets the counter to 0, sets the IRQ flag high (bit 10), and re-arms the one-shot.
    pub fn write(&mut self, offset: u32, val: u32) {
        let t = &mut self.ch[((offset >> 4) & 3) as usize];
        match offset & 0xF {
            0x0 => t.value = val as u16, // direct counter write; does not touch the latches
            0x4 => {
                // Store only the config bits (0-9); the status bits (10-12) are not software-writable.
                t.mode = (val as u16) & 0x03FF;
                // The four documented side effects of a mode write:
                t.value = 0; // counter restarts
                t.irq_flag = true; // bit 10 -> high (no IRQ)
                t.irq_fired = false; // re-arm the one-shot
                t.reached_target.set(false); // clear the status latches
                t.reached_max.set(false);
                t.div8_acc = 0; // restart the /8 prescaler cleanly
                t.sync_started = false; // re-arm sync mode 3's "wait for first blank"
            }
            0x8 => t.target = val as u16,
            _ => {}
        }
    }
}
