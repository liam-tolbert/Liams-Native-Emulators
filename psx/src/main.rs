//! Host shell for the PlayStation 1 (MIPS R3000A) emulator.
//!
//! Everything that isn't the emulated machine lives here — the same role `main.rs` plays in
//! the CHIP-8 and Game Boy crates. It loads a file, reports what it is, builds the machine
//! (bus -> cpu, the CPU owning the bus), then dispatches on an optional 2nd arg into one of
//! the run modes.
//!
//! **With M1 the CPU core is real.** The single-step trace mode now actually runs the
//! interpreter, and there's a `selftest` mode that drives a handful of hand-assembled MIPS
//! programs through the CPU and checks the results — a built-in, ROM-free correctness gate for
//! the trickiest parts of the chip (the load- and branch-delay slots, overflow trapping, the
//! unaligned LWL/LWR pair). The remaining modes still point at the milestone that fills them:
//!   * `<N>`      single-step register trace            -> M1 (this milestone)
//!   * `selftest` run the built-in CPU self-test        -> M1 (this milestone)
//!   * `dump`     headless GPU frame thumbnail          -> M4 (GPU)
//!   * `<exe>`    sideload a PS-EXE and run to a verdict -> M3 (boot + TTY harness)
//!   * (none)     boot the BIOS headless, echoing TTY    -> M3
//!
//! Mode dispatch deliberately mirrors the Game Boy host shell so the two read the same.

// Scaffold: some device modules are still only partly wired until M3-M4 fill them in (the TTY
// hook, the IRQ sources, most of the GPU/DMA), so a few items are "written but not yet called".
// Mirrors how the DMG crate carried this allow during build-out and dropped it once everything
// was reachable.
#![allow(dead_code)]

mod bus;
mod cop0;
mod cpu;
mod dma;
mod exe;
mod gpu;
mod irq;

use bus::Bus;
use cpu::Cpu;
use exe::PsxExe;

const BIOS_BYTES: usize = 512 * 1024;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // `selftest` needs no ROM at all — it builds its own tiny programs in RAM — so handle it
    // before we try to read a file.
    if args.get(1).map(String::as_str) == Some("selftest") {
        let ok = run_selftest();
        std::process::exit(if ok { 0 } else { 1 });
    }

    if args.len() < 2 {
        let me = &args[0];
        eprintln!("Usage:");
        eprintln!("  {me} <bios.bin>             boot the BIOS headless, echo TTY      [M3]");
        eprintln!("  {me} <bios.bin> <N>         single-step N instructions w/ trace   [M1]");
        eprintln!("  {me} selftest              run the built-in CPU self-test        [M1]");
        eprintln!("  {me} <bios.bin> <game.exe>  sideload a PS-EXE, run to a verdict    [M3]");
        eprintln!("  {me} <bios.bin> dump        headless GPU frame thumbnail          [M4]");
        std::process::exit(1);
    }

    let path = &args[1];
    let bytes = std::fs::read(path).unwrap_or_else(|e| {
        eprintln!("failed to read '{path}': {e}");
        std::process::exit(1);
    });

    // --- report what we loaded -----------------------------------------------------------
    // A real BIOS is exactly 512 KiB; a PS-EXE starts with the "PS-X EXE" magic and carries
    // its entry point / load address in its header. Recognise and summarise both.
    println!("Loaded: {path}  ({} bytes)", bytes.len());
    if let Some(exe) = PsxExe::parse(&bytes) {
        println!(
            "  PS-EXE  pc=0x{:08X}  gp=0x{:08X}  load=0x{:08X}  sp=0x{:08X}  image={} bytes",
            exe.initial_pc,
            exe.initial_gp,
            exe.load_addr,
            exe.initial_sp,
            exe.data.len()
        );
    } else if bytes.len() == BIOS_BYTES {
        println!("  Looks like a 512 KiB BIOS image.");
    } else {
        println!("  (unrecognised image — neither a 512 KiB BIOS nor a PS-EXE)");
    }

    // --- build the machine: bus -> cpu (the CPU owns the bus), exactly like the DMG -------
    let mut bus = Bus::new();
    if bytes.len() == BIOS_BYTES {
        bus.load_bios(bytes);
    }
    let mut cpu = Cpu::new(bus);
    println!(
        "Machine built.  reset PC = 0x{:08X}   BIOS loaded = {}",
        cpu.pc,
        cpu.bus.bios_loaded()
    );

    // --- run-mode dispatch ---------------------------------------------------------------
    match args.get(2).map(String::as_str) {
        Some(n) if n.bytes().all(|b| b.is_ascii_digit()) => {
            let steps: u64 = n.parse().unwrap_or(0);
            if cpu.bus.bios_loaded() {
                run_trace(&mut cpu, steps);
            } else {
                // No BIOS to single-step, so fall back to the ROM-free self-test instead of
                // tracing a machine whose reset vector reads back as 0xFFFFFFFF.
                println!("\n(no BIOS loaded — running the built-in self-test instead)\n");
                run_selftest();
            }
        }
        Some("dump") => {
            println!("\n[GPU frame thumbnail] — arrives with the M4 GPU.");
        }
        Some(other) => {
            println!("\n[PS-EXE sideload + TTY harness for '{other}'] — arrives in M3.");
        }
        None => {
            println!("\n[headless BIOS boot] — arrives in M3.");
        }
    }
}

// ===== single-step trace ================================================================

/// The conventional MIPS assembler names for the 32 general-purpose registers, used only to
/// make the trace human-readable.
const REG_NAMES: [&str; 32] = [
    "zero", "at", "v0", "v1", "a0", "a1", "a2", "a3", "t0", "t1", "t2", "t3", "t4", "t5", "t6",
    "t7", "s0", "s1", "s2", "s3", "s4", "s5", "s6", "s7", "t8", "t9", "k0", "k1", "gp", "sp", "fp",
    "ra",
];

/// Single-step `steps` instructions, printing the PC + raw instruction word about to run and
/// the resulting register file after each one. The format is deliberately plain so it can be
/// diffed line-by-line against a known-good reference log (e.g. a no$psx trace).
fn run_trace(cpu: &mut Cpu, steps: u64) {
    println!(
        "\n[single-step trace]  {steps} instructions from PC = 0x{:08X}\n",
        cpu.pc
    );
    for i in 0..steps {
        // The address we're about to execute, and the word sitting there.
        let pc = cpu.pc;
        let instr = cpu.bus.read32(pc);
        cpu.step();
        println!("#{i:<6} pc=0x{pc:08X}  instr=0x{instr:08X}");
        dump_regs(cpu);
    }
}

/// Dump the register file as four rows of eight, plus the multiply/divide and PC registers.
fn dump_regs(cpu: &Cpu) {
    for row in 0..4 {
        let mut line = String::from("    ");
        for col in 0..8 {
            let i = row * 8 + col;
            line.push_str(&format!("{}=0x{:08X} ", REG_NAMES[i], cpu.regs[i]));
        }
        println!("{line}");
    }
    println!(
        "    hi=0x{:08X} lo=0x{:08X}  next_pc=0x{:08X}\n",
        cpu.hi, cpu.lo, cpu.next_pc
    );
}

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

fn run_selftest() -> bool {
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
