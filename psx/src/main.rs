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
const VRAM_W: usize = 1024;
const VRAM_H: usize = 512;

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
fn vram_to_rgb(vram: &[u16]) -> Vec<u8> {
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
