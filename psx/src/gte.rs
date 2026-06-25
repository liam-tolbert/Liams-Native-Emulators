//! GTE — the Geometry Transformation Engine (MIPS coprocessor 2).
//!
//! The GTE is the PS1's fixed-point 3D-math unit: it transforms/projects vertices, does the
//! perspective divide, and shades with light/colour matrices. Like COP0 (and unlike the Game Boy's
//! bus devices), a MIPS coprocessor is *part of the CPU*, so `Gte` is owned directly by `Cpu` and the
//! CPU reaches it through the COP2 instructions (`MFC2/CFC2/MTC2/CTC2`, `LWC2/SWC2`, and the GTE
//! command words).
//!
//! **This module is M6.0: the register file + the moves only — no geometry math yet.** The 32 data
//! and 32 control registers are modelled here with their (many) read/write quirks, because those
//! quirks are exactly what the `ps1-tests` `gte/test-all` ROM checks first. The compute commands
//! (RTPS, NCLIP, the lighting family, ...) arrive in later sub-stages; `command` is a stub for now.
//!
//! Everything here is derived clean-room from Nocash **psx-spx** ("GTE Registers" / "GTE Opcodes").

/// Sign-extend the low 16 bits of `v` to 32. Several GTE registers physically hold a 16-bit signed
/// value in a 32-bit slot and read back **sign-extended** — the #1 register quirk the test ROM pins.
#[inline]
fn sx16(v: u32) -> u32 {
    v as i16 as u32 // `as i16` keeps the low 16 bits; `i16 as u32` sign-extends them
}

/// FLAG (control reg 31) bit-31 "error summary" = OR of the saturation/overflow bits 30..23 and
/// 18..13 (psx-spx). The other flag bits (the colour-FIFO and IR0 ones) deliberately don't feed it.
const FLAG_ERR_MASK: u32 = 0x7F87_E000;

pub struct Gte {
    /// COP2 *data* registers (cop2d0..31): the vectors, the screen/colour FIFOs, the IR/MAC working
    /// set. Stored as raw 32-bit words; the read/write quirks live in the accessors below.
    data: [u32; 32],
    /// COP2 *control* registers (cop2c0..31): the rotation/light/colour matrices, the translation and
    /// background/far-colour vectors, the screen offset + projection constants, and FLAG.
    ctrl: [u32; 32],
}

impl Gte {
    pub fn new() -> Self {
        Self { data: [0; 32], ctrl: [0; 32] }
    }

    // ===== data registers (MFC2 reads / MTC2 writes) ===========================================

    /// `MTC2 rt, rd` — write data register `rd`. Most slots store the word verbatim; the handful with
    /// *write* side-effects are spelled out (the read-side extensions happen in `read_data`).
    pub fn write_data(&mut self, reg: usize, val: u32) {
        match reg {
            // SXYP (d15): the screen-XY FIFO's "push" port. Writing it shifts the 3-deep FIFO down
            // (SXY0 <- SXY1 <- SXY2) and drops the new value in SXY2 — this is how RTPT-style code
            // feeds three projected points through a moving window. (Reading d15 mirrors SXY2.)
            15 => {
                self.data[12] = self.data[13];
                self.data[13] = self.data[14];
                self.data[14] = val;
            }
            // IRGB (d28): writing a packed 5:5:5 colour expands it into IR1,IR2,IR3 (each 5-bit field
            // scaled up by 0x80). Reading d28 doesn't return this word — it returns the re-packed ORGB.
            28 => {
                self.data[9] = (val & 0x1F) << 7;
                self.data[10] = ((val >> 5) & 0x1F) << 7;
                self.data[11] = ((val >> 10) & 0x1F) << 7;
            }
            // ORGB (d29) and LZCR (d31) are computed-on-read / read-only — writes are ignored.
            29 | 31 => {}
            _ => self.data[reg] = val,
        }
    }

    /// `MFC2 rt, rd` — read data register `rd`, applying the documented read quirks.
    pub fn read_data(&self, reg: usize) -> u32 {
        match reg {
            1 | 3 | 5 => sx16(self.data[reg]),  // VZ0/VZ1/VZ2 — 16-bit signed
            8..=11 => sx16(self.data[reg]),     // IR0/IR1/IR2/IR3 — 16-bit signed
            7 => self.data[7] & 0xFFFF,         // OTZ — 16-bit unsigned (zero-extended)
            16..=19 => self.data[reg] & 0xFFFF, // SZ0..SZ3 — 16-bit unsigned (zero-extended)
            15 => self.data[14],                // SXYP — reading mirrors SXY2 (the FIFO top)
            28 | 29 => self.orgb(),             // IRGB/ORGB both read back the packed-from-IR colour
            31 => self.lzcr(),                  // LZCR — count of LZCS's leading sign bits
            _ => self.data[reg],
        }
    }

    /// ORGB (d29, and the read of d28): pack IR1,IR2,IR3 back into a 5:5:5 colour, each channel
    /// arithmetic-shifted right by 7 and clamped to 0..1Fh.
    fn orgb(&self) -> u32 {
        let c = |ir: u32| ((ir as i16 as i32) >> 7).clamp(0, 0x1F) as u32;
        c(self.data[9]) | (c(self.data[10]) << 5) | (c(self.data[11]) << 10)
    }

    /// LZCR (d31): the count of leading bits of LZCS (d30) equal to its sign bit — leading zeros if
    /// LZCS is positive, leading ones if negative. Always 1..32 (and 32 for 0 / 0xFFFFFFFF). Games
    /// use it as a fast normaliser (e.g. to find a shift for the perspective divide).
    fn lzcr(&self) -> u32 {
        let v = self.data[30];
        if v & 0x8000_0000 != 0 { (!v).leading_zeros() } else { v.leading_zeros() }
    }

    // ===== control registers (CFC2 reads / CTC2 writes) ========================================

    /// `CTC2 rt, rd` — write control register `rd`. Only FLAG has a write side-effect.
    pub fn write_ctrl(&mut self, reg: usize, val: u32) {
        match reg {
            // FLAG (c31): store the meaningful bits (30..12; 11..0 are always 0) and recompute the
            // bit-31 error summary from the saturation/overflow bits, exactly as the hardware does.
            31 => {
                self.ctrl[31] = val & 0x7FFF_F000;
                if self.ctrl[31] & FLAG_ERR_MASK != 0 {
                    self.ctrl[31] |= 0x8000_0000;
                }
            }
            _ => self.ctrl[reg] = val,
        }
    }

    /// `CFC2 rt, rd` — read control register `rd`, applying the read quirks.
    pub fn read_ctrl(&self, reg: usize) -> u32 {
        match reg {
            4 | 12 | 20 => sx16(self.ctrl[reg]), // R33 / L33 / LL33 — 16-bit signed matrix corners
            // H (c26): the projection-plane distance. It's *used* as unsigned, but a hardware quirk
            // makes it read back SIGN-extended — a specific case the test ROM checks.
            26 => sx16(self.ctrl[26]),
            27 | 29 | 30 => sx16(self.ctrl[reg]), // DQA / ZSF3 / ZSF4 — 16-bit signed
            _ => self.ctrl[reg],
        }
    }

    // ===== GTE command words (the CO-bit ops) — implemented in later sub-stages =================

    /// Execute a GTE command (RTPS, NCLIP, the lighting family, ...). **M6.0 stub:** the register
    /// file + moves ship first, so a command word is accepted and does nothing yet (the geometry math
    /// lands in M6.1+). It does *not* fault — an un-recognised coprocessor op is a NOP on MIPS.
    pub fn command(&mut self, _op: u32) {}
}
