//! The joypad — the P1/JOYP register at 0xFF00.   *Implemented in milestone M6 (Liam).*
//!
//! Eight buttons share one register through a 2x4 matrix. The CPU writes bit 4 or
//! bit 5 to select either the direction pad or the action buttons, then reads the
//! low nibble. **Everything here is active-low: 0 = pressed, 1 = released** — and the
//! two *select* bits are active-low too (write a 0 to a line to select it). Pressing a
//! button (a high->low transition on a *selected* line) raises the Joypad interrupt.
//!
//! ```
//!   P1 / 0xFF00
//!   bit 7,6   unused        (always read 1)
//!   bit 5     select Action buttons   (0 = selected)   -> A B Select Start
//!   bit 4     select Direction pad    (0 = selected)   -> Right Left Up Down
//!   bit 3     down  / start           (0 = pressed)
//!   bit 2     up    / select          (0 = pressed)
//!   bit 1     left  / b               (0 = pressed)
//!   bit 0     right / a               (0 = pressed)
//! ```
//! Both lines can be selected at once (the CPU sees the AND of them); with neither
//! selected the low nibble reads all-1s.

use crate::interrupts::{Interrupts, JOYPAD};

// Our *internal* button bitmask — `1` = pressed (the intuitive sense; we invert to
// active-low only at read time). The layout is chosen so each button sits on the P1
// bit it lands on once its line is selected: the d-pad fills the low nibble (Right=bit0
// .. Down=bit3) and the action buttons fill the high nibble (A=bit4 .. Start=bit7, i.e.
// the same 0..3 order shifted up by four). The host (main.rs) ORs these together.
pub const BTN_RIGHT:  u8 = 1 << 0;
pub const BTN_LEFT:   u8 = 1 << 1;
pub const BTN_UP:     u8 = 1 << 2;
pub const BTN_DOWN:   u8 = 1 << 3;
pub const BTN_A:      u8 = 1 << 4;
pub const BTN_B:      u8 = 1 << 5;
pub const BTN_SELECT: u8 = 1 << 6;
pub const BTN_START:  u8 = 1 << 7;

pub struct Joypad {
    /// The selector the CPU last wrote (bits 4-5). Active-low: a 0 *selects* that line.
    select: u8,
    /// Live button state, `1` = pressed. Replaced by the host once per frame via
    /// `set_buttons`. Keeping last frame's value here is also what lets us spot the
    /// release->press *edge* that fires the interrupt.
    buttons: u8,
}

impl Joypad {
    pub fn new() -> Self {
        // 0x30 = both select lines high (nothing selected) — the post-boot state.
        Self { select: 0x30, buttons: 0x00 }
    }

    /// Read P1/JOYP (0xFF00). Assemble the active-low register the CPU expects.
    pub fn read(&self) -> u8 {
        // TODO(M6): build the active-low low nibble, then dress it up.
        //   1. start from 0x0F (all four lines released — remember, 1 = released).
        //   2. if the DIRECTION line is selected (select bit 4 == 0), clear the bit of
        //      every pressed d-pad button (the low nibble of `self.buttons`).
        //   3. if the ACTION line is selected (select bit 5 == 0), clear the bit of
        //      every pressed action button (the high nibble of `self.buttons`, shifted
        //      down into 0..3).
        //   4. OR the selector bits (self.select) and the two always-1 top bits back on.
        // todo!("M6: read() — merge select + buttons into the active-low P1 register")

        let mut lines = 0x0F;
        if self.select & 0x10 == 0 {
            lines &= !(self.buttons & 0x0F);
        }

        if self.select & 0x20 == 0 {
            lines &= !(self.buttons >> 4);
        }
        0xC0 | self.select | (lines & 0x0F)
    }

    pub fn write(&mut self, val: u8) {
        // Only bits 4-5 (the line selects) are writable; the buttons are inputs.
        self.select = val & 0x30;
    }

    /// Host hook: replace the live button state (call once per frame). `pressed` uses
    /// the BTN_* layout above (`1` = pressed). A fresh press is a high->low edge on the
    /// wire, which is what raises the Joypad interrupt — hence we take `&mut Interrupts`,
    /// exactly the way the timer/PPU are handed the interrupt line in `Bus::tick`.
    pub fn set_buttons(&mut self, pressed: u8, ints: &mut Interrupts) {
        // TODO(M6):
        //   1. work out which buttons are *newly* pressed this frame: set in `pressed`
        //      AND clear in the old `self.buttons`. That release->press edge is the IRQ
        //      trigger — holding a button must NOT keep re-firing it.
        //   2. latch the new state: self.buttons = pressed.
        //   3. if any newly-pressed button is on a currently-selected line, raise the
        //      Joypad interrupt: bring `JOYPAD` into the `use` above and call
        //      `ints.request(JOYPAD)`. (Good first cut: fire on *any* newly-pressed
        //      button; tighten to "only on a selected line" once it works — Tetris is
        //      happy either way.)
        //let _ = (pressed, ints); // remove once you use them
        //todo!("M6: set_buttons() — edge-detect a fresh press and request JOYPAD")

        let newly_pressed = pressed & !self.buttons;
        self.buttons = pressed;
        if newly_pressed != 0 {
            ints.request(JOYPAD);
        }
    }
}
