//! Built-in, ROM-free correctness gate — extracted from `main.rs` to keep the host shell lean.
//!
//! `run_selftest` hand-assembles tiny MIPS programs (the encoders live at the bottom) and small GP0
//! command sequences, runs them on a fresh `Bus`, and checks architecturally-correct results: the CPU
//! milestones (M1/M2), the GPU register + transfer model (M4a/M4b), DMA (M4c), and the rasterizer
//! (M4d-1/2/3). It is this crate's stand-in for a `cargo test` suite; `main` runs it for the
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

    // ===== M2: memory map, MMIO, exceptions & interrupt delivery =======================
    // M1 pulled most of this machinery forward so the CPU could boot the BIOS; M2 is where it
    // gets *exercised and gated*. The headline gap M1 left is interrupt delivery — `Irq::raise`
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
    // 0xBF801074 is I_MASK; a memory-control register round-trips a word; a stubbed timer
    // register reads back 0. (KSEG1 masks down to physical 0x1F80_1xxx — the uncached I/O view.)
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
            ori(8, 8, 0x1100), // r8 = 0xBF80_1100  (timer 0 — stubbed until M4)
            lw(9, 8, 0),       // r9 <- timer       (delayed)
            NOP,               // settle the last load
        ];
        let mut cpu = build(&prog);
        run(&mut cpu, prog.len());
        check(&mut pass, "I_MASK write/read (r3)", cpu.regs[3], 0x0000_000F);
        check(&mut pass, "I_MASK landed in device", cpu.bus.irq.read_mask() as u32, 0x0F);
        check(&mut pass, "mem-control round-trip (r7)", cpu.regs[7], 0x0000_CAFE);
        check(&mut pass, "stubbed timer reads 0 (r9)", cpu.regs[9], 0);
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
    // path `Irq::raise` exists for (the real sources arrive with the timers/GPU in M4).
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

    // ===== M4a: GPU register model + the PNG verify harness ============================
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
        g.gp0(0xDEAD_BEEF); // pixel data word 1 (dropped in M4a)
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

    // ===== M4b: VRAM transfers (A0 / C0 / 02 fill) =====================================
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

    // ===== M4c: DMA (channels 2 GPU + 6 OTC, and the DMA interrupt) ====================
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

    // ===== M4d-1: untextured rasterizer (polygons, rectangles, lines) =================
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

    // ===== M4d-2: texture mapping =====================================================
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

    // ===== M4d-3: semi-transparency ===================================================
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
