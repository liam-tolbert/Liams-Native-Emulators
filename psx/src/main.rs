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
mod img;
mod irq;
mod selftest;

use bus::Bus;
use cpu::Cpu;
use exe::PsxExe;

const BIOS_BYTES: usize = 512 * 1024;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // `selftest` needs no ROM at all — it builds its own tiny programs in RAM — so handle it
    // before we try to read a file.
    if args.get(1).map(String::as_str) == Some("selftest") {
        let ok = selftest::run_selftest();
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
                selftest::run_selftest();
            }
        }
        Some("dump") => run_dump(&mut cpu),
        Some(other) => {
            run_sideload(&mut cpu, other);
        }
        None => {
            if cpu.bus.bios_loaded() {
                run_bios_boot(&mut cpu);
            } else {
                eprintln!("\n(no BIOS loaded — supply a 512 KiB BIOS image to boot)");
                std::process::exit(1);
            }
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

// ===== BIOS boot / PS-EXE sideload harness (M3) =========================================
//
// A real PS1 boots like this: the CPU starts executing the BIOS ROM at 0xBFC00000; the BIOS sets
// up the kernel (exception handlers, the A0/B0/C0 function tables, a stack, the controllers...),
// shows the splash screen, then reads the game's boot executable off the CD into RAM and jumps to
// it at the fixed address 0x80030000.
//
// We don't emulate the CD-ROM yet, so we "sideload": let the *real* BIOS boot all the way to that
// 0x80030000 hand-off, then — in place of code read from a disc — drop a PS-EXE we already have on
// the host into RAM and jump into it ourselves. Booting the real BIOS first (rather than faking a
// minimal environment) is what gives the program everything it expects: the kernel function tables
// it will call for I/O, and a valid, kernel-initialized stack pointer. So 0x80030000 does double
// duty here — it is both the "BIOS finished booting" success marker and the exact instant we inject.

/// The address the BIOS jumps to when it hands control to the disc's boot executable — and so the
/// point where we inject instead. (psx-spx: this is where the shell execs the loaded PSX.EXE.)
const EXEC_POINT: u32 = 0x8003_0000;

/// Generous instruction budget for the real BIOS boot (it runs into the millions before the
/// hand-off). If a BIOS needs more, bump this; if the boot *stalls*, the budget is what turns an
/// infinite wait into a "pause and report".
const BOOT_BUDGET: u64 = 50_000_000;

/// Step the CPU until PC reaches `target`, or until `budget` instructions have run. Returns the
/// instruction count on success, or `None` if the budget ran out first. The check is at the top of
/// the loop, so on success the CPU is poised *at* `target` (it hasn't executed it yet) — which is
/// exactly the moment the sideloader wants, to overwrite PC with the EXE's entry point.
fn run_until_pc(cpu: &mut Cpu, target: u32, budget: u64) -> Option<u64> {
    for i in 0..budget {
        if cpu.pc == target {
            return Some(i);
        }
        cpu.step();
    }
    None
}

/// Headless BIOS boot: run the real BIOS with TTY capture on until it reaches the executable
/// hand-off point. Reaching it is the boot-success signal — many BIOS revisions print little or
/// nothing over the kernel TTY during boot (it's mostly used by games), so "no TTY" is not failure.
fn run_bios_boot(cpu: &mut Cpu) {
    cpu.capture_tty = true;
    println!("\n[headless BIOS boot] running until exec point 0x{EXEC_POINT:08X} ...\n");
    match run_until_pc(cpu, EXEC_POINT, BOOT_BUDGET) {
        Some(n) => {
            println!("\n\n[BIOS] reached 0x{EXEC_POINT:08X} after {n} instructions — boot OK.");
            println!("[BIOS] the kernel is ready to hand control to a program.");
        }
        None => {
            // The agreed contingency: if the boot stalls, stop and report where, rather than
            // sinking time into boot debugging unprompted.
            eprintln!(
                "\n\n[BIOS] did NOT reach 0x{EXEC_POINT:08X} within {BOOT_BUDGET} instructions \
                 — it stalled. Last CPU state:"
            );
            dump_regs(cpu);
            std::process::exit(1);
        }
    }
}

// ===== GPU frame dump / VRAM verify harness (M4) ========================================
//
// The graphics analog of the serial/Blargg golden-file trick: snapshot VRAM and either eyeball it
// (ASCII thumbnail), save it (PNG, via the from-scratch codec in `img.rs`), or diff it pixel-for-
// pixel against a reference PNG. M4a stands this up and calibrates it; from M4b on it gates the
// rasterizer against the `ps1-tests` reference images.

/// VRAM is a fixed 1024x512 grid of 16-bit pixels (matches `gpu.rs`).
pub(crate) const VRAM_W: usize = 1024;
pub(crate) const VRAM_H: usize = 512;

/// `dump` run-mode: boot the BIOS (so anything it draws is in VRAM), then snapshot VRAM to a PNG and
/// print an ASCII thumbnail. In M4a nothing is rasterized yet — a black frame here is expected and
/// correct; the pipeline (VRAM -> RGB -> PNG) is what's being exercised.
fn run_dump(cpu: &mut Cpu) {
    if cpu.bus.bios_loaded() {
        cpu.capture_tty = true;
        println!("\n[dump] booting BIOS to 0x{EXEC_POINT:08X} before snapshotting VRAM ...\n");
        let _ = run_until_pc(cpu, EXEC_POINT, BOOT_BUDGET);
    } else {
        println!("\n[dump] no BIOS loaded — dumping the (empty) power-on VRAM.");
    }

    let rgb = vram_to_rgb(cpu.bus.gpu.vram());
    print_vram_ascii(&rgb);

    let png = img::encode_rgb(VRAM_W as u32, VRAM_H as u32, &rgb);
    let path = "vram_dump.png";
    match std::fs::write(path, &png) {
        Ok(_) => println!("\n[dump] wrote {path}  ({VRAM_W}x{VRAM_H} VRAM, {} bytes)", png.len()),
        Err(e) => eprintln!("\n[dump] failed to write {path}: {e}"),
    }
    println!("[dump] (VRAM stays black until the rasterizer lands in M4d.)");
}

/// Expand the full 1024x512 VRAM into a packed 24-bit RGB buffer (`VRAM_W*VRAM_H*3` bytes). This is
/// the **single place** the 15->24-bit colour expansion is defined, so the dump, the PNG, and the
/// diff all agree.
pub(crate) fn vram_to_rgb(vram: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(vram.len() * 3);
    for &px in vram {
        // **Gotcha: 1-5-5-5 little-endian, with RED in the low bits** (mask:1 | B:5 | G:5 | R:5).
        // Reading them in screen order R,G,B means pulling the *low* field first — getting this
        // backwards paints everything blue-for-red, the classic first GPU bug.
        let r5 = (px & 0x1F) as u8;
        let g5 = ((px >> 5) & 0x1F) as u8;
        let b5 = ((px >> 10) & 0x1F) as u8;
        out.push(expand5(r5));
        out.push(expand5(g5));
        out.push(expand5(b5));
    }
    out
}

/// Expand a 5-bit channel to 8 bits. **Calibrated against the `ps1-tests` references (M4b):** the
/// suite top-aligns (`v << 3`), so 0x1F maps to 0xF8 and no channel ever reaches 0xFF — confirmed
/// empirically (a reference scan finds no 255). This must match the suite's generator exactly or the
/// pixel-diff is off by the low bits everywhere; bit-replication (`(v<<3)|(v>>2)`) was the wrong guess.
fn expand5(v: u8) -> u8 {
    v << 3
}

/// Print a coarse ASCII thumbnail of VRAM — the same quick eyeball the Game Boy `dump` mode gives.
/// Down-samples to 64x24 cells, mapping each sampled pixel's brightness onto a 10-step ramp.
fn print_vram_ascii(rgb: &[u8]) {
    const COLS: usize = 64;
    const ROWS: usize = 24;
    const RAMP: &[u8] = b" .:-=+*#%@";
    println!("\n[VRAM thumbnail {COLS}x{ROWS} of {VRAM_W}x{VRAM_H}]");
    for row in 0..ROWS {
        let mut line = String::with_capacity(COLS);
        for col in 0..COLS {
            // Sample one pixel near the centre of this cell.
            let x = col * VRAM_W / COLS;
            let y = row * VRAM_H / ROWS;
            let i = (y * VRAM_W + x) * 3;
            let lum = (rgb[i] as u32 + rgb[i + 1] as u32 + rgb[i + 2] as u32) / 3;
            let idx = (lum as usize * (RAMP.len() - 1)) / 255;
            line.push(RAMP[idx] as char);
        }
        println!("    {line}");
    }
}

/// Diff a VRAM snapshot against a reference PNG, pixel-exact. Returns true on a perfect match; on a
/// mismatch it prints how far off we are, including a hint that distinguishes a real rendering bug
/// from a mere colour-expansion miscalibration (the M4b "is `expand5` right?" question — if *every*
/// channel is off by <= 7, it's the 5->8 expansion, not the pixels).
fn diff_vram_vs_png(vram: &[u16], ref_path: &str) -> bool {
    let bytes = match std::fs::read(ref_path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[diff] cannot read {ref_path}: {e}");
            return false;
        }
    };
    let (w, h, ref_rgb) = match img::decode_rgb(&bytes) {
        Some(t) => t,
        None => {
            eprintln!("[diff] {ref_path} isn't an 8-bit RGB PNG the harness can read");
            return false;
        }
    };
    if (w as usize, h as usize) != (VRAM_W, VRAM_H) {
        eprintln!("[diff] size mismatch: ref {w}x{h} vs VRAM {VRAM_W}x{VRAM_H}");
        return false;
    }

    let rgb = vram_to_rgb(vram);
    if rgb == ref_rgb {
        return true;
    }

    // Mismatch — quantify it and hint at the cause.
    let mut diff_px = 0usize;
    let mut max_delta = 0u8;
    let mut ref_has_255 = false;
    for (a, b) in rgb.chunks_exact(3).zip(ref_rgb.chunks_exact(3)) {
        if a != b {
            diff_px += 1;
        }
        for k in 0..3 {
            max_delta = max_delta.max(a[k].abs_diff(b[k]));
            ref_has_255 |= b[k] == 0xFF;
        }
    }
    eprintln!(
        "[diff] {diff_px} / {} pixels differ; max channel delta {max_delta}; reference reaches 255: {ref_has_255}",
        VRAM_W * VRAM_H
    );
    if max_delta <= 7 {
        eprintln!(
            "[diff] all deltas <= 7 — this is a 5->8 colour-expansion mismatch, not a rendering bug \
             (adjust `expand5`)."
        );
    }
    false
}

// ----- PS-EXE sideload ------------------------------------------------------------------

/// A return address planted in `ra` before launching a sideloaded EXE. If the program returns
/// (`jr ra`) instead of looping, PC lands here and we stop cleanly — reaching null isn't something
/// a healthy test does mid-run, so it doubles as an "exited / jumped to null" signal.
const SENTINEL_RA: u32 = 0x0000_0000;

/// Post-injection instruction budget. A CPU test is short; this just bounds the run if a program
/// neither prints its end-marker nor returns.
const RUN_BUDGET: u64 = 50_000_000;

/// Sideload a PS-EXE: boot the BIOS to the hand-off point (so the kernel tables and a valid stack
/// exist), inject the image and set the registers the BIOS loader would, run with TTY capture until
/// the program ends, then diff the captured TTY against the sibling `psx.log` golden log.
fn run_sideload(cpu: &mut Cpu, exe_path: &str) {
    if !cpu.bus.bios_loaded() {
        eprintln!("\n(PS-EXE sideload needs a BIOS as the first argument — a 512 KiB image)");
        std::process::exit(1);
    }
    let bytes = std::fs::read(exe_path).unwrap_or_else(|e| {
        eprintln!("failed to read '{exe_path}': {e}");
        std::process::exit(1);
    });
    let exe = PsxExe::parse(&bytes).unwrap_or_else(|| {
        eprintln!("'{exe_path}' is not a PS-EXE (bad magic / too short)");
        std::process::exit(1);
    });

    // 1. Boot the real BIOS to the hand-off, so the A0/B0/C0 tables AND a usable SP exist (these
    //    test EXEs ship SP=0 on purpose and rely on the BIOS-established stack).
    cpu.capture_tty = true;
    println!("\n[sideload] booting BIOS to 0x{EXEC_POINT:08X} before injecting '{exe_path}' ...\n");
    if run_until_pc(cpu, EXEC_POINT, BOOT_BUDGET).is_none() {
        eprintln!("\n[sideload] BIOS stalled before 0x{EXEC_POINT:08X}; cannot inject.");
        dump_regs(cpu);
        std::process::exit(1);
    }

    // 2. Inject: copy the program image into RAM and hand-set the registers to the state the BIOS
    //    loader leaves behind, so the EXE starts exactly as if the BIOS had loaded it off a disc.
    //    Each value comes from the PS-EXE header (parsed in exe.rs):
    //      * pc        — the entry point. We also prime next_pc/current_pc because the CPU core runs
    //                    a two-PC model for branch-delay slots (next_pc is always "one instruction
    //                    past pc"); leaving them stale would mis-handle the very first branch.
    //      * r28 (gp)  — the "global pointer", a base register compiled C uses to reach its globals.
    //      * r29 (sp), r30 (fp) — stack and frame pointers. IMPORTANT gotcha: these test EXEs carry
    //                    an SP of 0 in their header, which by the format means "leave SP alone, use
    //                    the one the kernel set up." If we wrote 0 into SP, the program's first stack
    //                    push would hit address 0 and trample the kernel — so when the header SP is 0
    //                    we deliberately keep the BIOS's value (this is exactly why we boot the real
    //                    BIOS first, in step 1).
    //      * r4 (a0), r5 (a1) — the argc/argv-style pair the BIOS hands a freshly-loaded program.
    //      * r31 (ra)  — the return address. We plant a recognisable sentinel so that if the program
    //                    returns (`jr ra`) instead of looping forever, PC lands there and we can stop.
    cpu.bus.store_ram(exe.load_addr, &exe.data);
    cpu.pc = exe.initial_pc;
    cpu.next_pc = exe.initial_pc.wrapping_add(4);
    cpu.current_pc = exe.initial_pc;
    cpu.regs[28] = exe.initial_gp;
    if exe.initial_sp != 0 {
        cpu.regs[29] = exe.initial_sp;
        cpu.regs[30] = exe.initial_sp;
    }
    cpu.regs[4] = 1;
    cpu.regs[5] = 0;
    cpu.regs[31] = SENTINEL_RA;

    let tty_start = cpu.bus.tty_out.len(); // everything printed from here on is the EXE's
    println!(
        "[sideload] injected {} bytes at 0x{:08X} — entry 0x{:08X}, gp 0x{:08X}, sp 0x{:08X}\n",
        exe.data.len(),
        exe.load_addr,
        exe.initial_pc,
        exe.initial_gp,
        cpu.regs[29]
    );

    // 3. Run until the program prints its end-marker ("Done." in the ps1-tests), returns to the
    //    sentinel, or the budget runs out. Only scan for the marker when TTY actually grew.
    let mut last_len = tty_start;
    for _ in 0..RUN_BUDGET {
        if cpu.pc == SENTINEL_RA {
            break;
        }
        cpu.step();
        if cpu.bus.tty_out.len() != last_len {
            last_len = cpu.bus.tty_out.len();
            if cpu.bus.tty_out.trim_end().ends_with("Done.") {
                break;
            }
        }
    }

    // 4. Verdict. A `gpu/` test grades one of two ways: most print a pass/fail line to the kernel TTY
    //    (checked against the sibling `psx.log`), but the pure-render tests instead ship a reference
    //    VRAM image — for those we diff the framebuffer pixel-for-pixel. Use whichever reference is
    //    present next to the EXE.
    let captured = cpu.bus.tty_out[tty_start..].to_string();
    let vram_ref = std::path::Path::new(exe_path).with_file_name("vram.png");
    if vram_ref.exists() {
        if !captured.trim().is_empty() {
            println!("\n----- captured TTY -----\n{captured}\n------------------------");
        }
        if diff_vram_vs_png(cpu.bus.gpu.vram(), &vram_ref.to_string_lossy()) {
            println!("\n[verdict] VRAM MATCHES {} (pixel-exact)", vram_ref.display());
        } else {
            println!("\n[verdict] VRAM DIFFERS from {}", vram_ref.display());
            // Dump what we actually produced so the failure is inspectable (and useful for the M4d
            // rasterizer stage that makes these render tests pass for real).
            let rgb = vram_to_rgb(cpu.bus.gpu.vram());
            print_vram_ascii(&rgb);
            let _ = std::fs::write("vram_dump.png", img::encode_rgb(VRAM_W as u32, VRAM_H as u32, &rgb));
            println!("[verdict] wrote our VRAM to vram_dump.png for comparison");
            std::process::exit(1);
        }
    } else {
        report_verdict(exe_path, &captured);
    }
}

/// Compare a program's captured TTY against the golden `psx.log` shipped alongside it in the
/// ps1-tests suite. Exits non-zero on a mismatch (or a self-reported failure) so it can be scripted.
fn report_verdict(exe_path: &str, captured: &str) {
    println!("\n----- captured TTY -----\n{captured}\n------------------------");

    let golden_path = std::path::Path::new(exe_path).with_file_name("psx.log");
    match std::fs::read_to_string(&golden_path) {
        Ok(golden) => {
            let want = strip_markers(normalize(&golden));
            let got = normalize(captured);
            // The golden log holds only the *test's own* output, but our capture can carry BIOS boot
            // chatter ahead of it (e.g. the ResetGraph debug this BIOS revision prints), so we line
            // up the tail of the capture with the golden rather than demanding a head-to-toe match.
            let tail: &[String] = if got.len() >= want.len() {
                &got[got.len() - want.len()..]
            } else {
                &got
            };
            if !want.is_empty() && tail == want {
                println!("\n[verdict] MATCH — output matches {} (tail-aligned)", golden_path.display());
            } else {
                println!("\n[verdict] DIFFERS from {}", golden_path.display());
                print_diff(&want, tail);
                std::process::exit(1);
            }
        }
        Err(_) => {
            println!("\n[verdict] no sibling psx.log to compare against — captured output only.");
            if captured.to_lowercase().contains("fail") {
                std::process::exit(1);
            }
        }
    }
}

/// Break text into trimmed lines, dropping CRs and trailing blank lines, so a comparison doesn't
/// hinge on cosmetic whitespace / line-ending differences.
fn normalize(s: &str) -> Vec<String> {
    let mut lines: Vec<String> = s
        .replace('\r', "")
        .lines()
        .map(|l| l.trim_end().to_string())
        .collect();
    while lines.last().map(|l| l.is_empty()).unwrap_or(false) {
        lines.pop();
    }
    lines
}

/// The golden `psx.log` files prefix every line with "% " (the ps1-tests capture convention). Strip
/// it so the reference lines line up with our raw TTY, which carries no such marker.
fn strip_markers(lines: Vec<String>) -> Vec<String> {
    lines
        .into_iter()
        .map(|l| l.strip_prefix("% ").map(str::to_string).unwrap_or(l))
        .collect()
}

/// Print the first handful of line-level differences between the golden log and what we captured.
fn print_diff(want: &[String], got: &[String]) {
    let mut shown = 0;
    for i in 0..want.len().max(got.len()) {
        let w = want.get(i).map(String::as_str).unwrap_or("<none>");
        let g = got.get(i).map(String::as_str).unwrap_or("<none>");
        if w != g {
            println!("  line {:>3}: expected `{w}`  |  got `{g}`", i + 1);
            shown += 1;
            if shown >= 10 {
                println!("  ... (further differences omitted)");
                break;
            }
        }
    }
}

