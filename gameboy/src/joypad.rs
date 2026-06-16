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

    /// Read P1/JOYP (0xFF00) — rebuild the byte the CPU sees, from scratch, every
    /// time. There is no stored "register value": the byte is a *view* assembled
    /// from the select bits the CPU last wrote (`self.select`) and the live button
    /// state (`self.buttons`). Only four lines (bits 0-3) are ever readable, so the
    /// eight buttons take turns on them depending on which group is selected.
    pub fn read(&self) -> u8 {
        // The four input lines are active-low: a *released* line reads 1, a *pressed*
        // line reads 0. So start with all four released (0b0000_1111) and clear one
        // bit for each button that is actually held down.
        let mut lines = 0x0F;

        // The CPU "selects" a group by pulling its select bit LOW (0 = selected).
        // Bit 4 selects the direction pad. Our d-pad buttons already sit in the low
        // nibble of `self.buttons` at the exact positions the register uses
        // (Right=bit0, Left=bit1, Up=bit2, Down=bit3), but with our convention
        // "1 = pressed". `!(...)` flips each pressed bit to 0; AND-ing into `lines`
        // clears those lines (pressed -> 0) and leaves released lines as 1.
        if self.select & 0x10 == 0 {
            lines &= !(self.buttons & 0x0F);
        }

        // Bit 5 selects the action buttons. They live in the *high* nibble of
        // `self.buttons` (A=bit4, B=bit5, Select=bit6, Start=bit7); shifting right by
        // 4 drops them onto lines 0-3 — the same four output lines the d-pad uses.
        // If the CPU ever selects BOTH groups at once, both AND steps run and the
        // result is the bitwise-AND of the two groups, which is exactly what the real
        // shared wiring does (any button held in either group pulls its line low).
        if self.select & 0x20 == 0 {
            lines &= !(self.buttons >> 4);
        }

        // Assemble the full byte: bits 7-6 are unused and always read 1 (0xC0); bits
        // 5-4 read back the select bits the CPU wrote (already masked to 0x30 in
        // `write`); bits 3-0 are the active-low input lines we just built.
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
        // Edge-detect, don't level-detect. The Joypad interrupt fires the *instant* a
        // button goes from up to down — not once per frame it stays held. A bit is a
        // fresh press only if it is set NOW (`pressed`) and was clear LAST frame
        // (`!self.buttons`), so AND-ing those keeps exactly the buttons pressed this
        // frame. (Level-detecting here would re-fire the IRQ every frame a key is held
        // and drown the CPU in interrupts.)
        let newly_pressed = pressed & !self.buttons;

        // Latch the new state: this is what `read` reports as "currently held", and it
        // becomes "last frame" for the next call's edge comparison.
        self.buttons = pressed;

        // Any fresh press raises the interrupt request bit (the CPU services it only if
        // the Joypad interrupt is enabled). Real hardware is fussier — it only raises
        // the IRQ for a press on a currently-*selected* line — but firing on any press
        // is simpler and harmless: almost no game relies on this interrupt for input.
        // Games read the buttons by polling 0xFF00 every frame; the IRQ mostly exists
        // to wake the CPU out of the low-power STOP state.
        if newly_pressed != 0 {
            ints.request(JOYPAD);
        }
    }
}
