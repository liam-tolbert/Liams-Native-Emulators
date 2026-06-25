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

/// The GTE perspective-divide's Newton-Raphson seed. psx-spx gives both a 257-entry `unr_table` literal
/// and the generator formula it's built from; we compute it from the formula so there's no 256-value
/// table to transcribe (and mis-transcribe). index 0..256 → 0..0xFF.
fn unr_seed(index: u64) -> u64 {
    let v = (0x4_0000i64 / (index as i64 + 0x100) + 1) / 2 - 0x101;
    v.clamp(0, 0xFF) as u64
}

/// Wrap a MAC accumulation to **44-bit signed**: the GTE's MAC1/2/3 accumulators are 44 bits, so a
/// result beyond that range loses its top bits (and can flip sign) — which the SZ/IR pipeline then
/// sees. Sign-extends from bit 43. (Values already inside 44 bits pass through unchanged.)
#[inline]
fn wrap44(v: i64) -> i64 {
    (v << 20) >> 20
}

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

    // ===== GTE command words (the CO-bit ops) ==================================================
    // Each command reads its inputs from the registers, runs a fixed-point pipeline through the MAC
    // accumulators and the IR registers (with saturation + the FLAG error bits), and writes its
    // outputs back. Clean-room from psx-spx "GTE Opcode Summary" and the per-opcode pages. M6.1a does
    // the non-divide commands (MVMVA/NCLIP/AVSZ3/AVSZ4); RTPS/RTPT (the perspective divide) are M6.1b.

    /// Execute a GTE command word. The low 6 bits select the opcode; the `sf`/`lm`/`mx`/`v`/`cv`
    /// fields tune the fixed-point pipeline (shift, IR clamp, and — for MVMVA — which matrix/vector/
    /// translation). FLAG is recomputed from scratch each command.
    pub fn command(&mut self, op: u32) {
        let cmd = op & 0x3F;
        let sf = if op & (1 << 19) != 0 { 12 } else { 0 }; // shift-fraction: results >>12, or >>0
        let lm = op & (1 << 10) != 0; // IR saturation lower bound: 0 (set) or -0x8000 (clear)
        let mx = ((op >> 17) & 3) as usize; // MVMVA matrix select
        let vx = ((op >> 15) & 3) as usize; // MVMVA vector select
        let cv = ((op >> 13) & 3) as usize; // MVMVA translation select
        self.ctrl[31] = 0; // FLAG is built fresh by every command
        match cmd {
            0x01 => self.rtps(sf, lm),
            0x06 => self.nclip(),
            0x0C => self.op(sf, lm),
            0x12 => self.mvmva(sf, lm, mx, vx, cv),
            0x2D => self.avsz3(),
            0x2E => self.avsz4(),
            0x30 => self.rtpt(sf, lm),
            _ => {} // not yet implemented: the lighting family (M6.2), SQR/GPF/GPL (M6.3)
        }
        // bit 31 = OR of the saturation/overflow error bits set above (the FLAG error summary).
        if self.ctrl[31] & FLAG_ERR_MASK != 0 {
            self.ctrl[31] |= 0x8000_0000;
        }
    }

    // ----- the fixed-point pipeline primitives -------------------------------------------------
    #[inline]
    fn flag(&mut self, bit: u32) {
        self.ctrl[31] |= 1 << bit;
    }

    /// Write MACn (n=1..3): flag a 44-bit-signed overflow on the *unshifted* accumulation, then store
    /// the value shifted right by `sf` (0 or 12). Returns the **32-bit** MAC register value (sign-
    /// extended) — crucially NOT the full i64: the IR saturator clamps the truncated 32-bit MAC, so a
    /// 44-bit result whose low 32 bits flip sign (e.g. a huge negative whose low word is positive)
    /// saturates toward that 32-bit sign, exactly as hardware does. (ps1-tests gte/test-all pins this.)
    fn set_mac(&mut self, n: usize, val: i64, sf: u32) -> i64 {
        if val > 0x7FF_FFFF_FFFF {
            self.flag(31 - n as u32); // MAC1/2/3 positive overflow -> bits 30/29/28
        } else if val < -0x800_0000_0000 {
            self.flag(28 - n as u32); // MAC1/2/3 negative overflow -> bits 27/26/25
        }
        let r = wrap44(val) >> sf; // the accumulator is 44-bit; wrap before the fractional shift
        self.data[24 + n] = r as u32;
        r as i32 as i64
    }

    /// Saturate MACn into IRn (n=1..3): clamp to [-0x8000, 0x7FFF] (lm=0) or [0, 0x7FFF] (lm=1).
    fn set_ir(&mut self, n: usize, val: i64, lm: bool) {
        let lo = if lm { 0 } else { -0x8000 };
        let sat = val.clamp(lo, 0x7FFF);
        if sat != val {
            self.flag(25 - n as u32); // IR1/2/3 saturated -> bits 24/23/22
        }
        self.data[8 + n] = sat as i32 as u32;
    }

    /// Write MAC0: flag a 32-bit-signed overflow, then store.
    fn set_mac0(&mut self, val: i64) {
        if val > 0x7FFF_FFFF {
            self.flag(16);
        } else if val < -0x8000_0000 {
            self.flag(15);
        }
        self.data[24] = val as u32;
    }

    /// Saturate a Z-average into OTZ, clamped to [0, 0xFFFF].
    fn set_otz(&mut self, val: i64) {
        let sat = val.clamp(0, 0xFFFF);
        if sat != val {
            self.flag(18);
        }
        self.data[7] = sat as u32;
    }

    /// (SXn, SYn) of screen-XY FIFO register `reg` (12..14), each signed 16-bit.
    fn sxy(&self, reg: usize) -> (i64, i64) {
        let v = self.data[reg];
        (v as i16 as i64, (v >> 16) as i16 as i64)
    }
    /// SZn (16..19) as an unsigned 16-bit value.
    fn sz(&self, reg: usize) -> i64 {
        (self.data[reg] & 0xFFFF) as i64
    }

    // ----- the non-divide commands -------------------------------------------------------------
    /// NCLIP (0x06): the signed area (cross-product Z) of the three screen points in the SXY FIFO —
    /// games test its sign for backface culling. MAC0 = SX0*SY1 + SX1*SY2 + SX2*SY0 - SX0*SY2 -
    /// SX1*SY0 - SX2*SY1.
    fn nclip(&mut self) {
        let (sx0, sy0) = self.sxy(12);
        let (sx1, sy1) = self.sxy(13);
        let (sx2, sy2) = self.sxy(14);
        let v = sx0 * sy1 + sx1 * sy2 + sx2 * sy0 - sx0 * sy2 - sx1 * sy0 - sx2 * sy1;
        self.set_mac0(v);
    }

    /// AVSZ3 (0x2D): the ZSF3-scaled average of the last 3 SZ values, into OTZ — the ordering-table
    /// depth for a triangle. AVSZ4 (0x2E) is the same over 4 SZ values scaled by ZSF4 (for a quad).
    fn avsz3(&mut self) {
        let zsf3 = self.ctrl[29] as i16 as i64;
        let v = zsf3 * (self.sz(17) + self.sz(18) + self.sz(19));
        self.set_mac0(v);
        self.set_otz(v >> 12);
    }
    fn avsz4(&mut self) {
        let zsf4 = self.ctrl[30] as i16 as i64;
        let v = zsf4 * (self.sz(16) + self.sz(17) + self.sz(18) + self.sz(19));
        self.set_mac0(v);
        self.set_otz(v >> 12);
    }

    /// MVMVA (0x12): [IR1,IR2,IR3] = clamp( (CV*0x1000 + MX·V) >> sf ), where MX is the matrix
    /// (rotation/light/colour), V the vector (V0/V1/V2/IR), and CV the translation (TR/BK/FC/none),
    /// each chosen by the command's mx/v/cv fields — the general matrix-multiply every transform uses.
    fn mvmva(&mut self, sf: u32, lm: bool, mx: usize, vx: usize, cv: usize) {
        let m = self.matrix(mx);
        let v = self.vector(vx);
        let t = self.translation(cv);
        for i in 0..3 {
            let acc = (t[i] << 12) + m[i][0] * v[0] + m[i][1] * v[1] + m[i][2] * v[2];
            let mac = self.set_mac(i + 1, acc, sf);
            self.set_ir(i + 1, mac, lm);
        }
    }

    /// OP (0x0C): the outer (cross) product of the rotation-matrix diagonal [R11,R22,R33] with the
    /// current IR vector — [MAC1,MAC2,MAC3] = D × IR, then IR = clamp. Used for normal generation.
    fn op(&mut self, sf: u32, lm: bool) {
        let m = self.matrix(0); // rotation matrix; D = its diagonal
        let (d1, d2, d3) = (m[0][0], m[1][1], m[2][2]);
        let ir1 = self.data[9] as i16 as i64;
        let ir2 = self.data[10] as i16 as i64;
        let ir3 = self.data[11] as i16 as i64;
        let mac1 = self.set_mac(1, d2 * ir3 - d3 * ir2, sf);
        let mac2 = self.set_mac(2, d3 * ir1 - d1 * ir3, sf);
        let mac3 = self.set_mac(3, d1 * ir2 - d2 * ir1, sf);
        self.set_ir(1, mac1, lm);
        self.set_ir(2, mac2, lm);
        self.set_ir(3, mac3, lm);
    }

    // ----- matrix / vector / translation selectors (for MVMVA) ---------------------------------
    /// One of the three 3x3 matrices, as i64 elements. mx: 0=rotation, 1=light, 2=colour. (mx=3
    /// selects a "garbage" matrix on hardware — a documented bug deferred to a later pass.)
    fn matrix(&self, mx: usize) -> [[i64; 3]; 3] {
        let base = match mx {
            0 => 0,  // rotation matrix at ctrl[0..5]
            1 => 8,  // light matrix at ctrl[8..13]
            _ => 16, // colour matrix at ctrl[16..21]
        };
        let lo = |w: u32| w as i16 as i64;
        let hi = |w: u32| (w >> 16) as i16 as i64;
        let c = &self.ctrl;
        [
            [lo(c[base]), hi(c[base]), lo(c[base + 1])],
            [hi(c[base + 1]), lo(c[base + 2]), hi(c[base + 2])],
            [lo(c[base + 3]), hi(c[base + 3]), lo(c[base + 4])],
        ]
    }
    /// The multiply vector. vx: 0=V0, 1=V1, 2=V2, 3=[IR1,IR2,IR3].
    fn vector(&self, vx: usize) -> [i64; 3] {
        let lo = |w: u32| w as i16 as i64;
        let hi = |w: u32| (w >> 16) as i16 as i64;
        let d = &self.data;
        match vx {
            0 => [lo(d[0]), hi(d[0]), d[1] as i16 as i64],
            1 => [lo(d[2]), hi(d[2]), d[3] as i16 as i64],
            2 => [lo(d[4]), hi(d[4]), d[5] as i16 as i64],
            _ => [d[9] as i16 as i64, d[10] as i16 as i64, d[11] as i16 as i64],
        }
    }
    /// The translation vector. cv: 0=TR, 1=BK, 2=FC, 3=none(0). (The FC case has a hardware bug in
    /// the real MVMVA that a later pass models; here it is the plain translation.)
    fn translation(&self, cv: usize) -> [i64; 3] {
        let c = &self.ctrl;
        match cv {
            0 => [c[5] as i32 as i64, c[6] as i32 as i64, c[7] as i32 as i64], // TR
            1 => [c[13] as i32 as i64, c[14] as i32 as i64, c[15] as i32 as i64], // BK
            2 => [c[21] as i32 as i64, c[22] as i32 as i64, c[23] as i32 as i64], // FC
            _ => [0, 0, 0], // none
        }
    }

    // ----- RTPS / RTPT: perspective transform + the screen-projection divide (M6.1b) -----------
    /// RTPS (0x01): transform V0 by the rotation matrix + translation, push its depth (SZ) and its
    /// projected screen point (SXY), and compute the depth-cue factor IR0. The single-vertex form.
    fn rtps(&mut self, sf: u32, lm: bool) {
        let div = self.rtp(0, sf, lm);
        self.depth_cue(div);
    }
    /// RTPT (0x30): RTPS for all three vertices V0,V1,V2 (three SZ + SXY pushes); IR1-3 end at V2 and
    /// the depth cue uses V2's divide. The triangle form games call once per polygon.
    fn rtpt(&mut self, sf: u32, lm: bool) {
        self.rtp(0, sf, lm);
        self.rtp(1, sf, lm);
        let div = self.rtp(2, sf, lm);
        self.depth_cue(div);
    }

    /// One vertex of the perspective transform, shared by RTPS/RTPT. Transforms V[vi] by the rotation
    /// matrix + TR, sets MAC/IR, pushes the SZ and SXY FIFOs, and returns the divide result `div`
    /// (≈ H/SZ via the Newton-Raphson reciprocal) for the caller's depth cue.
    fn rtp(&mut self, vi: usize, sf: u32, lm: bool) -> i64 {
        let m = self.matrix(0); // RTPS/RTPT always use the rotation matrix + TR translation
        let v = self.vector(vi);
        let tr = [
            self.ctrl[5] as i32 as i64,
            self.ctrl[6] as i32 as i64,
            self.ctrl[7] as i32 as i64,
        ];
        let mut acc = [0i64; 3];
        for i in 0..3 {
            acc[i] = (tr[i] << 12) + m[i][0] * v[0] + m[i][1] * v[1] + m[i][2] * v[2];
        }
        let mac1 = self.set_mac(1, acc[0], sf);
        let mac2 = self.set_mac(2, acc[1], sf);
        let mac3 = self.set_mac(3, acc[2], sf);
        self.set_ir(1, mac1, lm);
        self.set_ir(2, mac2, lm);
        self.set_ir3_rtp(mac3, sf, lm); // IR3 has a flag quirk during RTP — see the method
        // Push the Z FIFO: SZ3 = the (44-bit-wrapped) Z sum SAR 12, clamped to a u16 (the depth below).
        self.push_sz(wrap44(acc[2]) >> 12);

        // The perspective divide: div ≈ H/SZ3 via the UNR reciprocal, clamped to 0x1FFFF.
        let h = self.ctrl[26] & 0xFFFF; // H — used unsigned here (despite its sign-extended read-back)
        let sz3 = self.data[19] & 0xFFFF;
        let div = self.divide(h, sz3);

        // Project to screen: SX/SY = (offset + IR*div) >> 16, clamped to the [-0x400,0x3FF] screen range.
        let ofx = self.ctrl[24] as i32 as i64;
        let ofy = self.ctrl[25] as i32 as i64;
        let ir1 = self.data[9] as i16 as i64;
        let ir2 = self.data[10] as i16 as i64;
        let macx = ofx + ir1 * div;
        let macy = ofy + ir2 * div;
        self.set_mac0(macx);
        let sx = self.sat_xy(macx >> 16, 14);
        self.set_mac0(macy);
        let sy = self.sat_xy(macy >> 16, 13);
        self.push_sxy(sx, sy);
        div
    }

    /// IR3 for RTPS/RTPT, which has a documented quirk: the IR3 *value* clamps MAC3 with the lm bit
    /// like any IR, but FLAG bit 22 (IR3 saturated) is set from a *separate* check — the SZ3-related
    /// value `MAC3 SAR ((1-sf)*12)` against the lm=0 bounds [-0x8000, 0x7FFF]. So a huge MAC3 whose
    /// shifted-for-Z value is in range leaves bit 22 clear even though the IR3 value itself clamps.
    fn set_ir3_rtp(&mut self, mac3: i64, sf: u32, lm: bool) {
        let lo = if lm { 0 } else { -0x8000 };
        self.data[11] = mac3.clamp(lo, 0x7FFF) as i32 as u32; // the value: normal lm clamp
        let chk = mac3 >> ((1 - sf / 12) * 12); // SAR 12 when sf=0, SAR 0 when sf=12
        if chk < -0x8000 || chk > 0x7FFF {
            self.flag(22);
        }
    }

    /// The depth-cue tail (RTPS, and RTPT's last vertex): IR0 = a fog/colour interpolation factor from
    /// DQB + DQA*div, clamped to [0, 0x1000].
    fn depth_cue(&mut self, div: i64) {
        let dqa = self.ctrl[27] as i16 as i64;
        let dqb = self.ctrl[28] as i32 as i64;
        let mac0 = dqb + dqa * div;
        self.set_mac0(mac0);
        let factor = mac0 >> 12;
        let sat = factor.clamp(0, 0x1000);
        if sat != factor {
            self.flag(12);
        }
        self.data[8] = sat as u32; // IR0
    }

    /// Push the screen-Z FIFO (SZ0<-SZ1<-SZ2<-SZ3) and clamp the new SZ3 to a u16 (FLAG bit 18).
    fn push_sz(&mut self, val: i64) {
        self.data[16] = self.data[17];
        self.data[17] = self.data[18];
        self.data[18] = self.data[19];
        let sat = val.clamp(0, 0xFFFF);
        if sat != val {
            self.flag(18);
        }
        self.data[19] = sat as u32;
    }

    /// Push the screen-XY FIFO (SXY0<-SXY1<-SXY2<-packed sx,sy).
    fn push_sxy(&mut self, sx: i64, sy: i64) {
        self.data[12] = self.data[13];
        self.data[13] = self.data[14];
        self.data[14] = (sx as u32 & 0xFFFF) | ((sy as u32 & 0xFFFF) << 16);
    }

    /// Clamp a projected screen coordinate to [-0x400, 0x3FF], flagging `bit` (14=SX2, 13=SY2).
    fn sat_xy(&mut self, val: i64, bit: u32) -> i64 {
        let sat = val.clamp(-0x400, 0x3FF);
        if sat != val {
            self.flag(bit);
        }
        sat
    }

    /// The GTE perspective divide: an Unsigned Newton-Raphson reciprocal of `H*0x10000/SZ3`, result
    /// clamped to 0x1FFFF. If `H >= 2*SZ3` the quotient would overflow → clamp to 0x1FFFF and flag
    /// bit 17. (Two NR refinement steps off the `unr_seed` give the hardware's exact ~inaccurate result.)
    fn divide(&mut self, h: u32, sz3: u32) -> i64 {
        if h < sz3.wrapping_mul(2) {
            let z = (sz3 as u16).leading_zeros();
            let n = (h as u64) << z;
            let d0 = (sz3 as u64) << z; // normalised divisor, in 0x8000..0xFFFF
            let u = unr_seed((d0 - 0x7FC0) >> 7) + 0x101;
            let d1 = (0x0200_0080u64 - d0 * u) >> 8;
            let d2 = (0x0000_0080u64 + d1 * u) >> 8;
            (((n * d2) + 0x8000) >> 16).min(0x1_FFFF) as i64
        } else {
            self.flag(17);
            0x1_FFFF
        }
    }
}
