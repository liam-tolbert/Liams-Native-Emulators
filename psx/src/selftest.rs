//! Built-in, ROM-free correctness gate — extracted from `main.rs` to keep the host shell lean.
//!
//! `run_selftest` hand-assembles tiny MIPS programs (the encoders live at the bottom) and small GP0
//! command sequences, runs them on a fresh `Bus`, and checks architecturally-correct results: the CPU,
//! the GPU register + transfer model, DMA, and the rasterizer. It is this crate's stand-in for a
//! `cargo test` suite; `main` runs it for the
//! `selftest` mode. Shared host-side helpers (`VRAM_W`/`VRAM_H`, `vram_to_rgb`, the PNG codec in
//! `img`) are imported from the crate root.

use crate::bus::Bus;
use crate::cpu::Cpu;
use crate::{gpu, img, irq};
use crate::{vram_to_rgb, VRAM_H, VRAM_W};

// ===== built-in CPU self-test ===========================================================
//
// A ROM-free correctness gate. Each scenario hand-assembles a tiny MIPS program, loads it into
// RAM at address 0, single-steps it, and checks the architecturally-correct result — with the
// emphasis on the cases that are easy to get subtly wrong: the load-delay slot (a loaded value
// is invisible to the very next instruction), the branch-delay slot (the instruction after a
// branch always runs), signed overflow trapping, signed-vs-unsigned compares, JAL/JR linking,
// and the unaligned LWL/LWR pair.

/// Build a fresh machine with `program` (a list of instruction words) loaded at address 0 and
/// the PC pointed there. Address 0 is the bottom of main RAM; the programs keep their data in
/// the 0x100+ range so code and data never overlap.
fn build(program: &[u32]) -> Cpu {
    let mut bus = Bus::new();
    let mut bytes = Vec::with_capacity(program.len() * 4);
    for &w in program {
        bytes.extend_from_slice(&w.to_le_bytes());
    }
    bus.store_ram(0, &bytes);
    let mut cpu = Cpu::new(bus);
    cpu.pc = 0x0000_0000;
    cpu.next_pc = 0x0000_0004;
    cpu.current_pc = 0x0000_0000;
    cpu
}

/// Run `program` for exactly `n` steps.
fn run(cpu: &mut Cpu, n: usize) {
    for _ in 0..n {
        cpu.step();
    }
}

fn check(pass: &mut bool, name: &str, got: u32, want: u32) {
    let ok = got == want;
    println!(
        "  [{}] {:<30} got=0x{:08X} want=0x{:08X}",
        if ok { "PASS" } else { "FAIL" },
        name,
        got,
        want
    );
    *pass &= ok;
}

/// Read one VRAM pixel back through a `C0` download — the rasterizer self-tests' eyeball into what
/// actually landed in VRAM. Issues a fresh `C0` each call so the read cursor resets to the requested
/// pixel; the returned value includes the mask bit (15).
fn px(g: &mut gpu::Gpu, x: u32, y: u32) -> u32 {
    g.gp0(0xC000_0000);
    g.gp0((y << 16) | x);
    g.gp0((1 << 16) | 1);
    g.read() & 0xFFFF
}

/// Plant `pixels` (row-major 15-bit values) into VRAM at (x,y) via an `A0` CPU→VRAM upload — used by
/// the texture self-tests to lay down a texture or a CLUT before drawing a textured primitive.
fn upload(g: &mut gpu::Gpu, x: u32, y: u32, w: u32, h: u32, pixels: &[u16]) {
    g.gp0(0xA000_0000);
    g.gp0((y << 16) | x);
    g.gp0((h << 16) | w);
    let mut i = 0;
    while i < pixels.len() {
        let lo = pixels[i] as u32;
        let hi = *pixels.get(i + 1).unwrap_or(&0) as u32; // odd tail: high half ignored by the GPU
        g.gp0((hi << 16) | lo);
        i += 2;
    }
}

/// A 16-sector synthetic CD image: sector L's 2048-byte Mode2/Form1 user area (bytes 24..24+2048 of
/// the raw 2352-byte sector) is filled with the byte `0xA0 + L`, so a read can be checked by value.
fn synth_disc() -> crate::cdrom::CdImage {
    let mut bytes = vec![0u8; 16 * 2352];
    for l in 0..16usize {
        let base = l * 2352 + 24;
        for b in &mut bytes[base..base + 2048] {
            *b = 0xA0 + l as u8;
        }
    }
    crate::cdrom::CdImage::from_bin(bytes)
}

/// Drive a fresh `bus` to "streaming sector 0": load the synth disc, Setloc LBA 0, issue ReadN,
/// then ack the INT3 acknowledge — leaving the queue armed for the first INT1 with the index at 1.
/// The ReadN case below spells this whole dance out as the canonical walk-through; the DMA-ch3 and
/// Pause cases (whose point is *downstream* of the read starting) call this so their setup doesn't
/// drown out what they actually check.
fn cd_begin_read(bus: &mut Bus) {
    bus.cdrom.load_disc(synth_disc());
    bus.write32(0x1F80_1800, 0);
    for p in [0x00, 0x02, 0x00] {
        bus.write32(0x1F80_1802, p); // Setloc MM:SS:FF = 00:02:00 -> LBA 0
    }
    bus.write32(0x1F80_1801, 0x02); // Setloc
    bus.tick(60_000);
    bus.write32(0x1F80_1800, 1);
    bus.write32(0x1F80_1803, 0x07); // ack Setloc
    bus.write32(0x1F80_1800, 0);
    bus.write32(0x1F80_1801, 0x06); // ReadN
    bus.tick(60_000);
    bus.write32(0x1F80_1800, 1);
    bus.write32(0x1F80_1803, 0x07); // ack the ReadN INT3
}

pub(crate) fn run_selftest() -> bool {
    println!("[CPU self-test]\n");
    let mut pass = true;

    // --- load-delay slot ---------------------------------------------------------------
    // After `lw r3`, the FOLLOWING instruction must still see the OLD r3; only the one after
    // that sees the loaded value.
    {
        let prog = [
            ori(1, 0, 0x100),  // r1 = 0x100 (a scratch RAM address)
            ori(2, 0, 0xAAAA), // r2 = 0xAAAA (a marker)
            sw(2, 1, 0),       // mem[0x100] = 0xAAAA
            ori(2, 0, 0x1234), // r2 = 0x1234 (clobber r2 so the test below is meaningful)
            lw(3, 1, 0),       // r3 <- mem[0x100], but delayed
            ori(4, 3, 0),      // delay: reads OLD r3 (== 0)
            ori(5, 3, 0),      // settled: reads NEW r3 (== 0xAAAA)
        ];
        let mut cpu = build(&prog);
        run(&mut cpu, prog.len());
        check(&mut pass, "load-delay stale read (r4)", cpu.regs[4], 0x0000_0000);
        check(&mut pass, "load-delay settled read (r5)", cpu.regs[5], 0x0000_AAAA);
        check(&mut pass, "loaded value (r3)", cpu.regs[3], 0x0000_AAAA);
    }

    // --- branch-delay slot -------------------------------------------------------------
    // A taken branch still executes the instruction right after it; the one past that is skipped.
    {
        let prog = [
            ori(1, 0, 1),     // r1 = 1
            ori(2, 0, 0),     // r2 = 0
            beq(0, 0, 2),     // always taken; lands two instructions past the delay slot
            ori(2, 0, 0x111), // delay slot — MUST run
            ori(1, 0, 0x222), // skipped over by the branch
            ori(3, 0, 0x333), // branch target
        ];
        let mut cpu = build(&prog);
        run(&mut cpu, 6);
        check(&mut pass, "delay slot ran (r2)", cpu.regs[2], 0x0000_0111);
        check(&mut pass, "branch skipped instr (r1)", cpu.regs[1], 0x0000_0001);
        check(&mut pass, "branch target ran (r3)", cpu.regs[3], 0x0000_0333);
    }

    // --- JAL / JR linking --------------------------------------------------------------
    // JAL leaves the return address (the instruction after its delay slot) in ra; JR ra returns.
    {
        let prog = [
            ori(2, 0, 0),     // 0x00  r2 = 0
            jal(0x18),        // 0x04  call 0x18; ra = 0x0C
            ori(3, 0, 0x55),  // 0x08  delay slot — runs
            ori(2, 0, 0x999), // 0x0C  return lands here
            NOP,              // 0x10
            NOP,              // 0x14
            ori(4, 0, 0x77),  // 0x18  function body
            jr(31),           // 0x1C  return to ra (0x0C)
            NOP,              // 0x20  jr delay slot
        ];
        let mut cpu = build(&prog);
        run(&mut cpu, 9);
        check(&mut pass, "return address (ra)", cpu.regs[31], 0x0000_000C);
        check(&mut pass, "jal delay slot (r3)", cpu.regs[3], 0x0000_0055);
        check(&mut pass, "function ran (r4)", cpu.regs[4], 0x0000_0077);
        check(&mut pass, "ran after return (r2)", cpu.regs[2], 0x0000_0999);
    }

    // --- SLT vs SLTU -------------------------------------------------------------------
    // 0x80000000 is negative as signed (so < 1) but huge as unsigned (so NOT < 1).
    {
        let prog = [
            ori(1, 0, 1),    // r1 = 1
            lui(2, 0x8000),  // r2 = 0x80000000
            slt(3, 2, 1),    // signed: 0x80000000 < 1  -> 1
            sltu(4, 2, 1),   // unsigned: 0x80000000 < 1 -> 0
        ];
        let mut cpu = build(&prog);
        run(&mut cpu, prog.len());
        check(&mut pass, "SLT signed (r3)", cpu.regs[3], 1);
        check(&mut pass, "SLTU unsigned (r4)", cpu.regs[4], 0);
    }

    // --- overflow trap -----------------------------------------------------------------
    // INT_MAX + 1 overflows a signed 32-bit add, so ADD must trap: the destination is left
    // unwritten, EPC points at the ADD, CAUSE.ExcCode says "overflow", and PC jumps to the
    // general-exception vector (0x80000080, because BEV is clear at construction).
    {
        let prog = [
            lui(1, 0x7FFF),    // 0x00
            ori(1, 1, 0xFFFF), // 0x04  r1 = 0x7FFFFFFF
            ori(2, 0, 1),      // 0x08  r2 = 1
            add(3, 1, 2),      // 0x0C  overflow -> trap
        ];
        let mut cpu = build(&prog);
        run(&mut cpu, prog.len());
        let exc_code = (cpu.cop0.cause >> 2) & 0x1F;
        check(&mut pass, "overflow dest untouched (r3)", cpu.regs[3], 0);
        check(&mut pass, "overflow ExcCode == 0x0C", exc_code, 0x0C);
        check(&mut pass, "EPC == faulting ADD", cpu.cop0.epc, 0x0000_000C);
        check(&mut pass, "vectored to handler", cpu.pc, 0x8000_0080);
    }

    // --- unaligned load via LWL/LWR pair ----------------------------------------------
    // Two aligned words in memory; read the unaligned word straddling them. The pair must
    // compose with no load-delay between LWR and LWL.
    {
        let prog = [
            ori(1, 0, 0x100),  // r1 = 0x100
            lui(2, 0x4433),
            ori(2, 2, 0x2211), // r2 = 0x44332211
            sw(2, 1, 0),       // mem[0x100] = 0x44332211 (bytes 11 22 33 44)
            lui(3, 0x8877),
            ori(3, 3, 0x6655), // r3 = 0x88776655
            sw(3, 1, 4),       // mem[0x104] = 0x88776655 (bytes 55 66 77 88)
            lwr(4, 1, 2),      // read low half of the word at 0x102
            lwl(4, 1, 5),      // read high half (merges with the LWR result)
            NOP,               // let the LWL load-delay settle
            ori(5, 4, 0),      // r5 = the assembled unaligned word
        ];
        let mut cpu = build(&prog);
        run(&mut cpu, prog.len());
        // The unaligned word at 0x102 is bytes 33 44 55 66 -> little-endian 0x66554433.
        check(&mut pass, "LWL/LWR assembled (r4)", cpu.regs[4], 0x6655_4433);
        check(&mut pass, "settled into r5", cpu.regs[5], 0x6655_4433);
    }

    // ===== memory map, MMIO, exceptions & interrupt delivery =======================
    // Most of this machinery was pulled forward so the CPU could boot the BIOS; here is where it
    // gets *exercised and gated*. The headline gap left earlier is interrupt delivery — `Irq::raise`
    // had no caller, so the whole source -> I_STAT -> mask -> Cause.IP2 -> service -> RFE chain
    // had never run. These scenarios drive it (device-free, via software interrupts and a
    // synthetic `raise`), alongside the memory map / MMIO routing and the address-error path.

    // --- COP0 register move round-trip (MTC0 / MFC0) -----------------------------------
    // MTC0 writes a COP0 register immediately; MFC0 reads one back but, like a load, only after
    // a one-instruction delay — so the value lands a step later (hence the trailing NOP).
    {
        let prog = [
            ori(1, 0, 0x1234), // r1 = 0x1234
            mtc0(1, 14),       // EPC <- r1   (EPC is COP0 reg 14)
            mfc0(2, 14),       // r2 <- EPC   (delayed)
            NOP,               // let the MFC0 delay settle
        ];
        let mut cpu = build(&prog);
        run(&mut cpu, prog.len());
        check(&mut pass, "MTC0 wrote EPC", cpu.cop0.epc, 0x0000_1234);
        check(&mut pass, "MFC0 read EPC back (r2)", cpu.regs[2], 0x0000_1234);
    }

    // --- MMIO round-trip (I/O registers via the KSEG1 uncached window 0xBF80_xxxx) ------
    // A store/load to the hardware-register block must route through the bus to the device.
    // 0xBF801074 is I_MASK; a memory-control register round-trips a word; a still-deferred SPU
    // register reads back 0. (KSEG1 masks down to physical 0x1F80_1xxx — the uncached I/O view.)
    // (The timers used to be the "reads 0" stub here, but they now count on every bus tick, so this
    // probe targets the SPU block instead — see the dedicated timer section near the end.)
    {
        let prog = [
            lui(1, 0xBF80),    // r1 = 0xBF80_0000
            ori(1, 1, 0x1074), // r1 = 0xBF80_1074  (I_MASK)
            ori(2, 0, 0x000F), // r2 = 0x0F
            sw(2, 1, 0),       // I_MASK = 0x0F
            lw(3, 1, 0),       // r3 <- I_MASK      (delayed; commits at the next instruction)
            lui(5, 0xBF80),    // (also commits the r3 load)
            ori(5, 5, 0x1000), // r5 = 0xBF80_1000  (memory-control reg 0)
            ori(6, 0, 0xCAFE), // r6 = 0xCAFE
            sw(6, 5, 0),       // mem_control[0] = 0xCAFE
            lw(7, 5, 0),       // r7 <- mem_control[0] (delayed)
            lui(8, 0xBF80),    // (commits the r7 load)
            ori(8, 8, 0x1C00), // r8 = 0xBF80_1C00  (an SPU register — deferred, reads 0)
            lw(9, 8, 0),       // r9 <- SPU reg     (delayed)
            NOP,               // settle the last load
        ];
        let mut cpu = build(&prog);
        run(&mut cpu, prog.len());
        check(&mut pass, "I_MASK write/read (r3)", cpu.regs[3], 0x0000_000F);
        check(&mut pass, "I_MASK landed in device", cpu.bus.irq.read_mask() as u32, 0x0F);
        check(&mut pass, "mem-control round-trip (r7)", cpu.regs[7], 0x0000_CAFE);
        check(&mut pass, "deferred SPU register reads 0 (r9)", cpu.regs[9], 0);
    }

    // --- I_STAT acknowledge semantics (write-to-ACK, not write-1-to-clear) -------------
    // Devices set pending bits in I_STAT; writing I_STAT keeps only the bits that are 1 in the
    // written value (`stat &= val`). Writing 0x0001 over {VBLANK,GPU} clears GPU but keeps
    // VBLANK — the opposite of the more common "write 1 to clear", and easy to get backwards.
    {
        let prog = [
            lui(1, 0xBF80),
            ori(1, 1, 0x1070), // r1 = I_STAT address
            ori(2, 0, 0x0001), // r2 = ack mask: keep bit 0 only
            sw(2, 1, 0),       // ack: stat &= 0x0001
        ];
        let mut cpu = build(&prog);
        cpu.bus.irq.raise(irq::source::VBLANK); // bit 0
        cpu.bus.irq.raise(irq::source::GPU); // bit 1 -> stat = 0b11
        run(&mut cpu, prog.len());
        check(
            &mut pass,
            "I_STAT ack kept VBLANK, cleared GPU",
            cpu.bus.irq.read_stat() as u32,
            0x0001,
        );
    }

    // --- address-error exception on a misaligned load ----------------------------------
    // LW needs a 4-byte-aligned address; 0x102 isn't, so the load faults: ExcCode = 4
    // (AddrErrLoad), BadVaddr = the bad address, EPC = the LW, PC -> the general vector.
    {
        let prog = [
            ori(1, 0, 0x102), // 0x00  r1 = 0x102 (misaligned for a word)
            lw(2, 1, 0),      // 0x04  faults
        ];
        let mut cpu = build(&prog);
        run(&mut cpu, prog.len());
        check(&mut pass, "load AddrErr ExcCode == 0x04", (cpu.cop0.cause >> 2) & 0x1F, 0x04);
        check(&mut pass, "load AddrErr BadVaddr", cpu.cop0.bad_vaddr, 0x0000_0102);
        check(&mut pass, "load AddrErr EPC == LW", cpu.cop0.epc, 0x0000_0004);
        check(&mut pass, "load AddrErr vectored", cpu.pc, 0x8000_0080);
    }

    // --- address-error exception on a misaligned store ---------------------------------
    {
        let prog = [
            ori(1, 0, 0x101),  // 0x00  r1 = 0x101 (misaligned)
            ori(2, 0, 0xBEEF), // 0x04
            sw(2, 1, 0),       // 0x08  faults: ExcCode 5 (AddrErrStore)
        ];
        let mut cpu = build(&prog);
        run(&mut cpu, prog.len());
        check(&mut pass, "store AddrErr ExcCode == 0x05", (cpu.cop0.cause >> 2) & 0x1F, 0x05);
        check(&mut pass, "store AddrErr BadVaddr", cpu.cop0.bad_vaddr, 0x0000_0101);
        check(&mut pass, "store AddrErr EPC == SW", cpu.cop0.epc, 0x0000_0008);
        check(&mut pass, "store AddrErr vectored", cpu.pc, 0x8000_0080);
    }

    // --- software interrupt delivery (COP0 only, no device) ----------------------------
    // Enable interrupts (SR.IEc, bit 0) and unmask software interrupt 0 (SR.IM bit 8), then
    // raise it by writing CAUSE's software-interrupt bit (bit 8 — one of the only CAUSE bits
    // software can write). The interrupt is recognised *between* instructions: the next one does
    // NOT run, EPC points at it, the SR stack is pushed, and we vector to the handler.
    {
        let prog = [
            ori(1, 0, 0x0101), // 0x00  r1 = IEc(bit0) | IM0(bit8)
            mtc0(1, 12),       // 0x04  SR <- r1
            ori(2, 0, 0x0100), // 0x08  r2 = CAUSE software-int 0 (bit 8)
            mtc0(2, 13),       // 0x0C  CAUSE <- r2   (raises the soft interrupt)
            ori(7, 0, 0xDEAD), // 0x10  MUST NOT run — the interrupt is taken first
        ];
        let mut cpu = build(&prog);
        run(&mut cpu, prog.len());
        check(&mut pass, "soft-int ExcCode == 0x00", (cpu.cop0.cause >> 2) & 0x1F, 0x00);
        check(&mut pass, "soft-int EPC == pending instr", cpu.cop0.epc, 0x0000_0010);
        check(&mut pass, "soft-int vectored", cpu.pc, 0x8000_0080);
        check(&mut pass, "soft-int pre-empted instr (r7)", cpu.regs[7], 0);
        // SR low 6 bits before: 0b000001 (IEc set). After the push: 0b000100 (IEc cleared so the
        // handler runs uninterrupted; the old IEc is preserved in the "previous" pair).
        check(&mut pass, "soft-int pushed SR stack", cpu.cop0.sr & 0x3F, 0x04);
    }

    // --- hardware interrupt delivery through the controller (the full IP2 loop) ----------
    // A "device" raises a source in I_STAT; I_MASK lets it through; the controller pulls the
    // CPU's single external line (COP0 Cause.IP2); with IEc + IM bit 10 set, the CPU takes it.
    // Then the handler acknowledges I_STAT and the aggregated line drops. This is the end-to-end
    // path `Irq::raise` exists for (the real sources arrive with the timers/GPU).
    {
        let prog = [
            lui(1, 0xBF80),
            ori(1, 1, 0x1074), // 0x04  r1 = I_MASK address
            ori(2, 0, 0x0010), // 0x08  r2 = unmask TIMER0 (bit 4)
            sw(2, 1, 0),       // 0x0C  I_MASK = 0x10
            ori(3, 0, 0x0401), // 0x10  r3 = IEc(bit0) | IM2(bit10)
            mtc0(3, 12),       // 0x14  SR <- r3
            ori(7, 0, 0xBEEF), // 0x18  MUST NOT run once the IRQ is taken
        ];
        let mut cpu = build(&prog);
        run(&mut cpu, 6); // the 6 setup instructions (through MTC0 SR); no IRQ pending yet
        cpu.bus.irq.raise(irq::source::TIMER0); // a device pulls its line
        run(&mut cpu, 1); // next step: line high + enabled -> Interrupt taken before 0x18 runs
        check(&mut pass, "hw-int IP2 set", (cpu.cop0.cause >> 10) & 1, 1);
        check(&mut pass, "hw-int ExcCode == 0x00", (cpu.cop0.cause >> 2) & 0x1F, 0x00);
        check(&mut pass, "hw-int EPC == pending instr", cpu.cop0.epc, 0x0000_0018);
        check(&mut pass, "hw-int pre-empted instr (r7)", cpu.regs[7], 0);

        // The handler acknowledges the source; the controller's aggregated line then drops, and
        // the CPU's next step mirrors that down into Cause.IP2.
        cpu.bus.irq.ack(0); // clear all pending bits
        check(&mut pass, "ack cleared pending", cpu.bus.irq.pending() as u32, 0);
        cpu.step(); // one handler instruction (the vector page is zeroed RAM = NOP); refreshes IP2
        check(&mut pass, "hw-int IP2 dropped after ack", (cpu.cop0.cause >> 10) & 1, 0);
    }

    // --- RFE pops the SR mode/interrupt stack ------------------------------------------
    // SR's low 6 bits are a 3-deep stack of (kernel-mode, interrupt-enable) pairs. RFE pops it:
    // current <- previous, previous <- old, and the "old" pair is left in place. Seed 0b01_01_00
    // and pop to 0b01_01_01.
    {
        let prog = [
            ori(1, 0, 0x0014), // r1 = 0b010100 (current=00, previous=01, old=01)
            mtc0(1, 12),       // SR <- r1
            rfe(),             // pop the stack
        ];
        let mut cpu = build(&prog);
        run(&mut cpu, prog.len());
        check(&mut pass, "RFE popped SR stack", cpu.cop0.sr & 0x3F, 0x15);
    }

    // --- cache isolation drops stores (SR.IsC, bit 16) ---------------------------------
    // While the isolate-cache bit is set, stores hit the (unemulated) data cache, not RAM — the
    // BIOS relies on this to scrub the cache during boot. A store made while isolated must NOT
    // reach RAM; one made after clearing IsC must. (0x200 is written while isolated, 0x204 after.)
    {
        let prog = [
            lui(1, 0x0001),    // 0x00  r1 = 0x0001_0000 (SR.IsC)
            mtc0(1, 12),       // 0x04  SR <- r1            (cache isolated)
            ori(2, 0, 0x0200), // 0x08  r2 = 0x200
            ori(3, 0, 0xAAAA), // 0x0C  r3 = 0xAAAA
            sw(3, 2, 0),       // 0x10  DROPPED (isolated)
            mtc0(0, 12),       // 0x14  SR <- 0             (no longer isolated)
            ori(4, 0, 0x0204), // 0x18  r4 = 0x204
            ori(5, 0, 0xBBBB), // 0x1C  r5 = 0xBBBB
            sw(5, 4, 0),       // 0x20  writes RAM (not isolated)
            lw(6, 2, 0),       // 0x24  r6 <- mem[0x200] (delayed) -> 0 (the store was dropped)
            lw(7, 4, 0),       // 0x28  r7 <- mem[0x204] (delayed) -> 0xBBBB
            NOP,               // 0x2C  settle the second load
        ];
        let mut cpu = build(&prog);
        run(&mut cpu, prog.len());
        check(&mut pass, "isolated store dropped (r6)", cpu.regs[6], 0);
        check(&mut pass, "normal store wrote (r7)", cpu.regs[7], 0x0000_BBBB);
    }

    // ===== GPU register model + the PNG verify harness ============================
    // The GPU draws nothing yet, so these don't use the `gpu/` reference images — they pin the
    // register model (GP0 state commands + GP1 knobs surfaced through GPUSTAT, and the GP0 FIFO
    // consuming the right word counts) and calibrate the VRAM -> RGB -> PNG -> RGB round-trip the
    // rasterizer stages will be graded against.

    // --- GPUSTAT bits assembled from GP0/GP1 state -------------------------------------
    {
        let mut bus = Bus::new();
        let g = &mut bus.gpu;
        g.gp1(0x0000_0000); // GP1(00) reset -> known baseline

        // GP0(E1) draw-mode low 11 bits map straight to GPUSTAT bits 0-10.
        g.gp0(0xE100_0123);
        check(&mut pass, "GPUSTAT draw-mode bits 0-10", g.status() & 0x7FF, 0x123);

        // GP0(E6) mask settings -> GPUSTAT bits 11 (set-mask) and 12 (check-mask).
        g.gp0(0xE600_0003);
        check(&mut pass, "GPUSTAT mask bits 11/12", (g.status() >> 11) & 3, 3);

        // GP1(03) display enable/disable -> GPUSTAT bit 23 (1 = disabled).
        g.gp1(0x0300_0001);
        check(&mut pass, "GPUSTAT display-disabled (bit 23)", (g.status() >> 23) & 1, 1);
        g.gp1(0x0300_0000);
        check(&mut pass, "GPUSTAT display-enabled (bit 23)", (g.status() >> 23) & 1, 0);

        // GP1(04) DMA direction -> bits 29-30, and the bit-25 data-request mirror (direction 2
        // mirrors the DMA-block-ready bit 28, which we force high).
        g.gp1(0x0400_0002);
        check(&mut pass, "GPUSTAT DMA direction (bits 29-30)", (g.status() >> 29) & 3, 2);
        check(&mut pass, "GPUSTAT DMA request (bit 25)", (g.status() >> 25) & 1, 1);

        // GP0(1F) raises the GPU IRQ (bit 24); GP1(02) acknowledges it.
        g.gp0(0x1F00_0000);
        check(&mut pass, "GPUSTAT GPU-IRQ set (bit 24)", (g.status() >> 24) & 1, 1);
        g.gp1(0x0200_0000);
        check(&mut pass, "GPUSTAT GPU-IRQ cleared (bit 24)", (g.status() >> 24) & 1, 0);

        // The "ready" bits (26/27/28) stay high so a polling BIOS proceeds.
        check(&mut pass, "GPUSTAT ready bits 26/27/28", (g.status() >> 26) & 7, 7);
    }

    // --- GP0 FIFO consumes exactly the right number of words ---------------------------
    // If a multi-word draw command miscounts, the *next* command desyncs. Send a 4-word flat
    // triangle, then an E4 draw-area setting, and read that setting back via GP1(10): it only
    // round-trips correctly if the triangle swallowed exactly its 4 words.
    {
        let mut bus = Bus::new();
        let g = &mut bus.gpu;
        g.gp1(0x0000_0000);
        g.gp0(0x2000_00FF); // flat triangle (cmd 0x20): colour word + 3 vertices = 4 words
        g.gp0(0x0000_0000); // vertex 0
        g.gp0(0x0010_0000); // vertex 1
        g.gp0(0x0000_0010); // vertex 2 (command complete here)
        g.gp0(0xE400_0000 | (200 << 10) | 300); // draw area bottom-right = (300, 200)
        g.gp1(0x1000_0004); // GPU-info request 4 -> draw-area BR into GPUREAD
        check(&mut pass, "GP0 FIFO in sync after a triangle", g.read(), 300 | (200 << 10));
    }

    // --- CPU->VRAM (A0) image upload drains the right pixel-word count ------------------
    // A 2x2 upload is 4 pixels = 2 data words. After draining them an E3 setting must land cleanly.
    {
        let mut bus = Bus::new();
        let g = &mut bus.gpu;
        g.gp1(0x0000_0000);
        g.gp0(0xA000_0000); // CPU->VRAM
        g.gp0(0x0000_0000); // destination (0, 0)
        g.gp0(0x0002_0002); // size 2 x 2 -> 4 pixels -> 2 data words
        g.gp0(0xDEAD_BEEF); // pixel data word 1
        g.gp0(0xCAFE_BABE); // pixel data word 2
        g.gp0(0xE300_0000 | (20 << 10) | 10); // draw area top-left = (10, 20)
        g.gp1(0x1000_0003); // GPU-info request 3 -> draw-area TL
        check(&mut pass, "GP0 image upload drained its data", g.read(), 10 | (20 << 10));
    }

    // --- harness calibration: VRAM -> RGB -> PNG -> RGB round-trips exactly -------------
    {
        let mut bus = Bus::new();
        {
            let v = bus.gpu.vram_mut();
            v[0] = 0x001F; // pure red   (R = 31, in the low 5 bits)
            v[1] = 0x03E0; // pure green (G = 31)
            v[VRAM_W] = 0x7C00; // pure blue (B = 31), first pixel of row 1
            v[VRAM_W + 1] = 0x7FFF; // white
        }
        let rgb = vram_to_rgb(bus.gpu.vram());
        // 5-bit 31 expands to 8-bit 0xF8 (top-aligned — see `expand5`; the suite never reaches 0xFF);
        // the empty channels stay 0.
        let px0 = (rgb[0] as u32) << 16 | (rgb[1] as u32) << 8 | rgb[2] as u32;
        let px1 = (rgb[3] as u32) << 16 | (rgb[4] as u32) << 8 | rgb[5] as u32;
        check(&mut pass, "VRAM red -> 0xF80000", px0, 0xF8_0000);
        check(&mut pass, "VRAM green -> 0x00F800", px1, 0x00_F800);

        // Encode then decode must return identical pixels and dimensions.
        let png = img::encode_rgb(VRAM_W as u32, VRAM_H as u32, &rgb);
        match img::decode_rgb(&png) {
            Some((w, h, rgb2)) => {
                check(&mut pass, "PNG round-trip width", w, VRAM_W as u32);
                check(&mut pass, "PNG round-trip height", h, VRAM_H as u32);
                check(&mut pass, "PNG round-trip pixels match", (rgb2 == rgb) as u32, 1);
            }
            None => check(&mut pass, "PNG round-trip decodes", 0, 1),
        }
    }

    // ===== VRAM transfers (A0 / C0 / 02 fill) =====================================
    // --- A0 CPU->VRAM upload, then C0 VRAM->CPU download, round-trips the same pixels ---
    {
        let mut bus = Bus::new();
        let g = &mut bus.gpu;
        g.gp1(0x0000_0000);
        g.gp0(0xA000_0000); // CPU->VRAM
        g.gp0((3 << 16) | 5); // destination (x=5, y=3)
        g.gp0((1 << 16) | 2); // size 2 wide x 1 tall (2 pixels = 1 data word)
        g.gp0(0x5678_1234); // packed pixels: low half 0x1234, high half 0x5678
        g.gp0(0xC000_0000); // VRAM->CPU
        g.gp0((3 << 16) | 5); // source (5, 3)
        g.gp0((1 << 16) | 2); // size 2 x 1
        check(&mut pass, "A0 upload -> C0 download round-trip", g.read(), 0x5678_1234);
    }

    // --- 02 fill, then read a filled pixel back via C0 --------------------------------
    {
        let mut bus = Bus::new();
        let g = &mut bus.gpu;
        g.gp1(0x0000_0000);
        g.gp0(0x0200_00FF); // fill, command colour 0x0000FF (R=0xFF) -> 15-bit 0x001F
        g.gp0(0x0000_0000); // top-left (0, 0)
        g.gp0((16 << 16) | 16); // size 16 x 16
        g.gp0(0xC000_0000); // read pixel (0,0) back
        g.gp0(0x0000_0000); // source (0, 0)
        g.gp0((1 << 16) | 1); // size 1 x 1
        check(&mut pass, "02 fill + read-back", g.read() & 0xFFFF, 0x001F);
    }

    // --- 80 VRAM->VRAM copy: upload, copy the block elsewhere, read the copy back -------
    {
        let mut bus = Bus::new();
        let g = &mut bus.gpu;
        g.gp1(0x0000_0000);
        g.gp0(0xA000_0000); // upload 2 pixels (0xABCD, 0x1357) ...
        g.gp0(0x0000_0000); // ... to (0, 0)
        g.gp0((1 << 16) | 2); // size 2 x 1
        g.gp0(0x1357_ABCD);
        g.gp0(0x8000_0000); // VRAM->VRAM copy ...
        g.gp0(0x0000_0000); // source (0, 0)
        g.gp0((5 << 16) | 10); // destination (10, 5)
        g.gp0((1 << 16) | 2); // size 2 x 1
        g.gp0(0xC000_0000); // read the copy back ...
        g.gp0((5 << 16) | 10); // ... from (10, 5)
        g.gp0((1 << 16) | 2); // size 2 x 1
        check(&mut pass, "80 VRAM->VRAM copy round-trip", g.read(), 0x1357_ABCD);
    }

    // ===== DMA (channels 2 GPU + 6 OTC, and the DMA interrupt) ====================
    // These drive the DMA through the real bus path: program MADR/BCR (and DPCR/DICR) via MMIO
    // writes, then write CHCR last — the start bit in that write is what kicks the transfer. DMA
    // runs synchronously, so the result is observable immediately afterward. (DMA addresses are
    // physical RAM; we use plain KUSEG addresses like 0x1000, which decode straight to RAM.)

    // --- channel 6 (OTC): clears a backward-linked ordering table in RAM --------------
    {
        let mut bus = Bus::new();
        // A channel won't start unless DPCR enables it: bit (ch*4+3), so ch6 -> bit 27.
        bus.write32(0x1F80_10F0, 0x0765_4321 | (1 << 27));
        bus.write32(0x1F80_10E0, 0x0000_1000); // ch6 MADR = table head (highest address)
        bus.write32(0x1F80_10E4, 4); // ch6 BCR = 4 entries
        bus.write32(0x1F80_10E8, 0x1100_0002); // ch6 CHCR = start(24) | manual-trigger(28) | step-down(1)
        // The head points one entry down; each entry links downward; the lowest entry terminates.
        check(&mut pass, "OTC head -> head-4", bus.read32(0x1000), 0x0000_0FFC);
        check(&mut pass, "OTC chain link", bus.read32(0x0FFC), 0x0000_0FF8);
        check(&mut pass, "OTC end marker (0xFFFFFF)", bus.read32(0x0FF4), 0x00FF_FFFF);
        // The start bits must self-clear once the transfer completes.
        let chcr = bus.read32(0x1F80_10E8);
        check(&mut pass, "OTC CHCR start bits cleared", chcr & ((1 << 24) | (1 << 28)), 0);
    }

    // --- channel 2 (GPU) linked-list: walk a packet chain, feed each word to GP0 -------
    // This is the keystone path: build an A0 (CPU->VRAM) upload as a one-node display list in RAM,
    // DMA it through GP0, then read the pixel back with a direct C0 download.
    {
        let mut bus = Bus::new();
        bus.write32(0x1F80_10F0, 0x0765_4321 | (1 << 11)); // enable ch2 (bit 2*4+3 = 11)
        bus.gpu.gp1(0x0000_0000); // reset GPU
        bus.gpu.gp1(0x0400_0002); // GP1(04) DMA direction = CPU->GP0 (documents the real handshake)
        bus.write32(0x1000, (4 << 24) | 0x00FF_FFFF); // node header: 4 payload words, then end-of-list
        bus.write32(0x1004, 0xA000_0000); // GP0 A0: CPU->VRAM upload
        bus.write32(0x1008, 0x0000_0000); // destination (0, 0)
        bus.write32(0x100C, (1 << 16) | 1); // size 1 x 1 -> one pixel -> one data word
        bus.write32(0x1010, 0x0000_ABCD); // the pixel (low half used)
        bus.write32(0x1F80_10A0, 0x0000_1000); // ch2 MADR = list head
        bus.write32(0x1F80_10A8, 0x0100_0401); // ch2 CHCR = start(24) | sync 2 linked-list (bit10) | RAM->dev (bit0)
        // Read pixel (0,0) back through a direct C0 download.
        bus.gpu.gp0(0xC000_0000);
        bus.gpu.gp0(0x0000_0000); // source (0, 0)
        bus.gpu.gp0((1 << 16) | 1); // size 1 x 1
        check(&mut pass, "DMA linked-list feeds GP0 (A0)", bus.gpu.read() & 0xFFFF, 0xABCD);
    }

    // --- channel 2 (GPU) block mode: stream GPUREAD into RAM ----------------------------
    // Fill a VRAM block, latch a C0 download, then DMA the GPUREAD stream into RAM (device->RAM).
    {
        let mut bus = Bus::new();
        bus.write32(0x1F80_10F0, 0x0765_4321 | (1 << 11)); // enable ch2
        bus.gpu.gp1(0x0000_0000);
        bus.gpu.gp0(0x0200_00FF); // 02 fill, command colour 0x0000FF (R=0xFF) -> 15-bit pixel 0x001F
        bus.gpu.gp0(0x0000_0000); // top-left (0, 0)
        bus.gpu.gp0((16 << 16) | 16); // 16 x 16 (fill snaps to 16-pixel units)
        bus.gpu.gp0(0xC000_0000); // latch a VRAM->CPU download ...
        bus.gpu.gp0(0x0000_0000); // ... source (0, 0)
        bus.gpu.gp0((1 << 16) | 2); // size 2 x 1 -> two pixels -> one word
        bus.write32(0x1F80_10A0, 0x0000_2000); // ch2 MADR = 0x2000
        bus.write32(0x1F80_10A4, (1 << 16) | 1); // ch2 BCR = 1 block x 1 word
        bus.write32(0x1F80_10A8, 0x0100_0200); // ch2 CHCR = start(24) | sync 1 request (bit9) | device->RAM (bit0=0)
        let p: u32 = 0x001F; // the filled pixel; two of them pack into one word
        check(&mut pass, "DMA block GPUREAD->RAM", bus.read32(0x2000), p | (p << 16));
    }

    // --- the DMA interrupt: completion -> DICR flag -> I_STAT bit 3 ---------------------
    {
        let mut bus = Bus::new();
        bus.write32(0x1F80_1074, 0x0000_FFFF); // I_MASK: allow all sources (incl. DMA, bit 3)
        bus.write32(0x1F80_10F0, 0x0765_4321 | (1 << 27)); // enable DMA channel 6
        bus.write32(0x1F80_10F4, (1 << 23) | (1 << 22)); // DICR: master enable (23) + ch6 IRQ enable (16+6)
        bus.write32(0x1F80_10E0, 0x0000_1000); // OTC MADR
        bus.write32(0x1F80_10E4, 2); // 2 entries
        bus.write32(0x1F80_10E8, 0x1100_0002); // CHCR start -> transfer completes -> raise the IRQ
        check(&mut pass, "DMA completion -> I_STAT bit 3", (bus.read32(0x1F80_1070) >> 3) & 1, 1);
        check(&mut pass, "DICR ch6 flag set (bit 30)", (bus.read32(0x1F80_10F4) >> 30) & 1, 1);
        check(&mut pass, "DICR master flag set (bit 31)", (bus.read32(0x1F80_10F4) >> 31) & 1, 1);
        // Acknowledge: I_STAT is write-0-to-clear; DICR flags are write-1-to-clear (opposite!).
        bus.write32(0x1F80_1070, !(1 << 3)); // clear I_STAT bit 3
        check(&mut pass, "I_STAT bit 3 acknowledged", (bus.read32(0x1F80_1070) >> 3) & 1, 0);
        bus.write32(0x1F80_10F4, (1 << 23) | (1 << 22) | (1 << 30)); // write 1 to clear ch6 flag; keep enables
        check(&mut pass, "DICR ch6 flag cleared", (bus.read32(0x1F80_10F4) >> 30) & 1, 0);
        check(&mut pass, "DICR master flag cleared", (bus.read32(0x1F80_10F4) >> 31) & 1, 0);
    }

    // ===== untextured rasterizer (polygons, rectangles, lines) =================
    // Each test draws a primitive, then reads pixels back through a C0 download (`px`) to confirm what
    // the rasterizer wrote. Command colour 0x0000FF (red) truncates to the 15-bit VRAM pixel 0x001F.
    // GP0(E4) sets the drawing-area bottom-right; `FULL_AREA` opens it to the whole framebuffer so the
    // scissor doesn't reject our test pixels (after a GP1 reset the area is just the single pixel 0,0).
    const FULL_AREA: u32 = 0xE400_0000 | (511 << 10) | 1023;

    // --- flat triangle: interior filled, exterior untouched ---------------------------
    {
        let mut bus = Bus::new();
        let g = &mut bus.gpu;
        g.gp1(0x0000_0000);
        g.gp0(FULL_AREA);
        g.gp0(0x2000_00FF); // flat triangle, colour 0x0000FF
        g.gp0(0x0000_0000); // v0 (0, 0)
        g.gp0(0x0000_000A); // v1 (10, 0)
        g.gp0(0x000A_0000); // v2 (0, 10)
        check(&mut pass, "flat triangle interior", px(g, 2, 2), 0x001F);
        check(&mut pass, "flat triangle exterior", px(g, 8, 8), 0x0000);
    }

    // --- scissor: only pixels inside the GP0(E3/E4) box are drawn ----------------------
    {
        let mut bus = Bus::new();
        let g = &mut bus.gpu;
        g.gp1(0x0000_0000);
        g.gp0(0xE300_0000 | (2 << 10) | 2); // draw area top-left = (2, 2)
        g.gp0(0xE400_0000 | (5 << 10) | 5); // draw area bottom-right = (5, 5)
        g.gp0(0x2000_00FF); // a triangle larger than the box
        g.gp0(0x0000_0000); // v0 (0, 0)
        g.gp0(0x0000_000A); // v1 (10, 0)
        g.gp0(0x000A_0000); // v2 (0, 10)
        check(&mut pass, "scissor keeps inside-box pixel", px(g, 3, 3), 0x001F);
        check(&mut pass, "scissor drops outside-box pixel", px(g, 0, 0), 0x0000);
    }

    // --- flat rectangles: variable-size and fixed 1x1 ---------------------------------
    {
        let mut bus = Bus::new();
        let g = &mut bus.gpu;
        g.gp1(0x0000_0000);
        g.gp0(FULL_AREA);
        g.gp0(0x6000_00FF); // variable-size rect, colour 0x0000FF
        g.gp0(0x0000_0000); // position (0, 0)
        g.gp0((4 << 16) | 4); // size 4 wide x 4 tall
        check(&mut pass, "rect 4x4 filled corner", px(g, 3, 3), 0x001F);
        check(&mut pass, "rect 4x4 just outside", px(g, 4, 4), 0x0000);
        g.gp0(0x6800_00FF); // fixed 1x1 rect
        g.gp0((10 << 16) | 10); // position (10, 10)
        check(&mut pass, "rect 1x1 fixed size", px(g, 10, 10), 0x001F);
    }

    // --- flat lines: horizontal and vertical ------------------------------------------
    {
        let mut bus = Bus::new();
        let g = &mut bus.gpu;
        g.gp1(0x0000_0000);
        g.gp0(FULL_AREA);
        g.gp0(0x4000_00FF); // flat line ...
        g.gp0(0x0000_0000); // (0, 0) ...
        g.gp0(0x0000_000A); // to (10, 0) -> horizontal
        check(&mut pass, "h-line midpoint", px(g, 5, 0), 0x001F);
        check(&mut pass, "h-line off-axis empty", px(g, 5, 1), 0x0000);
        g.gp0(0x4000_00FF);
        g.gp0(0x0000_0000); // (0, 0) ...
        g.gp0(0x000A_0000); // to (0, 10) -> vertical
        check(&mut pass, "v-line midpoint", px(g, 0, 5), 0x001F);
    }

    // --- drawing offset (GP0 E5) shifts every primitive -------------------------------
    {
        let mut bus = Bus::new();
        let g = &mut bus.gpu;
        g.gp1(0x0000_0000);
        g.gp0(FULL_AREA);
        g.gp0(0xE500_0000 | (5 << 11) | 5); // offset (+5, +5)
        g.gp0(0x6800_00FF); // 1x1 rect at (0,0) ...
        g.gp0(0x0000_0000);
        check(&mut pass, "offset moves rect to (5,5)", px(g, 5, 5), 0x001F);
        check(&mut pass, "offset leaves origin empty", px(g, 0, 0), 0x0000);
    }

    // --- mask bit: check (write-protect) then force-set -------------------------------
    {
        let mut bus = Bus::new();
        let g = &mut bus.gpu;
        g.gp1(0x0000_0000);
        g.gp0(FULL_AREA);
        g.gp0(0xA000_0000); // upload a mask-bit-set pixel at (0,0) ...
        g.gp0(0x0000_0000); // dst (0, 0)
        g.gp0((1 << 16) | 1); // size 1x1
        g.gp0(0x0000_8000); // pixel = mask bit (15) set, colour 0
        g.gp0(0xE600_0002); // GP0(E6): check-mask on
        g.gp0(0x6800_00FF); // try to draw a 1x1 rect over the protected pixel
        g.gp0(0x0000_0000);
        check(&mut pass, "mask-check protects pixel", px(g, 0, 0), 0x8000);
    }
    {
        let mut bus = Bus::new();
        let g = &mut bus.gpu;
        g.gp1(0x0000_0000);
        g.gp0(FULL_AREA);
        g.gp0(0xE600_0001); // GP0(E6): force-set-mask on
        g.gp0(0x6800_00FF); // draw a 1x1 (colour 0x001F)
        g.gp0(0x0000_0000);
        check(&mut pass, "force-set-mask sets bit 15", px(g, 0, 0), 0x801F);
    }

    // --- Gouraud triangle with a uniform colour interpolates to that colour -----------
    {
        let mut bus = Bus::new();
        let g = &mut bus.gpu;
        g.gp1(0x0000_0000);
        g.gp0(FULL_AREA);
        g.gp0(0x3000_00FF); // Gouraud triangle, vertex-0 colour 0x0000FF
        g.gp0(0x0000_0000); // v0 (0, 0)
        g.gp0(0x0000_00FF); // colour 1
        g.gp0(0x0000_000A); // v1 (10, 0)
        g.gp0(0x0000_00FF); // colour 2
        g.gp0(0x000A_0000); // v2 (0, 10)
        check(&mut pass, "gouraud uniform-colour interior", px(g, 2, 2), 0x001F);
    }

    // ===== texture mapping =====================================================
    // Each test uploads a tiny texture (and CLUT) via A0, sets the texpage/CLUT/window via E1/E2 and
    // the command, draws a textured primitive at (100,100), then reads the result back with `px`.
    // Command colour 0x808080 is the neutral (identity) modulation value.

    // --- 4bpp via CLUT: nibble select + palette lookup --------------------------------
    {
        let mut bus = Bus::new();
        let g = &mut bus.gpu;
        g.gp1(0x0000_0000);
        g.gp0(FULL_AREA);
        upload(g, 0, 0, 1, 1, &[0x3210]); // 4bpp texels: nibbles give indices 0,1,2,3 for U=0..3
        upload(g, 0, 2, 2, 1, &[0x7FFF, 0x001F]); // CLUT at (0,2): entry0=white, entry1=red
        g.gp0(0xE100_0000); // E1: texpage 0 (texX/texY 0), depth 0 = 4bpp
        g.gp0(0x6C80_8080); // 1x1 textured rect, neutral colour
        g.gp0(0x0064_0064); // at (100,100)
        g.gp0(0x0080_0001); // U=1,V=0; CLUT id 0x80 -> (0,2). U=1 -> index 1 -> red
        check(&mut pass, "4bpp texture via CLUT", px(g, 100, 100), 0x001F);
    }

    // --- 8bpp via CLUT: byte select ---------------------------------------------------
    {
        let mut bus = Bus::new();
        let g = &mut bus.gpu;
        g.gp1(0x0000_0000);
        g.gp0(FULL_AREA);
        upload(g, 0, 0, 1, 1, &[0x0201]); // 8bpp: byte0=index1, byte1=index2
        upload(g, 0, 2, 3, 1, &[0x7FFF, 0x7FFF, 0x03E0]); // CLUT entry2 = green
        g.gp0(0xE100_0080); // E1: depth 1 = 8bpp (bit7)
        g.gp0(0x6C80_8080);
        g.gp0(0x0064_0064);
        g.gp0(0x0080_0001); // U=1 -> high byte -> index 2 -> green
        check(&mut pass, "8bpp texture via CLUT", px(g, 100, 100), 0x03E0);
    }

    // --- 15bpp direct colour ----------------------------------------------------------
    {
        let mut bus = Bus::new();
        let g = &mut bus.gpu;
        g.gp1(0x0000_0000);
        g.gp0(FULL_AREA);
        upload(g, 0, 0, 1, 1, &[0x7FFF]);
        g.gp0(0xE100_0100); // E1: depth 2 = 15bpp
        g.gp0(0x6C80_8080);
        g.gp0(0x0064_0064);
        g.gp0(0x0000_0000); // U=0,V=0,CLUT unused
        check(&mut pass, "15bpp direct texture", px(g, 100, 100), 0x7FFF);
    }

    // --- black-texel transparency (0x0000 transparent, 0x8000 opaque) -----------------
    {
        let mut bus = Bus::new();
        let g = &mut bus.gpu;
        g.gp1(0x0000_0000);
        g.gp0(FULL_AREA);
        upload(g, 0, 0, 2, 1, &[0x0000, 0x8000]); // texel0 = transparent black, texel1 = mask-black
        upload(g, 100, 100, 2, 1, &[0x1234, 0x1234]); // pre-fill both destinations
        g.gp0(0xE100_0100); // 15bpp
        g.gp0(0x6C80_8080);
        g.gp0(0x0064_0064); // (100,100), U=0 -> 0x0000 -> skip
        g.gp0(0x0000_0000);
        g.gp0(0x6C80_8080);
        g.gp0(0x0064_0065); // (101,100), U=1 -> 0x8000 -> opaque
        g.gp0(0x0000_0001);
        check(&mut pass, "black texel is transparent", px(g, 100, 100), 0x1234);
        check(&mut pass, "mask-black texel is opaque", px(g, 101, 100), 0x8000);
    }

    // --- colour modulation: out5 = (tex5 * col8) >> 7 ---------------------------------
    {
        let mut bus = Bus::new();
        let g = &mut bus.gpu;
        g.gp1(0x0000_0000);
        g.gp0(FULL_AREA);
        upload(g, 0, 0, 1, 1, &[0x0010]); // texel R = 16 (5-bit), G=B=0
        g.gp0(0xE100_0100); // 15bpp
        g.gp0(0x6C00_0040); // command colour R = 64 -> R modulates to (16*64)>>7 = 8
        g.gp0(0x0064_0064);
        g.gp0(0x0000_0000);
        check(&mut pass, "texture colour modulation", px(g, 100, 100), 0x0008);
    }

    // --- textured sprite X-flip (E1 bit 12) reverses U ---------------------------------
    {
        let mut bus = Bus::new();
        let g = &mut bus.gpu;
        g.gp1(0x0000_0000);
        g.gp0(FULL_AREA);
        upload(g, 0, 0, 2, 1, &[0x0001, 0x0002]); // texel(0)=1, texel(1)=2
        g.gp0(0xE100_1100); // 15bpp + X-flip (bit12)
        g.gp0(0x6480_8080); // variable-size textured rect
        g.gp0(0x0064_0064); // (100,100)
        g.gp0(0x0000_0000); // base U=0, V=0
        g.gp0(0x0001_0002); // size 2x1
        // X-flip reverses U (with the +1 hardware offset): screen dx=0 -> U=1 -> texel 2;
        // dx=1 -> U=0 -> texel 1.
        check(&mut pass, "sprite X-flip left", px(g, 100, 100), 0x0002);
        check(&mut pass, "sprite X-flip right", px(g, 101, 100), 0x0001);
    }

    // --- texture window wraps U within a sub-tile -------------------------------------
    {
        let mut bus = Bus::new();
        let g = &mut bus.gpu;
        g.gp1(0x0000_0000);
        g.gp0(FULL_AREA);
        upload(g, 0, 0, 9, 1, &[0x1234, 0, 0, 0, 0, 0, 0, 0, 0x5678]); // texel(0) and texel(8)
        g.gp0(0xE100_0100); // 15bpp
        g.gp0(0xE200_0001); // E2: mask X = 1 (8-pixel step) -> clears bit 3 of U
        g.gp0(0x6C80_8080);
        g.gp0(0x0064_0064);
        g.gp0(0x0000_0008); // U=8 -> windowed to 0 -> texel(0), NOT texel(8)
        check(&mut pass, "texture window wraps U", px(g, 100, 100), 0x1234);
    }

    // --- textured polygon latches its texpage into GPUSTAT ----------------------------
    {
        let mut bus = Bus::new();
        let g = &mut bus.gpu;
        g.gp1(0x0000_0000);
        g.gp0(FULL_AREA);
        g.gp0(0x2480_8080); // flat textured triangle
        g.gp0(0x0000_0000); // v0 (0,0), uv0+CLUT next
        g.gp0(0x0000_0000); // uv0=0, CLUT=0
        g.gp0(0x0000_0004); // v1 (4,0)
        g.gp0(0x0014_0000); // uv1=0, TEXPAGE=0x14 (high half)
        g.gp0(0x0004_0000); // v2 (0,4)
        g.gp0(0x0000_0000); // uv2=0
        check(&mut pass, "textured poly latches texpage", g.status() & 0x1FF, 0x14);
    }

    // ===== semi-transparency ===================================================
    // Blend an incoming pixel over a pre-filled "back" pixel B = (10,10,10) = 0x294A. The front
    // F = (20,4,0) comes from an untextured 1x1 rect with command colour 0x0020A0. The blend mode is
    // GP0(E1) bits 5-6; the semi-transparent enable is command-byte bit 1 (so `0x6A` = semi 1x1 rect,
    // `0x68` = opaque 1x1 rect).
    {
        let mut bus = Bus::new();
        let g = &mut bus.gpu;
        g.gp1(0x0000_0000);
        g.gp0(FULL_AREA);
        upload(g, 100, 100, 1, 1, &[0x294A]);
        g.gp0(0xE100_0000); // mode 0 = (B+F)/2
        g.gp0(0x6A00_20A0); // semi-transparent 1x1 rect
        g.gp0(0x0064_0064); // at (100,100)
        check(&mut pass, "semi mode 0 (average)", px(g, 100, 100), 0x14EF); // (15,7,5)
    }
    {
        let mut bus = Bus::new();
        let g = &mut bus.gpu;
        g.gp1(0x0000_0000);
        g.gp0(FULL_AREA);
        upload(g, 100, 100, 1, 1, &[0x294A]);
        g.gp0(0xE100_0020); // mode 1 = B+F
        g.gp0(0x6A00_20A0);
        g.gp0(0x0064_0064);
        check(&mut pass, "semi mode 1 (additive)", px(g, 100, 100), 0x29DE); // (30,14,10)
    }
    {
        let mut bus = Bus::new();
        let g = &mut bus.gpu;
        g.gp1(0x0000_0000);
        g.gp0(FULL_AREA);
        upload(g, 100, 100, 1, 1, &[0x7BDE]); // back (30,30,30)
        g.gp0(0xE100_0020); // mode 1 -> R,G clamp at 31
        g.gp0(0x6A00_20A0);
        g.gp0(0x0064_0064);
        check(&mut pass, "semi additive clamps to 31", px(g, 100, 100), 0x7BFF); // (31,31,30)
    }
    {
        let mut bus = Bus::new();
        let g = &mut bus.gpu;
        g.gp1(0x0000_0000);
        g.gp0(FULL_AREA);
        upload(g, 100, 100, 1, 1, &[0x294A]);
        g.gp0(0xE100_0040); // mode 2 = B-F (clamped at 0)
        g.gp0(0x6A00_20A0);
        g.gp0(0x0064_0064);
        check(&mut pass, "semi mode 2 (subtract)", px(g, 100, 100), 0x28C0); // (0,6,10)
    }
    {
        let mut bus = Bus::new();
        let g = &mut bus.gpu;
        g.gp1(0x0000_0000);
        g.gp0(FULL_AREA);
        upload(g, 100, 100, 1, 1, &[0x294A]);
        g.gp0(0xE100_0060); // mode 3 = B + F/4
        g.gp0(0x6A00_20A0);
        g.gp0(0x0064_0064);
        check(&mut pass, "semi mode 3 (quarter)", px(g, 100, 100), 0x296F); // (15,11,10)
    }
    {
        let mut bus = Bus::new();
        let g = &mut bus.gpu;
        g.gp1(0x0000_0000);
        g.gp0(FULL_AREA);
        upload(g, 100, 100, 1, 1, &[0x294A]);
        g.gp0(0xE100_0020);
        g.gp0(0x6800_20A0); // OPAQUE 1x1 rect (no semi bit) -> just overwrite
        g.gp0(0x0064_0064);
        check(&mut pass, "opaque rect ignores blend", px(g, 100, 100), 0x0094); // (20,4,0)
    }
    {
        // Textured: blending is gated per-pixel by the texel's STP bit (15).
        let mut bus = Bus::new();
        let g = &mut bus.gpu;
        g.gp1(0x0000_0000);
        g.gp0(FULL_AREA);
        upload(g, 0, 0, 2, 1, &[0x800A, 0x000A]); // texel0 = STP+R10, texel1 = R10 no STP
        upload(g, 100, 100, 2, 1, &[0x294A, 0x294A]); // back pixels
        g.gp0(0xE100_0120); // 15bpp + mode 1
        g.gp0(0x6680_8080); // semi-transparent textured rect, neutral colour
        g.gp0(0x0064_0064); // (100,100)
        g.gp0(0x0000_0000); // U=0, V=0, CLUT 0
        g.gp0(0x0001_0002); // size 2x1
        check(&mut pass, "textured STP set -> blended", px(g, 100, 100), 0xA954);
        check(&mut pass, "textured STP clear -> opaque", px(g, 101, 100), 0x000A);
    }
    {
        let mut bus = Bus::new();
        let g = &mut bus.gpu;
        g.gp1(0x0000_0000);
        g.gp0(FULL_AREA);
        upload(g, 100, 100, 1, 1, &[0x294A]);
        g.gp0(0xE100_0020); // mode 1
        g.gp0(0xE600_0001); // force-set mask
        g.gp0(0x6A00_20A0); // semi rect -> blended, then mask bit forced
        g.gp0(0x0064_0064);
        check(&mut pass, "blend + force-set mask", px(g, 100, 100), 0xA9DE);
    }
    {
        let mut bus = Bus::new();
        let g = &mut bus.gpu;
        g.gp1(0x0000_0000);
        g.gp0(FULL_AREA);
        upload(g, 100, 100, 1, 1, &[0xA94A]); // back already has the mask bit set
        g.gp0(0xE100_0020);
        g.gp0(0xE600_0002); // check-mask on
        g.gp0(0x6A00_20A0); // semi rect -> skipped (dest is masked)
        g.gp0(0x0064_0064);
        check(&mut pass, "check-mask skips blend", px(g, 100, 100), 0xA94A);
    }

    // ===== display timing (VBlank) ================================================
    // Drive the bus's catch-up `tick` directly (no CPU) and watch the GPU trip a frame: VBlank should
    // hit `I_STAT` bit 0 exactly when the beam crosses into the vertical blank (line 240, i.e.
    // NTSC_VBLANK_START), the `frame_ready` flag should latch (and clear on read), and GPUSTAT bit 31
    // (the interlace field) should toggle once per frame. (The scanline machine makes this the
    // vblank-start edge, not the whole-frame boundary — the visible image is complete at that point.)
    {
        let mut bus = Bus::new();
        bus.write32(0x1F80_1074, 0x0000_FFFF); // I_MASK: allow all sources (incl. VBlank, bit 0)
        let field_before = (bus.gpu.status() >> 31) & 1;

        // One cycle short of the vblank-start edge: no VBlank, no frame yet.
        bus.tick(gpu::NTSC_VBLANK_START - 1);
        check(&mut pass, "no VBlank before vblank start", (bus.read32(0x1F80_1070) >> 0) & 1, 0);
        check(&mut pass, "no frame before vblank start", bus.gpu.take_frame() as u32, 0);

        // Crossing into the vblank raises VBlank, latches the frame, and flips the field.
        bus.tick(2);
        check(&mut pass, "VBlank raised at vblank start", bus.read32(0x1F80_1070) & 1, 1);
        let field_after = (bus.gpu.status() >> 31) & 1;
        check(&mut pass, "GPUSTAT field bit toggled", field_before ^ field_after, 1);
        check(&mut pass, "frame_ready latched", bus.gpu.take_frame() as u32, 1);
        check(&mut pass, "frame_ready clears on read", bus.gpu.take_frame() as u32, 0);
    }

    // ===== root counters / timers (TIMER0/1/2) ====================================
    // Drive the bus's catch-up `tick` directly (no CPU) and program the timers through MMIO, exactly
    // as software would: counter at base+0x0, mode at +0x4, target at +0x8, with bases T0 0x1F80_1100,
    // T1 0x1F80_1110, T2 0x1F80_1120. I_STAT is 0x1F80_1070 (the timer sources are bits 4/5/6). One
    // subtlety baked into the setups: writing the mode register **zeroes the counter**, so any seed of
    // the counter value must come *after* the mode write (and the target before it).

    // --- counts the system clock, and the counter is writable -------------------------
    {
        let mut bus = Bus::new();
        bus.tick(100); // TIMER0 defaults to the system-clock source
        check(&mut pass, "timer0 counts system clock", bus.read32(0x1F80_1100), 100);
    }
    {
        let mut bus = Bus::new();
        bus.write32(0x1F80_1100, 0x1234); // a direct counter write (no latch reset)
        bus.tick(3);
        check(&mut pass, "timer0 counter is writable", bus.read32(0x1F80_1100), 0x1237);
    }

    // --- a mode write resets the counter and raises the IRQ flag (bit 10) -------------
    {
        let mut bus = Bus::new();
        bus.tick(50);
        bus.write32(0x1F80_1104, 0); // mode write -> counter back to 0, bit 10 high
        check(&mut pass, "mode write zeroes the counter", bus.read32(0x1F80_1100), 0);
        check(&mut pass, "mode write sets IRQ flag (bit 10)", (bus.read32(0x1F80_1104) >> 10) & 1, 1);
    }

    // --- TIMER2 system-clock/8 source divides by 8, carrying the remainder ------------
    {
        let mut bus = Bus::new();
        bus.write32(0x1F80_1124, 0x0200); // T2 mode: clock source = system clock / 8 (bits 8-9 = 2)
        bus.tick(8);
        check(&mut pass, "timer2 /8: 8 cycles -> 1", bus.read32(0x1F80_1120), 1);
        bus.tick(7);
        check(&mut pass, "timer2 /8: +7 cycles -> still 1", bus.read32(0x1F80_1120), 1);
        bus.tick(1); // the carried remainder (7) + 1 = 8 -> one more tick
        check(&mut pass, "timer2 /8: +1 cycle -> 2 (remainder carried)", bus.read32(0x1F80_1120), 2);
    }

    // --- reset-on-target (mode bit 3) wraps the counter at the target ------------------
    {
        let mut bus = Bus::new();
        bus.write32(0x1F80_1108, 10); // target first ...
        bus.write32(0x1F80_1104, 0x0008); // ... then mode: reset on target (bit 3)
        bus.tick(10);
        check(&mut pass, "reset-on-target wraps at target", bus.read32(0x1F80_1100), 0);
        bus.tick(5);
        check(&mut pass, "counter continues after target reset", bus.read32(0x1F80_1100), 5);
    }

    // --- free-run wraps past 0xFFFF back to 0 -----------------------------------------
    {
        let mut bus = Bus::new();
        bus.write32(0x1F80_1104, 0); // mode: free-run, reset on the 0xFFFF wrap
        bus.write32(0x1F80_1100, 0xFFFE); // seed near the wrap (after the mode write)
        bus.tick(3); // 0xFFFE -> 0xFFFF -> 0x0000 -> 0x0001
        check(&mut pass, "counter wraps 0xFFFF -> 0", bus.read32(0x1F80_1100), 1);
    }

    // --- reached-target latch (bit 11) sets, then clears on a mode read ----------------
    {
        let mut bus = Bus::new();
        bus.write32(0x1F80_1108, 5);
        bus.write32(0x1F80_1104, 0); // free-run, no reset/IRQ — just watch the latch
        bus.tick(5);
        check(&mut pass, "reached-target latch set (bit 11)", (bus.read32(0x1F80_1104) >> 11) & 1, 1);
        check(&mut pass, "reached-target clears on mode read", (bus.read32(0x1F80_1104) >> 11) & 1, 0);
    }

    // --- reached-0xFFFF latch (bit 12) sets on the wrap -------------------------------
    {
        let mut bus = Bus::new();
        bus.write32(0x1F80_1104, 0);
        bus.write32(0x1F80_1100, 0xFFFF); // one tick from the wrap (after the mode write)
        bus.tick(1);
        check(&mut pass, "reached-0xFFFF latch set (bit 12)", (bus.read32(0x1F80_1104) >> 12) & 1, 1);
    }

    // --- the target IRQ pulls the I_STAT line -----------------------------------------
    {
        let mut bus = Bus::new();
        bus.write32(0x1F80_1108, 20);
        bus.write32(0x1F80_1104, 0x0010); // mode bit 4: IRQ when counter == target
        bus.tick(20);
        check(&mut pass, "target IRQ raises I_STAT (TIMER0 bit 4)", (bus.read32(0x1F80_1070) >> 4) & 1, 1);
    }

    // --- one-shot fires exactly once until re-armed (mode bit 6 = 0) -------------------
    {
        let mut bus = Bus::new();
        bus.write32(0x1F80_1118, 10);
        bus.write32(0x1F80_1114, 0x0010); // T1: IRQ on target, one-shot
        bus.tick(10);
        check(&mut pass, "one-shot fires (I_STAT TIMER1 bit 5)", (bus.read32(0x1F80_1070) >> 5) & 1, 1);
        bus.write32(0x1F80_1070, 0); // acknowledge (I_STAT is write-0-to-clear)
        bus.tick(0x1_0000); // counter sweeps the whole range, hitting the target again
        check(&mut pass, "one-shot stays silent until re-armed", (bus.read32(0x1F80_1070) >> 5) & 1, 0);
    }

    // --- repeat mode re-fires every time the condition recurs (mode bit 6 = 1) ---------
    {
        let mut bus = Bus::new();
        bus.write32(0x1F80_1118, 10);
        bus.write32(0x1F80_1114, 0x0058); // T1: IRQ on target (4) + repeat (6) + reset-on-target (3)
        bus.tick(10);
        check(&mut pass, "repeat IRQ fires first time", (bus.read32(0x1F80_1070) >> 5) & 1, 1);
        bus.write32(0x1F80_1070, 0); // acknowledge
        bus.tick(10);
        check(&mut pass, "repeat IRQ fires again after reset", (bus.read32(0x1F80_1070) >> 5) & 1, 1);
    }

    // ===== timer GPU clock sources + sync modes ===================================
    // These exercise the seam M4.5b fills: TIMER0's dot clock and TIMER1's hblank now advance from the
    // GPU's scanline/dot machine, and the sync modes pause/reset the counters around h/v-blank. Set the
    // resolution/video-mode with GP1(08) first where the dot rate matters; `gpu::CYCLES_PER_SCANLINE`
    // and `gpu::NTSC_VBLANK_START` are the scanline/vblank boundaries.

    // --- TIMER0 dot clock advances at the resolution-derived rate ----------------------
    {
        let mut bus = Bus::new();
        bus.gpu.gp1(0x0800_0001); // GP1(08): hres 320 -> dot divider 8, NTSC
        bus.write32(0x1F80_1104, 0x0100); // T0 mode: source = dot clock (bits 8-9 = 1)
        bus.tick(56); // 56*11 / (7*8) = 11 dots exactly
        check(&mut pass, "timer0 dot clock advances", bus.read32(0x1F80_1100), 11);
    }

    // --- the dot rate scales with horizontal resolution (640 is 2x 320) ----------------
    // Halving the divider (8 -> 4) doubles the dot rate, so the same 56 cycles that gave 11 dots
    // in the divider-8 case just above give exactly 22 here.
    {
        let mut bus = Bus::new();
        bus.gpu.gp1(0x0800_0003); // hres 640 -> divider 4
        bus.write32(0x1F80_1104, 0x0100);
        bus.tick(56); // 56*11 / (7*4) = 22
        check(&mut pass, "dot rate scales with resolution", bus.read32(0x1F80_1100), 22);
    }

    // --- TIMER1 hblank advances one tick per scanline ---------------------------------
    {
        let mut bus = Bus::new();
        bus.write32(0x1F80_1114, 0x0100); // T1 mode: source = hblank (bits 8-9 = 1)
        bus.tick(gpu::CYCLES_PER_SCANLINE * 10 + 5); // cross 10 scanline boundaries in one step
        check(&mut pass, "timer1 hblank advances per scanline", bus.read32(0x1F80_1110), 10);
    }

    // --- TIMER2 sync stop vs free-run (its two sync bits only choose those) ------------
    {
        let mut bus = Bus::new();
        bus.write32(0x1F80_1124, 0x0001); // sync enable + mode 0 -> STOP
        bus.tick(500);
        check(&mut pass, "timer2 sync stop (mode 0)", bus.read32(0x1F80_1120), 0);
        let mut b1 = Bus::new();
        b1.write32(0x1F80_1124, 0x0003); // sync enable + mode 1 -> free-run
        b1.tick(500);
        check(&mut pass, "timer2 sync free-run (mode 1)", b1.read32(0x1F80_1120), 500);
        let mut b3 = Bus::new();
        b3.write32(0x1F80_1124, 0x0007); // sync enable + mode 3 -> STOP
        b3.tick(500);
        check(&mut pass, "timer2 sync stop (mode 3)", b3.read32(0x1F80_1120), 0);
    }

    // --- TIMER0 reset-at-hblank (sync mode 1) -----------------------------------------
    // Count up within a scanline, then cross the hblank: the counter zeroes and counts the rest.
    {
        let mut bus = Bus::new();
        bus.write32(0x1F80_1104, 0x0003); // T0: sync enable + mode 1, sysclock source
        bus.tick(gpu::CYCLES_PER_SCANLINE - 100); // no hblank crossed yet -> counts up
        check(&mut pass, "timer0 pre-hblank counts up", bus.read32(0x1F80_1100), gpu::CYCLES_PER_SCANLINE - 100);
        bus.tick(100); // crosses the hblank -> reset to 0, then counts the 100
        check(&mut pass, "timer0 reset-at-hblank", bus.read32(0x1F80_1100), 100);
    }

    // --- TIMER1 reset-at-vblank, and pause-during-vblank ------------------------------
    {
        // reset-at-vblank (mode 1): cross the vblank start and the counter zeroes + counts the rest.
        let mut bus = Bus::new();
        bus.write32(0x1F80_1114, 0x0003); // T1: sync enable + mode 1, sysclock source
        bus.tick(gpu::NTSC_VBLANK_START - 100); // up to just before vblank (no reset yet)
        bus.tick(100); // crosses vblank start -> reset, then +100
        check(&mut pass, "timer1 reset-at-vblank", bus.read32(0x1F80_1110), 100);
    }
    {
        // pause-during-vblank (mode 0): a step that ends in the vblank region doesn't count.
        let mut bus = Bus::new();
        bus.write32(0x1F80_1114, 0x0001); // T1: sync enable + mode 0, sysclock source
        bus.tick(gpu::CYCLES_PER_SCANLINE * 239 + 100); // last visible scanline -> counter counted
        let before = bus.read32(0x1F80_1110);
        check(&mut pass, "timer1 counted during visible lines", (before > 0) as u32, 1);
        bus.tick(gpu::CYCLES_PER_SCANLINE); // crosses into vblank, ends there -> paused
        check(&mut pass, "timer1 pause-during-vblank holds", bus.read32(0x1F80_1110), before);
    }

    // --- sync mode 3: pause until the first blank event, then free-run -----------------
    {
        let mut bus = Bus::new();
        bus.write32(0x1F80_1104, 0x0007); // T0: sync enable + mode 3, sysclock source
        bus.tick(gpu::CYCLES_PER_SCANLINE - 50); // no hblank yet -> still paused
        check(&mut pass, "timer0 mode3 paused before first hblank", bus.read32(0x1F80_1100), 0);
        bus.tick(100); // first hblank -> starts running
        let c = bus.read32(0x1F80_1100);
        check(&mut pass, "timer0 mode3 runs after first hblank", (c > 0) as u32, 1);
        bus.tick(200); // thereafter free-runs at the sysclock rate
        check(&mut pass, "timer0 mode3 free-runs", bus.read32(0x1F80_1100), c + 200);
    }

    // ===== CD-ROM drive ===========================================================
    // The drive is a second processor: a command produces interrupts *later*, and the next interrupt
    // is held until the host acknowledges the previous one (write IF). These tests drive that handshake
    // directly. Registers: 0x1800 status/index, 0x1801 command/response, 0x1802 param/IE/data, 0x1803
    // request/IF. The sequence is always: set index 0 -> push params -> write command -> tick -> read
    // response + IF (at index 1) -> ack. `tick`s use round numbers past the (coarse) command delays.

    // --- index select + idle FIFO-ready bits ------------------------------------------
    {
        let mut bus = Bus::new();
        bus.write32(0x1F80_1800, 0); // index 0
        let s = bus.read32(0x1F80_1800);
        check(&mut pass, "cd index 0 selected", s & 3, 0);
        check(&mut pass, "cd param FIFO empty (bit3)", (s >> 3) & 1, 1);
        check(&mut pass, "cd param FIFO ready (bit4)", (s >> 4) & 1, 1);
        bus.write32(0x1F80_1800, 1); // index 1
        check(&mut pass, "cd index 1 selected", bus.read32(0x1F80_1800) & 3, 1);
    }

    // --- pushing parameters clears PRMEMPT --------------------------------------------
    {
        let mut bus = Bus::new();
        bus.write32(0x1F80_1800, 0);
        bus.write32(0x1F80_1802, 0x11);
        bus.write32(0x1F80_1802, 0x22);
        check(&mut pass, "cd param FIFO not empty (bit3=0)", (bus.read32(0x1F80_1800) >> 3) & 1, 0);
    }

    // --- Test 0x20 returns the controller version ------------------------------------
    {
        let mut bus = Bus::new();
        bus.write32(0x1F80_1800, 0);
        bus.write32(0x1F80_1802, 0x20); // sub-function
        bus.write32(0x1F80_1801, 0x19); // Test
        bus.tick(60_000);
        check(&mut pass, "cd response ready (bit5)", (bus.read32(0x1F80_1800) >> 5) & 1, 1);
        bus.write32(0x1F80_1800, 1);
        check(&mut pass, "cd Test IF = INT3", bus.read32(0x1F80_1803) & 7, 3);
        check(&mut pass, "cd Test ver[0]", bus.read32(0x1F80_1801) & 0xFF, 0x94);
        bus.read32(0x1F80_1801); // 0x09
        bus.read32(0x1F80_1801); // 0x19
        check(&mut pass, "cd Test ver[3]", bus.read32(0x1F80_1801) & 0xFF, 0xC0);
    }

    // --- Getstat returns the idle status byte -----------------------------------------
    {
        let mut bus = Bus::new();
        bus.write32(0x1F80_1800, 0);
        bus.write32(0x1F80_1801, 0x01); // Getstat
        bus.tick(60_000);
        check(&mut pass, "cd Getstat status 0x02", bus.read32(0x1F80_1801) & 0xFF, 0x02);
    }

    // --- GetID two-phase: INT3, held until ack, then INT2 with the 'SCEA' region (the key case) ---
    {
        let mut bus = Bus::new();
        bus.cdrom.load_disc(synth_disc());
        bus.write32(0x1F80_1074, 0x0000_FFFF); // unmask all I_STAT sources
        bus.write32(0x1F80_1800, 1);
        bus.write32(0x1F80_1802, 0x1F); // IE = enable all CD interrupts (0x1802@idx1)
        bus.write32(0x1F80_1800, 0);
        bus.write32(0x1F80_1801, 0x1A); // GetID
        bus.tick(60_000); // phase A: INT3
        check(&mut pass, "cd GetID INT3 raises I_STAT bit2", (bus.read32(0x1F80_1070) >> 2) & 1, 1);
        bus.write32(0x1F80_1800, 1);
        check(&mut pass, "cd GetID phase A = INT3", bus.read32(0x1F80_1803) & 7, 3);
        check(&mut pass, "cd GetID status byte", bus.read32(0x1F80_1801) & 0xFF, 0x02);
        bus.tick(60_000); // INT2 must NOT arrive un-acked
        check(&mut pass, "cd GetID INT2 held until ack", bus.read32(0x1F80_1803) & 7, 3);
        bus.write32(0x1F80_1070, 0); // clear I_STAT
        bus.write32(0x1F80_1803, 0x07); // ack INT3
        bus.tick(60_000); // phase B: INT2
        check(&mut pass, "cd GetID phase B = INT2", bus.read32(0x1F80_1803) & 7, 2);
        // (the bit-2 I_STAT raise is already pinned by phase A above and the IE-gating case below)
        bus.read32(0x1F80_1801); // status
        bus.read32(0x1F80_1801); // flags
        bus.read32(0x1F80_1801); // disc type
        bus.read32(0x1F80_1801); // 0x00
        check(&mut pass, "cd GetID region 'S'", bus.read32(0x1F80_1801) & 0xFF, b'S' as u32);
        check(&mut pass, "cd GetID region 'C'", bus.read32(0x1F80_1801) & 0xFF, b'C' as u32);
        check(&mut pass, "cd GetID region 'E'", bus.read32(0x1F80_1801) & 0xFF, b'E' as u32);
        check(&mut pass, "cd GetID region 'A'", bus.read32(0x1F80_1801) & 0xFF, b'A' as u32);
    }

    // --- IE gates the CPU line: IF still latches, but I_STAT bit2 stays low ------------
    {
        let mut bus = Bus::new();
        bus.cdrom.load_disc(synth_disc());
        bus.write32(0x1F80_1074, 0x0000_FFFF); // I_MASK open; IE stays 0
        bus.write32(0x1F80_1800, 0);
        bus.write32(0x1F80_1801, 0x1A); // GetID
        bus.tick(60_000);
        bus.write32(0x1F80_1800, 1);
        check(&mut pass, "cd IF latches with IE=0", bus.read32(0x1F80_1803) & 7, 3);
        check(&mut pass, "cd IE=0 gates I_STAT bit2", (bus.read32(0x1F80_1070) >> 2) & 1, 0);
    }

    // --- no disc: GetID errors with INT5 ----------------------------------------------
    {
        let mut bus = Bus::new();
        bus.write32(0x1F80_1800, 0);
        bus.write32(0x1F80_1801, 0x1A); // GetID, no disc
        bus.tick(60_000); // INT3
        bus.write32(0x1F80_1800, 1);
        bus.write32(0x1F80_1803, 0x07); // ack INT3
        bus.tick(60_000); // INT5
        check(&mut pass, "cd no-disc GetID -> INT5", bus.read32(0x1F80_1803) & 7, 5);
    }

    // --- Setloc then SeekL: two-phase, INT2 only after the seek latency ----------------
    {
        let mut bus = Bus::new();
        bus.write32(0x1F80_1800, 0);
        bus.write32(0x1F80_1802, 0x00); // MM
        bus.write32(0x1F80_1802, 0x02); // SS
        bus.write32(0x1F80_1802, 0x00); // FF -> LBA 0
        bus.write32(0x1F80_1801, 0x02); // Setloc
        bus.tick(60_000);
        bus.write32(0x1F80_1800, 1);
        bus.write32(0x1F80_1803, 0x07); // ack
        bus.write32(0x1F80_1800, 0);
        bus.write32(0x1F80_1801, 0x15); // SeekL
        bus.tick(60_000);
        bus.write32(0x1F80_1800, 1);
        check(&mut pass, "cd SeekL INT3 status seeking", bus.read32(0x1F80_1801) & 0xFF, 0x42);
        check(&mut pass, "cd SeekL phase A = INT3", bus.read32(0x1F80_1803) & 7, 3);
        bus.write32(0x1F80_1803, 0x07); // ack
        bus.tick(500_000); // < SEEK_DELAY
        check(&mut pass, "cd SeekL INT2 not before seek delay", bus.read32(0x1F80_1803) & 7, 0);
        bus.tick(600_000); // past the seek delay
        check(&mut pass, "cd SeekL phase B = INT2", bus.read32(0x1F80_1803) & 7, 2);
    }

    // --- Setmode stores the double-speed flag -----------------------------------------
    {
        let mut bus = Bus::new();
        bus.write32(0x1F80_1800, 0);
        bus.write32(0x1F80_1802, 0x80); // double speed
        bus.write32(0x1F80_1801, 0x0E); // Setmode
        bus.tick(60_000);
        check(&mut pass, "cd Setmode stored", bus.cdrom.debug_mode() as u32, 0x80);
    }

    // --- ReadN streams INT1 sectors; Request copies the user data to the data FIFO -----
    {
        let mut bus = Bus::new();
        bus.cdrom.load_disc(synth_disc());
        bus.write32(0x1F80_1800, 0);
        bus.write32(0x1F80_1802, 0x00);
        bus.write32(0x1F80_1802, 0x02);
        bus.write32(0x1F80_1802, 0x00); // Setloc LBA 0
        bus.write32(0x1F80_1801, 0x02);
        bus.tick(60_000);
        bus.write32(0x1F80_1800, 1);
        bus.write32(0x1F80_1803, 0x07); // ack Setloc
        bus.write32(0x1F80_1800, 0);
        bus.write32(0x1F80_1801, 0x06); // ReadN
        bus.tick(60_000);
        bus.write32(0x1F80_1800, 1);
        check(&mut pass, "cd ReadN INT3 ack", bus.read32(0x1F80_1803) & 7, 3);
        bus.write32(0x1F80_1803, 0x07); // ack INT3
        bus.tick(500_000); // < SEEK_DELAY
        check(&mut pass, "cd ReadN no sector before seek", bus.read32(0x1F80_1803) & 7, 0);
        bus.tick(600_000); // first sector
        check(&mut pass, "cd ReadN INT1 sector ready", bus.read32(0x1F80_1803) & 7, 1);
        bus.write32(0x1F80_1800, 0);
        bus.write32(0x1F80_1803, 0x80); // Request want-data
        check(&mut pass, "cd data FIFO ready (bit6)", (bus.read32(0x1F80_1800) >> 6) & 1, 1);
        check(&mut pass, "cd sector 0 data byte", bus.read32(0x1F80_1802) & 0xFF, 0xA0);
        bus.write32(0x1F80_1800, 1);
        bus.write32(0x1F80_1803, 0x07); // ack INT1
        bus.tick(451_585); // one sector period -> next INT1
        check(&mut pass, "cd ReadN streams next sector", bus.read32(0x1F80_1803) & 7, 1);
        bus.write32(0x1F80_1800, 0);
        bus.write32(0x1F80_1803, 0x80); // Request sector 1
        check(&mut pass, "cd sector 1 data byte", bus.read32(0x1F80_1802) & 0xFF, 0xA1);
    }

    // --- GetlocL gates on a valid position (ps1-tests cdrom/getloc) -------------------
    // GetlocL reports the header of the sector under the head; with none (a bare reset) it errors
    // with INT5, and after a SeekL it succeeds with the sought position.
    {
        let mut bus = Bus::new();
        bus.cdrom.load_disc(synth_disc());
        // (a) GetlocL before any seek/read -> INT5 error.
        bus.write32(0x1F80_1800, 0);
        bus.write32(0x1F80_1801, 0x10); // GetlocL
        bus.tick(60_000);
        bus.write32(0x1F80_1800, 1); // index 1 to read the Interrupt Flag (not IE)
        check(&mut pass, "cd GetlocL errors before a seek/read", bus.read32(0x1F80_1803) & 7, 5);
        bus.write32(0x1F80_1803, 0x07); // ack
        // (b) Setloc 00:02:04 (LBA 4) -> SeekL -> complete; then GetlocL succeeds with that frame.
        bus.write32(0x1F80_1800, 0);
        bus.write32(0x1F80_1802, 0x00);
        bus.write32(0x1F80_1802, 0x02);
        bus.write32(0x1F80_1802, 0x04); // BCD 00:02:04 -> LBA 4
        bus.write32(0x1F80_1801, 0x02); // Setloc
        bus.tick(60_000);
        bus.write32(0x1F80_1800, 1);
        bus.write32(0x1F80_1803, 0x07);
        bus.write32(0x1F80_1800, 0);
        bus.write32(0x1F80_1801, 0x15); // SeekL
        bus.tick(60_000);
        bus.write32(0x1F80_1800, 1);
        bus.write32(0x1F80_1803, 0x07); // ack INT3
        bus.tick(1_100_000); // past the seek delay -> INT2 complete
        bus.write32(0x1F80_1803, 0x07); // ack INT2
        bus.write32(0x1F80_1800, 0);
        bus.write32(0x1F80_1801, 0x10); // GetlocL again
        bus.tick(60_000);
        bus.write32(0x1F80_1800, 1); // index 1 to read the Interrupt Flag
        check(&mut pass, "cd GetlocL succeeds after a seek", bus.read32(0x1F80_1803) & 7, 3);
        let _m = bus.read32(0x1F80_1801) & 0xFF; // minute (00)
        let _s = bus.read32(0x1F80_1801) & 0xFF; // second (02)
        check(&mut pass, "cd GetlocL reports the sought frame (04)", bus.read32(0x1F80_1801) & 0xFF, 0x04);
    }

    // --- SeekL past the end of the disc fails with INT5 + seek-error status -----------
    {
        let mut bus = Bus::new();
        bus.cdrom.load_disc(synth_disc()); // 16 sectors (LBA 0..15)
        bus.write32(0x1F80_1800, 0);
        bus.write32(0x1F80_1802, 0x00);
        bus.write32(0x1F80_1802, 0x03);
        bus.write32(0x1F80_1802, 0x00); // BCD 00:03:00 -> LBA 75, well past the 16-sector disc
        bus.write32(0x1F80_1801, 0x02); // Setloc
        bus.tick(60_000);
        bus.write32(0x1F80_1800, 1);
        bus.write32(0x1F80_1803, 0x07); // ack Setloc
        bus.write32(0x1F80_1800, 0);
        bus.write32(0x1F80_1801, 0x15); // SeekL
        bus.tick(60_000);
        bus.write32(0x1F80_1800, 1);
        check(&mut pass, "cd SeekL past end: INT3 ack", bus.read32(0x1F80_1803) & 7, 3);
        bus.write32(0x1F80_1803, 0x07); // ack INT3
        bus.tick(1_100_000); // past the seek delay -> the error phase
        check(&mut pass, "cd SeekL past end: INT5 error", bus.read32(0x1F80_1803) & 7, 5);
        check(&mut pass, "cd SeekL past end: seek-error status 0x04", bus.read32(0x1F80_1801) & 0xFF, 0x04);
    }

    // --- a sector pulled into RAM via DMA channel 3 -----------------------------------
    {
        let mut bus = Bus::new();
        cd_begin_read(&mut bus); // Setloc LBA 0 -> ReadN -> INT3 acked
        bus.tick(1_100_000); // first sector loaded
        bus.write32(0x1F80_1800, 0);
        bus.write32(0x1F80_1803, 0x80); // fill data FIFO with sector 0 (2048 bytes of 0xA0)
        // DMA channel 3: device -> RAM. MADR 0x1F8010B0, BCR 0x1F8010B4, CHCR 0x1F8010B8.
        bus.write32(0x1F80_10F0, bus.read32(0x1F80_10F0) | (1 << 15)); // enable ch3 (DPCR bit 15)
        bus.write32(0x1F80_10B0, 0x0000_3000); // MADR = 0x3000
        bus.write32(0x1F80_10B4, (1 << 16) | 0x200); // BCR = 1 block x 512 words = 2048 bytes
        bus.write32(0x1F80_10B8, 0x0100_0200); // CHCR = start | sync 1 | device->RAM
        check(&mut pass, "cd DMA ch3 sector -> RAM", bus.read32(0x0000_3000), 0xA0A0_A0A0);
    }

    // --- MSF <-> LBA conversion (BCD, with the 150-frame pregap) -----------------------
    {
        use crate::cdrom::{lba_to_msf_bcd, msf_bcd_to_lba};
        check(&mut pass, "cd MSF->LBA 00:02:00", msf_bcd_to_lba(0x00, 0x02, 0x00), 0);
        check(&mut pass, "cd MSF->LBA 00:02:74", msf_bcd_to_lba(0x00, 0x02, 0x74), 74);
        check(&mut pass, "cd MSF->LBA 00:03:00", msf_bcd_to_lba(0x00, 0x03, 0x00), 75);
        let (m, s, f) = lba_to_msf_bcd(75);
        let packed = ((m as u32) << 16) | ((s as u32) << 8) | f as u32;
        check(&mut pass, "cd LBA->MSF 75 round-trip", packed, 0x00_0300);
    }

    // --- Init: two-phase, and it resets the mode --------------------------------------
    {
        let mut bus = Bus::new();
        bus.write32(0x1F80_1800, 0);
        bus.write32(0x1F80_1802, 0x80); // set double speed first
        bus.write32(0x1F80_1801, 0x0E); // Setmode
        bus.tick(60_000);
        bus.write32(0x1F80_1800, 1);
        bus.write32(0x1F80_1803, 0x07);
        bus.write32(0x1F80_1800, 0);
        bus.write32(0x1F80_1801, 0x0A); // Init
        bus.tick(80_000); // > INIT_ACK
        bus.write32(0x1F80_1800, 1);
        check(&mut pass, "cd Init INT3", bus.read32(0x1F80_1803) & 7, 3);
        bus.write32(0x1F80_1803, 0x07); // ack
        bus.tick(100_000); // < INIT_DONE
        check(&mut pass, "cd Init INT2 not yet", bus.read32(0x1F80_1803) & 7, 0);
        bus.tick(500_000); // past INIT_DONE
        check(&mut pass, "cd Init INT2 complete", bus.read32(0x1F80_1803) & 7, 2);
        check(&mut pass, "cd Init reset mode", bus.cdrom.debug_mode() as u32, 0);
    }

    // --- Pause stops the streaming read -----------------------------------------------
    {
        let mut bus = Bus::new();
        cd_begin_read(&mut bus); // Setloc LBA 0 -> ReadN -> INT3 acked
        bus.tick(1_100_000); // first INT1
        bus.write32(0x1F80_1803, 0x07); // ack INT1
        bus.write32(0x1F80_1800, 0);
        bus.write32(0x1F80_1801, 0x09); // Pause
        bus.tick(60_000);
        bus.write32(0x1F80_1800, 1);
        check(&mut pass, "cd Pause INT3", bus.read32(0x1F80_1803) & 7, 3);
        bus.write32(0x1F80_1803, 0x07); // ack
        // A Pause issued *mid-read* doesn't complete one ack later — the drive has to finish the
        // in-flight sector and wind the read engine down, so the INT2 arrives ~PAUSE_DONE (~1M cycles)
        // after the ack, not ~50k. (ps1-tests cdrom/timing measures this completion at ~1.01M cycles.)
        bus.tick(60_000); // < PAUSE_DONE
        check(&mut pass, "cd Pause INT2 not yet", bus.read32(0x1F80_1803) & 7, 0);
        bus.tick(1_100_000); // past PAUSE_DONE
        check(&mut pass, "cd Pause INT2 complete", bus.read32(0x1F80_1803) & 7, 2);
        bus.write32(0x1F80_1803, 0x07); // ack
        bus.tick(1_000_000); // longer than a sector period — the stream stays stopped
        check(&mut pass, "cd Pause stopped the stream", bus.read32(0x1F80_1803) & 7, 0);
    }

    // --- GTE (COP2) register file + moves (M6.0) ---------------------------------------
    // The GTE is a CPU coprocessor; its registers are reached via MTC2/MFC2 (data) and CTC2/CFC2
    // (control), gated on SR.CU2. These pin the read/write *quirks* the ps1-tests gte/test-all ROM
    // checks first — sign/zero-extension, the screen-XY FIFO, IRGB/ORGB, LZCS/LZCR, the H read-back
    // bug, and the FLAG error-summary bit. (The geometry commands themselves arrive in M6.1+.) Each
    // program ends in a NOP so a load-delayed MFC2/CFC2 result settles into the register file first.
    {
        // COP2 ops fault unless software enabled the coprocessor — set SR.CU2, then run the program.
        let gte_run = |prog: &[u32]| -> Cpu {
            let mut cpu = build(prog);
            cpu.cop0.sr |= 1 << 30; // SR.CU2 = "GTE usable"
            run(&mut cpu, prog.len());
            cpu
        };

        // A plain 32-bit data register (MAC0, d24) round-trips verbatim.
        let cpu = gte_run(&[lui(1, 0xDEAD), ori(1, 1, 0xBEEF), mtc2(1, 24), mfc2(2, 24), NOP]);
        check(&mut pass, "gte MAC0 round-trip", cpu.regs[2], 0xDEAD_BEEF);

        // IR1 (d9) is 16-bit signed: 0xFFFF reads back sign-extended.
        let cpu = gte_run(&[ori(1, 0, 0xFFFF), mtc2(1, 9), mfc2(2, 9), NOP]);
        check(&mut pass, "gte IR1 sign-extended", cpu.regs[2], 0xFFFF_FFFF);

        // VZ0 (d1) is 16-bit signed; a positive value zero-fills the top half.
        let cpu = gte_run(&[ori(1, 0, 0x1234), mtc2(1, 1), mfc2(2, 1), NOP]);
        check(&mut pass, "gte VZ0 positive", cpu.regs[2], 0x0000_1234);

        // OTZ (d7) is 16-bit UNSIGNED: 0xFFFF reads back zero-extended (not sign-extended).
        let cpu = gte_run(&[ori(1, 0, 0xFFFF), mtc2(1, 7), mfc2(2, 7), NOP]);
        check(&mut pass, "gte OTZ zero-extended", cpu.regs[2], 0x0000_FFFF);

        // SZ3 (d19) is 16-bit unsigned; the high half of the written word is dropped on read.
        let cpu = gte_run(&[lui(1, 0x1234), ori(1, 1, 0xFFFF), mtc2(1, 19), mfc2(2, 19), NOP]);
        check(&mut pass, "gte SZ3 zero-extended", cpu.regs[2], 0x0000_FFFF);

        // SXYP (d15): writing pushes the screen-XY FIFO (SXY0<-SXY1<-SXY2<-val); reading mirrors SXY2.
        let cpu = gte_run(&[
            ori(1, 0, 0x1111), mtc2(1, 12), // SXY0
            ori(1, 0, 0x2222), mtc2(1, 13), // SXY1
            ori(1, 0, 0x3333), mtc2(1, 14), // SXY2
            ori(1, 0, 0x4444), mtc2(1, 15), // SXYP write -> push: SXY0=2222, SXY1=3333, SXY2=4444
            mfc2(2, 15),                    // r2 <- SXYP (mirrors SXY2 == 4444)
            mfc2(3, 12),                    // r3 <- SXY0 (== old SXY1 == 2222, proving the push)
            NOP,
        ]);
        check(&mut pass, "gte SXYP mirrors SXY2", cpu.regs[2], 0x0000_4444);
        check(&mut pass, "gte SXY FIFO pushed", cpu.regs[3], 0x0000_2222);

        // IRGB (d28) write unpacks a 5:5:5 colour into IR1/IR2/IR3 (each field *0x80); ORGB (d29)
        // re-packs them back to 5:5:5. Write 0x7FFF -> IR1=0xF80; read ORGB -> 0x7FFF.
        let cpu = gte_run(&[ori(1, 0, 0x7FFF), mtc2(1, 28), mfc2(2, 9), mfc2(3, 29), NOP]);
        check(&mut pass, "gte IRGB unpacks to IR1", cpu.regs[2], 0x0000_0F80);
        check(&mut pass, "gte ORGB re-packs IR", cpu.regs[3], 0x0000_7FFF);

        // ORGB (d29) is read-only: writing it must not disturb IR1.
        let cpu = gte_run(&[
            ori(1, 0, 0), mtc2(1, 9), // IR1 = 0
            ori(1, 0, 0x7FFF), mtc2(1, 29), // write ORGB (ignored)
            mfc2(2, 9), NOP, // IR1 still 0
        ]);
        check(&mut pass, "gte ORGB write ignored", cpu.regs[2], 0x0000_0000);

        // LZCS/LZCR (d30/d31): LZCR = count of LZCS's leading sign bits. 0x00FFFFFF -> 8 leading zeros.
        let cpu = gte_run(&[lui(1, 0x00FF), ori(1, 1, 0xFFFF), mtc2(1, 30), mfc2(2, 31), NOP]);
        check(&mut pass, "gte LZCR leading zeros", cpu.regs[2], 8);

        // LZCR for a negative LZCS counts leading ones instead: 0xFFF00000 -> 12.
        let cpu = gte_run(&[lui(1, 0xFFF0), mtc2(1, 30), mfc2(2, 31), NOP]);
        check(&mut pass, "gte LZCR leading ones", cpu.regs[2], 12);

        // LZCS (d30) itself reads back raw (it's the input value, not the count).
        let cpu = gte_run(&[lui(1, 0x00FF), ori(1, 1, 0xFFFF), mtc2(1, 30), mfc2(2, 30), NOP]);
        check(&mut pass, "gte LZCS raw read", cpu.regs[2], 0x00FF_FFFF);

        // A plain 32-bit control register (TRX, c5) round-trips via CTC2/CFC2.
        let cpu = gte_run(&[lui(1, 0xCAFE), ori(1, 1, 0xF00D), ctc2(1, 5), cfc2(2, 5), NOP]);
        check(&mut pass, "gte TRX round-trip", cpu.regs[2], 0xCAFE_F00D);

        // R33 (c4) is a 16-bit signed matrix corner: 0x8000 reads back sign-extended.
        let cpu = gte_run(&[ori(1, 0, 0x8000), ctc2(1, 4), cfc2(2, 4), NOP]);
        check(&mut pass, "gte R33 sign-extended", cpu.regs[2], 0xFFFF_8000);

        // H (c26): used as unsigned, but the hardware reads it back SIGN-extended (a checked quirk).
        let cpu = gte_run(&[ori(1, 0, 0xFFFF), ctc2(1, 26), cfc2(2, 26), NOP]);
        check(&mut pass, "gte H reads sign-extended", cpu.regs[2], 0xFFFF_FFFF);

        // FLAG (c31): writing an error bit (24 = IR1 saturated) sets the bit-31 summary on read-back.
        let cpu = gte_run(&[lui(1, 0x0100), ctc2(1, 31), cfc2(2, 31), NOP]);
        check(&mut pass, "gte FLAG error sets bit31", cpu.regs[2], 0x8100_0000);

        // FLAG bit-31 is NOT set by a non-summary bit (21 = colour-FIFO-R saturated).
        let cpu = gte_run(&[lui(1, 0x0020), ctc2(1, 31), cfc2(2, 31), NOP]);
        check(&mut pass, "gte FLAG non-summary bit", cpu.regs[2], 0x0020_0000);

        // LWC2/SWC2 move a word between memory and a GTE data register; round-trip through RAM.
        let cpu = gte_run(&[
            ori(1, 0, 0x200), // r1 = scratch address
            lui(2, 0x1234), ori(2, 2, 0x5678), // r2 = 0x12345678
            sw(2, 1, 0), // mem[0x200] = r2
            lwc2(24, 1, 0), // GTE d24 <- mem[0x200]
            swc2(24, 1, 4), // mem[0x204] <- GTE d24
            lw(3, 1, 4), NOP, // r3 <- mem[0x204]
        ]);
        check(&mut pass, "gte LWC2/SWC2 via RAM", cpu.regs[3], 0x1234_5678);
    }

    // --- GTE (COP2) commands (M6.1): the geometry core ---------------------------------
    // RTPS/RTPT + the fixed-point pipeline are validated by the ps1-tests gte/test-all ROM (200 checks
    // pass). These pin the other geometry-core commands (NCLIP, AVSZ, OP, MVMVA) with hand-computable
    // inputs, because test-all breaks on the first un-implemented command (the colour family, M6.2)
    // before reaching them. A GTE command is a COP2 op (opcode 0x12) with the "CO" bit (25) set.
    {
        let gte_run = |prog: &[u32]| -> Cpu {
            let mut cpu = build(prog);
            cpu.cop0.sr |= 1 << 30; // SR.CU2 = "GTE usable"
            run(&mut cpu, prog.len());
            cpu
        };

        // NCLIP: MAC0 = the signed area of the three screen points. SXY0=(1,0), SXY1=(0,1), SXY2=(0,0)
        // -> SX0*SY1 + ... = 1*1 = 1.
        let cpu = gte_run(&[
            ori(1, 0, 1), mtc2(1, 12), // SXY0 = (sx=1, sy=0)
            lui(1, 1), mtc2(1, 13), // SXY1 = (sx=0, sy=1)
            ori(1, 0, 0), mtc2(1, 14), // SXY2 = (0, 0)
            0x4A00_0006, // NCLIP
            mfc2(2, 24), NOP, // r2 <- MAC0
        ]);
        check(&mut pass, "gte NCLIP signed area", cpu.regs[2], 1);

        // AVSZ3: OTZ = ZSF3*(SZ1+SZ2+SZ3) >> 12. ZSF3=0x1000, SZ1=SZ2=SZ3=0x100 -> 0x1000*0x300>>12.
        let cpu = gte_run(&[
            ori(1, 0, 0x100), mtc2(1, 17), mtc2(1, 18), mtc2(1, 19), // SZ1=SZ2=SZ3=0x100
            ori(1, 0, 0x1000), ctc2(1, 29), // ZSF3 = 0x1000
            0x4A00_002D, // AVSZ3
            mfc2(2, 7), NOP, // r2 <- OTZ
        ]);
        check(&mut pass, "gte AVSZ3 average Z", cpu.regs[2], 0x300);

        // OP: cross product of the rotation diagonal [R11,R22,R33] with IR. R*=0x1000, IR=(1,2,3):
        // MAC1 = R22*IR3 - R33*IR2 = 0x1000*3 - 0x1000*2 = 0x1000.
        let cpu = gte_run(&[
            ori(1, 0, 0x1000), ctc2(1, 0), ctc2(1, 2), ctc2(1, 4), // R11=R22=R33=0x1000
            ori(1, 0, 1), mtc2(1, 9), ori(1, 0, 2), mtc2(1, 10), ori(1, 0, 3), mtc2(1, 11), // IR=1,2,3
            0x4A00_000C, // OP (sf=0)
            mfc2(2, 25), NOP, // r2 <- MAC1
        ]);
        check(&mut pass, "gte OP cross product", cpu.regs[2], 0x1000);

        // MVMVA: rotation = identity (R11=R22=R33=0x1000), V0=(5,6,7), cv=none, sf=1:
        // IR1 = (R11*VX0) >> 12 = (0x1000*5) >> 12 = 5.
        let cpu = gte_run(&[
            ori(1, 0, 0x1000), ctc2(1, 0), ctc2(1, 2), ctc2(1, 4), // identity rotation
            lui(2, 0x0006), ori(2, 2, 0x0005), mtc2(2, 0), // VXY0 = (VX0=5, VY0=6)
            ori(2, 0, 7), mtc2(2, 1), // VZ0 = 7
            0x4A08_6012, // MVMVA (sf=1, mx=0=rot, v=0=V0, cv=3=none)
            mfc2(3, 9), NOP, // r3 <- IR1
        ]);
        check(&mut pass, "gte MVMVA identity*V0", cpu.regs[3], 5);
    }

    println!(
        "\n[CPU self-test] {}",
        if pass { "ALL PASSED" } else { "FAILURES ABOVE" }
    );
    pass
}

// ----- a tiny MIPS assembler for the self-test programs -----------------------------------
// Just enough encoders to build the scenarios above. Registers are passed as their numbers;
// the comments at each call site name them.

const NOP: u32 = 0; // SLL r0, r0, 0 — the canonical do-nothing word

/// I-type: opcode | rs | rt | 16-bit immediate.
fn enc_i(op: u32, rs: u32, rt: u32, imm: u32) -> u32 {
    (op << 26) | (rs << 21) | (rt << 16) | (imm & 0xFFFF)
}
/// R-type: opcode 0 | rs | rt | rd | shamt | funct.
fn enc_r(rs: u32, rt: u32, rd: u32, shamt: u32, funct: u32) -> u32 {
    (rs << 21) | (rt << 16) | (rd << 11) | (shamt << 6) | funct
}
/// J-type: opcode | 26-bit word target (the byte target shifted right by 2).
fn enc_j(op: u32, target: u32) -> u32 {
    (op << 26) | ((target >> 2) & 0x03FF_FFFF)
}

fn ori(rt: u32, rs: u32, imm: u32) -> u32 {
    enc_i(0x0D, rs, rt, imm)
}
fn lui(rt: u32, imm: u32) -> u32 {
    enc_i(0x0F, 0, rt, imm)
}
fn lw(rt: u32, rs: u32, imm: u32) -> u32 {
    enc_i(0x23, rs, rt, imm)
}
fn lwl(rt: u32, rs: u32, imm: u32) -> u32 {
    enc_i(0x22, rs, rt, imm)
}
fn lwr(rt: u32, rs: u32, imm: u32) -> u32 {
    enc_i(0x26, rs, rt, imm)
}
fn sw(rt: u32, rs: u32, imm: u32) -> u32 {
    enc_i(0x2B, rs, rt, imm)
}
fn beq(rs: u32, rt: u32, off: u32) -> u32 {
    enc_i(0x04, rs, rt, off)
}
fn add(rd: u32, rs: u32, rt: u32) -> u32 {
    enc_r(rs, rt, rd, 0, 0x20)
}
fn slt(rd: u32, rs: u32, rt: u32) -> u32 {
    enc_r(rs, rt, rd, 0, 0x2A)
}
fn sltu(rd: u32, rs: u32, rt: u32) -> u32 {
    enc_r(rs, rt, rd, 0, 0x2B)
}
fn jal(target: u32) -> u32 {
    enc_j(0x03, target)
}
fn jr(rs: u32) -> u32 {
    enc_r(rs, 0, 0, 0, 0x08)
}

// COP0 (coprocessor 0) moves — primary opcode 0x10, then the `rs` field selects the form. These
// mirror the decode in `cpu.rs` (`0x10 => match rs { ... }`): MTC0 writes a COP0 register from a
// GPR, MFC0 reads one back (load-delayed, like a memory load), and RFE pops the exception stack.
fn mtc0(rt: u32, rd: u32) -> u32 {
    (0x10 << 26) | (0x04 << 21) | (rt << 16) | (rd << 11) // rs = 0x04 = MTC0
}
fn mfc0(rt: u32, rd: u32) -> u32 {
    (0x10 << 26) | (rt << 16) | (rd << 11) // rs = 0x00 = MFC0
}
fn rfe() -> u32 {
    (0x10 << 26) | (0x10 << 21) | 0x10 // CO bit set (rs>=0x10) + funct 0x10 = RFE
}

// COP2 (the GTE) moves — primary opcode 0x12, `rs` selects the form (mirrors the `0x12 => match rs`
// decode in cpu.rs); LWC2/SWC2 are their own primary opcodes. MFC2/CFC2 read the GTE into a GPR
// load-delayed (like MFC0); MTC2/CTC2 write a GTE data/control register from a GPR. `rd` is the GTE
// register, `rt` the GPR.
fn mtc2(rt: u32, rd: u32) -> u32 {
    (0x12 << 26) | (0x04 << 21) | (rt << 16) | (rd << 11) // rs = 0x04 = MTC2 (data)
}
fn mfc2(rt: u32, rd: u32) -> u32 {
    (0x12 << 26) | (rt << 16) | (rd << 11) // rs = 0x00 = MFC2 (data)
}
fn ctc2(rt: u32, rd: u32) -> u32 {
    (0x12 << 26) | (0x06 << 21) | (rt << 16) | (rd << 11) // rs = 0x06 = CTC2 (control)
}
fn cfc2(rt: u32, rd: u32) -> u32 {
    (0x12 << 26) | (0x02 << 21) | (rt << 16) | (rd << 11) // rs = 0x02 = CFC2 (control)
}
fn lwc2(rt: u32, rs: u32, imm: u32) -> u32 {
    enc_i(0x32, rs, rt, imm) // load a word from mem -> GTE data register rt
}
fn swc2(rt: u32, rs: u32, imm: u32) -> u32 {
    enc_i(0x3A, rs, rt, imm) // store GTE data register rt -> mem
}
