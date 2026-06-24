//! Host shell for the PlayStation 1 (MIPS R3000A) emulator.
//!
//! Everything that isn't the emulated machine lives here — the same role `main.rs` plays in
//! the CHIP-8 and Game Boy crates. It loads a file, reports what it is, builds the machine
//! (bus -> cpu, the CPU owning the bus), then dispatches on an optional 2nd arg into one of
//! the run modes.
//!
//! The single-step trace mode runs the interpreter, and there's a `selftest` mode that drives a
//! handful of hand-assembled MIPS programs through the CPU and checks the results — a built-in,
//! ROM-free correctness gate for the trickiest parts of the chip (the load- and branch-delay
//! slots, overflow trapping, the unaligned LWL/LWR pair). The run modes are:
//!   * `<N>`      single-step register trace
//!   * `selftest` run the built-in CPU self-test
//!   * `window`   open a window and run (BIOS-logo demo)
//!   * `dump [N]` headless: run N frames -> VRAM PNG
//!   * `<exe>`    sideload a PS-EXE and run to a verdict
//!   * (none)     boot the BIOS headless, echoing TTY
//!
//! Mode dispatch deliberately mirrors the Game Boy host shell so the two read the same.

// Scaffold: some device modules are still only partly wired (the TTY
// hook, the IRQ sources, most of the GPU/DMA), so a few items are "written but not yet called".
// Mirrors how the DMG crate carried this allow during build-out and dropped it once everything
// was reachable.
#![allow(dead_code)]

mod bus;
mod cdrom;
mod cop0;
mod cpu;
mod dma;
mod exe;
mod gpu;
mod img;
mod irq;
mod iso;
mod selftest;
mod timer;

use bus::Bus;
use cdrom::CdImage;
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
        eprintln!("  {me} <bios.bin>             boot the BIOS headless, echo TTY");
        eprintln!("  {me} <bios.bin> window      open a window, run (BIOS-logo demo)");
        eprintln!("  {me} <bios.bin> <N>         single-step N instructions w/ trace");
        eprintln!("  {me} selftest              run the built-in CPU self-test");
        eprintln!("  {me} <bios.bin> <game.exe>  sideload a PS-EXE, run to a verdict");
        eprintln!("  {me} <bios.bin> dump [N]    headless: run N frames -> VRAM PNG");
        eprintln!("  {me} <bios.bin> disc <.cue>         boot a real disc, headless, report the stall");
        eprintln!("  {me} <bios.bin> disc <.cue> window  boot a real disc in a window (watch it render)");
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
        Some("window") => run_window(&mut cpu),
        Some("dump") => {
            // Optional frame count (default 120): how many VBlank-driven frames to run after the
            // hand-off so the BIOS's boot animation actually paints before we snapshot.
            let frames = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(120);
            run_dump(&mut cpu, frames);
        }
        Some("disc") => {
            // `disc <path>` runs headless to the stall report; `disc <path> window` opens a window.
            let path = args.get(3).map(String::as_str);
            if args.get(4).map(String::as_str) == Some("window") {
                run_disc_window(&mut cpu, path);
            } else {
                run_disc(&mut cpu, path);
            }
        }
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

// ===== BIOS boot / PS-EXE sideload harness =========================================
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

// ===== GPU frame dump / VRAM verify harness ========================================
//
// The graphics analog of the serial/Blargg golden-file trick: snapshot VRAM and either eyeball it
// (ASCII thumbnail), save it (PNG, via the from-scratch codec in `img.rs`), or diff it pixel-for-
// pixel against a reference PNG. It gates the rasterizer against the `ps1-tests` reference images.

/// VRAM is a fixed 1024x512 grid of 16-bit pixels (matches `gpu.rs`).
pub(crate) const VRAM_W: usize = 1024;
pub(crate) const VRAM_H: usize = 512;

/// `dump` run-mode: boot the BIOS, run `frames` VBlank-driven frames so its boot animation actually
/// paints (the Sony logo is drawn *after* the hand-off, one step per VBlank), then snapshot VRAM to a
/// PNG + ASCII thumbnail. The window-free way to capture the rendered frame for a screenshot.
fn run_dump(cpu: &mut Cpu, frames: u32) {
    if cpu.bus.bios_loaded() {
        cpu.capture_tty = true;
        println!("\n[dump] booting BIOS to 0x{EXEC_POINT:08X} before snapshotting VRAM ...\n");
        let _ = run_until_pc(cpu, EXEC_POINT, BOOT_BUDGET);
        if frames > 0 {
            println!("\n[dump] running {frames} frames so the VBlank-driven boot animation advances ...");
            run_frames(cpu, frames);
        }
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

    // Also write just the visible frame — what's actually on screen, cropped out of the larger VRAM
    // (which holds the off-screen work buffers too). This is the clean screenshot.
    let (w, h, frame) = cpu.bus.gpu.display_frame();
    let mut screen_rgb = Vec::with_capacity(w * h * 3);
    for px in &frame {
        screen_rgb.extend_from_slice(&[(px >> 16) as u8, (px >> 8) as u8, *px as u8]);
    }
    let screen = img::encode_rgb(w as u32, h as u32, &screen_rgb);
    match std::fs::write("screen.png", &screen) {
        Ok(_) => println!("[dump] wrote screen.png  ({w}x{h} visible frame, {} bytes)", screen.len()),
        Err(e) => eprintln!("[dump] failed to write screen.png: {e}"),
    }
}

/// Step the CPU until `n` whole frames have elapsed — each frame ends when the GPU trips VBlank
/// (`take_frame`). A per-frame step guard keeps a wedged machine from hanging the run.
fn run_frames(cpu: &mut Cpu, n: u32) {
    for _ in 0..n {
        let mut guard = 0u32;
        while !cpu.bus.gpu.take_frame() {
            cpu.step();
            guard += 1;
            if guard > 5_000_000 {
                return;
            }
        }
    }
}

/// `window` run-mode: open a real window and run the machine at ~60 Hz, blitting the GPU's
/// visible framebuffer each frame. The demo is the BIOS booting to its **Sony Computer Entertainment
/// logo** on screen — GPU-drawn, animated by the VBlank loop, no game or CD-ROM needed. Mirrors the
/// Game Boy host shell's window loop. No controller input yet (the PS1 pads come later); Esc / closing
/// the window quits.
fn run_window(cpu: &mut Cpu) {
    use minifb::{Key, Scale, ScaleMode, Window, WindowOptions};
    use std::time::{Duration, Instant};

    if !cpu.bus.bios_loaded() {
        eprintln!("\n(no BIOS loaded — supply a 512 KiB BIOS image to open a window)");
        std::process::exit(1);
    }
    cpu.capture_tty = true; // keep echoing the boot TTY to the terminal

    // One fixed 640x480 canvas (the largest PS1 mode). minifb letterboxes each frame's live buffer
    // into it via AspectRatioStretch, so the BIOS starting blanked and switching resolution mid-run
    // needs no window recreation, and a 256x240 splash keeps its aspect instead of smearing.
    let mut window = Window::new(
        "PlayStation 1  —  [Esc] to quit",
        640,
        480,
        WindowOptions {
            scale: Scale::X1,
            scale_mode: ScaleMode::AspectRatioStretch,
            ..WindowOptions::default()
        },
    )
    .expect("failed to create window");

    let frame_time = Duration::from_micros(16_666); // ~60 Hz, paced against the wall clock

    while window.is_open() && !window.is_key_down(Key::Escape) {
        let frame_start = Instant::now();

        // Run one emulated frame: step until the GPU trips VBlank. The guard stops a wedged machine
        // from freezing the window (normally it breaks the instant a frame completes).
        let mut guard = 0u32;
        while !cpu.bus.gpu.take_frame() {
            cpu.step();
            guard += 1;
            if guard > 5_000_000 {
                break;
            }
        }

        let (w, h, buf) = cpu.bus.gpu.display_frame();
        window
            .update_with_buffer(&buf, w, h)
            .expect("failed to update window");

        let elapsed = frame_start.elapsed();
        if elapsed < frame_time {
            std::thread::sleep(frame_time - elapsed);
        }
    }
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

/// Expand a 5-bit channel to 8 bits. **Calibrated against the `ps1-tests` references:** the
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
/// from a mere colour-expansion miscalibration (the "is `expand5` right?" question — if *every*
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

/// Inject a parsed PS-EXE into the booted machine and hand-set the registers to the state the BIOS
/// loader leaves behind, so the program starts exactly as if the BIOS had loaded it off a disc.
/// Shared by the PS-EXE sideload and the real-disc (`disc`) boot — both reach the same `0x80030000`
/// hand-off and differ only in *where the EXE came from*. Each value comes from the PS-EXE header
/// (parsed in exe.rs):
///   * pc        — the entry point. We also prime next_pc/current_pc because the CPU core runs a
///                 two-PC model for branch-delay slots (next_pc is always "one instruction past pc");
///                 leaving them stale would mis-handle the very first branch.
///   * r28 (gp)  — the "global pointer", a base register compiled C uses to reach its globals.
///   * r29 (sp), r30 (fp) — stack and frame pointers. IMPORTANT gotcha: a header SP of 0 means "leave
///                 SP alone, use the one the kernel set up." Writing 0 would send the first stack push
///                 to address 0 and trample the kernel — so when the header SP is 0 we keep the BIOS's
///                 value (exactly why both paths boot the real BIOS first).
///   * r4 (a0), r5 (a1) — the argc/argv-style pair the BIOS hands a freshly-loaded program.
///   * r31 (ra)  — a recognisable sentinel return address, so a program that returns (`jr ra`)
///                 instead of looping lands somewhere we can detect rather than running off into noise.
fn inject_exe(cpu: &mut Cpu, exe: &PsxExe) {
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
}

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

    // 2. Inject the image and set the registers exactly as the BIOS loader would, so the EXE starts
    //    as if the BIOS had loaded it off a disc. Shared with the real-disc boot path (`inject_exe`).
    inject_exe(cpu, &exe);

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
            // Dump what we actually produced so the failure is inspectable (and useful for the
            // rasterizer that makes these render tests pass for real).
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

// ===== real-disc boot (HLE) ============================================================
//
// The genuine version of what the sideloader fakes: instead of taking a PS-EXE off the host
// filesystem, we attach the game's CD image and read its boot executable straight off the disc, then
// inject it at the same 0x80030000 hand-off. The CD-ROM *drive* (`cdrom.rs`) is real and the disc
// stays inserted, so the running game can still stream its assets through it; only the disc
// *filesystem* walk — finding SYSTEM.CNF and the SLUS executable — is done host-side here (`iso.rs`).
// That's "HLE" (high-level emulation) boot: skip the BIOS shell's slow disc-scan, but run the real
// game code. Pure-LLE (letting the BIOS shell boot the disc itself) is a later authenticity stretch.

/// `disc` run-mode: attach a CD image, boot the BIOS to the hand-off, HLE-load the game's boot EXE off
/// the disc, inject it, and run until it stalls — reporting where (the signal for the next milestone).
fn run_disc(cpu: &mut Cpu, path_arg: Option<&str>) {
    boot_disc(cpu, path_arg);
    // Run the game's own code until it stalls, and report where (the M6-selecting signal).
    let report = run_until_stall(cpu);
    report.print(cpu);
}

/// The disc-boot preamble shared by the headless `disc` mode and the windowed `disc … window` mode:
/// require a BIOS, resolve & attach the `.cue`, boot the real BIOS to the exec hand-off (so the kernel
/// tables + a valid stack exist), HLE-load the game's boot EXE off the disc's ISO9660 filesystem, and
/// inject it. On any failure it prints the reason and exits — the same contract `run_disc` had inline.
/// On return the CPU is poised at the game's entry point, the disc still inserted for asset streaming;
/// the two modes differ only in what they do next (report a stall vs. present frames).
fn boot_disc(cpu: &mut Cpu, path_arg: Option<&str>) {
    if !cpu.bus.bios_loaded() {
        eprintln!("\n(disc boot needs a BIOS as the first argument — a 512 KiB image)");
        std::process::exit(1);
    }
    let path = path_arg.unwrap_or_else(|| {
        eprintln!("\n(usage: <bios.bin> disc <path-to-.cue or game dir> [window])");
        std::process::exit(1);
    });

    // 1. Resolve the .cue (accept a .cue directly, or a directory holding exactly one) and attach it.
    let cue = resolve_cue(path).unwrap_or_else(|| {
        eprintln!("[disc] no .cue found at '{path}'");
        std::process::exit(1);
    });
    let img = CdImage::from_cue(&cue).unwrap_or_else(|e| {
        eprintln!("[disc] failed to open the disc image for '{}': {e}", cue.display());
        std::process::exit(1);
    });
    println!(
        "\n[disc] attached {} ({} sectors)",
        cue.display(),
        img.sector_count()
    );
    cpu.bus.cdrom.load_disc(img);

    // 2. Boot the real BIOS to the exec hand-off, so the kernel tables + a valid stack exist.
    cpu.capture_tty = true;
    println!("[disc] booting BIOS to 0x{EXEC_POINT:08X} ...\n");
    if run_until_pc(cpu, EXEC_POINT, BOOT_BUDGET).is_none() {
        eprintln!("\n[disc] BIOS stalled before 0x{EXEC_POINT:08X}; cannot boot the disc.");
        dump_regs(cpu);
        std::process::exit(1);
    }

    // 3. HLE-load the boot executable off the disc's ISO9660 filesystem (exits with a reason on miss).
    let exe = match load_boot_exe(cpu) {
        Some(e) => e,
        None => std::process::exit(1),
    };

    // 4. Inject exactly as the BIOS loader would (shared with the sideloader).
    inject_exe(cpu, &exe);
    println!(
        "\n[disc] injected {} bytes at 0x{:08X} — entry 0x{:08X}, gp 0x{:08X}, sp 0x{:08X}\n",
        exe.data.len(),
        exe.load_addr,
        exe.initial_pc,
        exe.initial_gp,
        cpu.regs[29]
    );
}

/// `disc … window` run-mode: boot the disc exactly like `run_disc`, but instead of running headless to
/// a stall, open a window and present the game's frames at ~60 Hz so we can *watch* the boot render
/// before it parks. Crucially `bus.tick` advances the GPU's video clock every instruction, so VBlank
/// keeps firing — and the window keeps refreshing — even after the game settles into its poll-wait; it
/// simply shows the last frame the game drew. The full "what is it polling" diagnosis is the headless
/// `disc` mode; here we only flag *when* it parks. View-only — PS1 controllers are a later milestone;
/// Esc or closing the window quits. (Mirrors `run_window`, with the disc preamble in front.)
fn run_disc_window(cpu: &mut Cpu, path_arg: Option<&str>) {
    use minifb::{Key, Scale, ScaleMode, Window, WindowOptions};
    use std::time::{Duration, Instant};

    boot_disc(cpu, path_arg);
    println!("[disc] opening a window — watch the boot render ([Esc] quits) ...\n");

    let mut window = Window::new(
        "PlayStation 1  —  disc  —  [Esc] to quit",
        640,
        480,
        WindowOptions {
            scale: Scale::X1,
            scale_mode: ScaleMode::AspectRatioStretch,
            ..WindowOptions::default()
        },
    )
    .expect("failed to create window");

    let frame_time = Duration::from_micros(16_666); // ~60 Hz, paced against the wall clock

    // "It parked" watch — the same forward-progress idea the headless plateau watchdog uses (a tight
    // poll loop cycles over several PCs, so a single-PC test would miss it): once the furthest RAM PC
    // and the TTY both stop advancing for a long stretch, the game has reached its poll-wait. Note it
    // once; the window stays live showing the last frame.
    const PARK_LIMIT: u64 = 8_000_000;
    let mut max_ram_pc = 0u32;
    let mut plateau = 0u64;
    let mut tty_len = cpu.bus.tty_out.len();
    let mut noted = false;

    while window.is_open() && !window.is_key_down(Key::Escape) {
        let frame_start = Instant::now();

        // Run one emulated frame: step until the GPU trips VBlank. The guard stops a wedged machine
        // from freezing the window (normally it breaks the instant a frame completes).
        let mut guard = 0u32;
        while !cpu.bus.gpu.take_frame() {
            cpu.step();

            if !noted {
                let pc = cpu.pc;
                let in_ram = (0x8001_0000..0x8020_0000).contains(&pc);
                let tty_now = cpu.bus.tty_out.len();
                if (in_ram && pc > max_ram_pc) || tty_now != tty_len {
                    if in_ram && pc > max_ram_pc {
                        max_ram_pc = pc;
                    }
                    tty_len = tty_now;
                    plateau = 0;
                } else {
                    plateau += 1;
                    if plateau > PARK_LIMIT {
                        noted = true;
                        println!(
                            "\n[disc] poll-wait reached (furthest RAM pc 0x{max_ram_pc:08X}); the window \
                             stays open showing the last frame. Run the headless `disc` mode for the full \
                             diagnosis of what it's waiting on.\n"
                        );
                    }
                }
            }

            guard += 1;
            if guard > 5_000_000 {
                break;
            }
        }

        let (w, h, buf) = cpu.bus.gpu.display_frame();
        window
            .update_with_buffer(&buf, w, h)
            .expect("failed to update window");

        let elapsed = frame_start.elapsed();
        if elapsed < frame_time {
            std::thread::sleep(frame_time - elapsed);
        }
    }
}

/// Walk the attached disc's ISO9660 filesystem to find SYSTEM.CNF, read the `BOOT=` executable name,
/// load that executable off the disc, and parse it as a PS-EXE. Returns `None` (after printing the
/// reason) on any missing piece. Borrows the disc immutably — it stays inserted for the drive.
fn load_boot_exe(cpu: &Cpu) -> Option<PsxExe> {
    let disc = cpu.bus.cdrom.disc_ref()?;
    let iso = iso::IsoReader::open(disc).or_else(|| {
        eprintln!("[disc] not an ISO9660 volume (no CD001 at sector 16)");
        None
    })?;
    let (cnf_lba, cnf_len) = iso.find_in_root("SYSTEM.CNF").or_else(|| {
        eprintln!("[disc] SYSTEM.CNF not found in the disc root");
        None
    })?;
    let cnf = String::from_utf8_lossy(&iso.read_file(cnf_lba, cnf_len)).into_owned();
    println!("[disc] SYSTEM.CNF:\n{}", cnf.trim_end());
    let boot = iso::parse_boot_filename(&cnf).or_else(|| {
        eprintln!("[disc] no BOOT= line in SYSTEM.CNF");
        None
    })?;
    let (exe_lba, exe_len) = iso.find_in_root(&boot).or_else(|| {
        eprintln!("[disc] boot executable '{boot}' not found in the disc root");
        None
    })?;
    println!("[disc] BOOT = {boot}  (lba {exe_lba}, {exe_len} bytes)");
    let raw = iso.read_file(exe_lba, exe_len);
    let magic = String::from_utf8_lossy(&raw[..raw.len().min(8)]).into_owned();
    let exe = PsxExe::parse(&raw).or_else(|| {
        eprintln!("[disc] '{boot}' off the disc isn't a PS-EXE (magic: {magic:?})");
        None
    })?;
    println!(
        "[disc] parsed PS-EXE off the disc — magic {magic:?}, pc 0x{:08X}, gp 0x{:08X}, load 0x{:08X}",
        exe.initial_pc, exe.initial_gp, exe.load_addr
    );
    Some(exe)
}

/// Resolve the disc argument to a `.cue`: a `.cue` path directly, or a directory holding exactly one.
fn resolve_cue(path: &str) -> Option<std::path::PathBuf> {
    let p = std::path::Path::new(path);
    if p.is_file() && p.extension().is_some_and(|e| e.eq_ignore_ascii_case("cue")) {
        return Some(p.to_path_buf());
    }
    if p.is_dir() {
        let mut found = None;
        for entry in std::fs::read_dir(p).ok()?.flatten() {
            let q = entry.path();
            if q.extension().is_some_and(|e| e.eq_ignore_ascii_case("cue")) {
                if found.is_some() {
                    return None; // more than one .cue — can't choose
                }
                found = Some(q);
            }
        }
        return found;
    }
    None
}

/// The result of sampling a detected poll-wait with the MMIO-read tap armed: which register(s) the
/// loop spins on. `top` is the hottest I/O offset (`phys - 0x1F801000`), or `None` when the loop read
/// no device port at all in the sample window (i.e. it polls a RAM/kernel variable — a missed IRQ).
struct PollDiag {
    summary: String,
    top: Option<u32>,
}

/// Why the post-boot run stopped — the signal that picks the next milestone. `pc`/`instr` name the
/// instruction the report is about (the faulting one for a fault loop, not the handler). `poll` is
/// present only for the poll-wait stops, carrying the diagnosis of what the loop is waiting on.
struct StallReport {
    pc: u32,
    instr: u32,
    reason: String,
    steps: u64,
    poll: Option<PollDiag>,
}

impl StallReport {
    fn print(&self, cpu: &Cpu) {
        println!(
            "\n[disc] game code ran {} instructions, then stalled.",
            self.steps
        );
        println!(
            "[disc] STALL @ pc=0x{:08X}  instr=0x{:08X}  — {}",
            self.pc, self.instr, self.reason
        );

        // For a poll-wait, the headline of this stage: *what* is it waiting on, plus the interrupt
        // state that tells "polling a stubbed device" apart from "waiting on an IRQ that never comes".
        if let Some(diag) = &self.poll {
            println!("[disc] poll target: {}", diag.summary);
            let stat = cpu.bus.irq.read_stat();
            let mask = cpu.bus.irq.read_mask();
            let sr = cpu.cop0.sr;
            let cause = cpu.cop0.cause;
            println!(
                "[disc] IRQ state: I_STAT=0x{stat:04X} I_MASK=0x{mask:04X} enabled&pending=0x{:04X}  \
                 SR=0x{sr:08X} (IEc={}, IM=0x{:02X})  CAUSE=0x{cause:08X} (IP=0x{:02X})",
                stat & mask,
                sr & 1,
                (sr >> 8) & 0xFF,
                (cause >> 8) & 0xFF,
            );
            println!("[disc] suggested next: {}", poll_verdict(diag.top));
        }

        let tty = cpu.bus.tty_out.trim_end();
        if !tty.is_empty() {
            let lines: Vec<&str> = tty.lines().collect();
            let start = lines.len().saturating_sub(8);
            println!("----- last TTY -----\n{}\n--------------------", lines[start..].join("\n"));
        }
        dump_regs(cpu);
    }
}

/// Once a poll-wait is detected, run a short window with the MMIO-read tap armed and tally which I/O
/// register(s) the loop reads. The hottest is what it is spinning on. This is the concrete, recorded
/// signal that orders the next milestone — it turns "stuck somewhere" into "polling 0x1F801DAE".
fn diagnose_poll(cpu: &mut Cpu) -> PollDiag {
    // Generous so even a poll loop that calls deep into the BIOS each pass (few iterations per 100k
    // instrs) still tallies its target many times; a 1M-instruction diagnostic sample is cheap.
    const SAMPLE: u64 = 1_000_000;
    *cpu.bus.io_trace.borrow_mut() = Some(std::collections::HashMap::new());
    for _ in 0..SAMPLE {
        cpu.step();
    }
    let hist = cpu.bus.io_trace.borrow_mut().take().unwrap_or_default();

    let mut entries: Vec<(u32, u64)> = hist.into_iter().collect();
    entries.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    if entries.is_empty() {
        return PollDiag {
            summary: format!(
                "no I/O register read in {SAMPLE} sampled instructions — the loop polls a RAM/kernel \
                 variable, not a device port (it is waiting for an interrupt handler to update it)"
            ),
            top: None,
        };
    }
    let parts: Vec<String> = entries
        .iter()
        .take(3)
        .map(|(off, n)| format!("0x{:08X} ({}) x{n}", 0x1F80_1000 + off, Bus::io_name(*off)))
        .collect();
    PollDiag {
        summary: format!("over {SAMPLE} sampled instructions the loop polls {}", parts.join(", ")),
        top: entries.first().map(|(off, _)| *off),
    }
}

/// Turn the hottest polled register into a one-line "build this next" recommendation. Diagnose-only —
/// this is text, not a fix. The arms cover the realistic boot walls; the key distinction is between a
/// *stubbed* device (implement it) and an *implemented* one whose behaviour doesn't yet match what the
/// game waits for (reconcile the timing/semantics — not a new subsystem).
fn poll_verdict(top: Option<u32>) -> &'static str {
    match top {
        // No device read at all: the loop spins on a RAM word a BIOS interrupt handler should bump.
        None => "the loop waits on a memory variable, not a device — check the IRQ state above: if \
                 the expected source (VBlank bit 0 / CD bit 2) is masked in I_MASK/SR.IM or SR.IEc=0, \
                 this is an interrupt-delivery gap to close; otherwise trace which BIOS event it set up",
        // Root counters (Timer0/1/2) — IMPLEMENTED, so this is a frame-pacing/timer mismatch, not a
        // missing device. The PS1's common software VSync waits on Timer1 (hblank/vblank-synced).
        Some(o) if (0x100..=0x12F).contains(&o) => {
            "frame-timing wait on a root counter (Timer0/1/2) — those ARE implemented, so this is not a \
             missing device: the game paces on timer counts/targets that our coarse 2-cycles-per- \
             instruction model produces differently than hardware. THE M6 DRIVER: reconcile the polled \
             timer's mode/target + its vblank/hblank-sync cadence (and GPUSTAT, also polled) with the \
             game's frame-sync — this is GPU/timer timing, not GTE and not the SPU"
        }
        // GPU status — a vblank / draw-ready wait; the bits are assembled live from our video clock.
        Some(0x810) | Some(0x814) => {
            "GPU-status / VSync wait (GPUREAD/GPUSTAT) — GPUSTAT is implemented; verify its timing bits \
             (vblank, even/odd field, DMA/cmd-ready) toggle under our video clock as the game expects"
        }
        // The SPU register block — audio-init readiness.
        Some(o) if (0xC00..=0xFFF).contains(&o) => {
            "SPU init poll — pull a minimal SPU status/control stub forward (part of M8) so the \
             readiness bit the loop waits on reads as ready"
        }
        // I_STAT/I_MASK — it's interrupt-driven; confirm the source is enabled and actually raised.
        Some(0x070) | Some(0x074) => {
            "interrupt-driven wait on I_STAT — confirm the awaited source is unmasked (I_MASK & SR.IM) \
             and is being raised; if so this is an IRQ-delivery gap, not a missing device"
        }
        // The CD-ROM controller ports — a response/sector INT isn't arriving.
        Some(o) if (0x800..=0x803).contains(&o) => {
            "CD-event wait — the loop polls the CD-ROM controller; the next response/sector interrupt \
             isn't arriving (check the ack-gated event queue)"
        }
        Some(_) => "polls a device port — inspect that register's status/ready semantics (and whether \
                    the game is also waiting on an interrupt that isn't arriving)",
    }
}

/// Name a COP0 exception cause code (the low part the `disc` stall report cares about).
fn exc_name(code: u32) -> &'static str {
    match code {
        0x00 => "interrupt",
        0x04 => "address error (load)",
        0x05 => "address error (store)",
        0x06 => "bus error (instr fetch)",
        0x07 => "bus error (data)",
        0x08 => "syscall",
        0x09 => "breakpoint",
        0x0A => "reserved/illegal instruction",
        0x0B => "coprocessor unusable",
        0x0C => "arithmetic overflow",
        _ => "other",
    }
}

/// Step the injected game until it hits a *real* wall, classifying why.
///
/// The load-bearing discipline (learned the hard way): **a machine still taking interrupts is alive,
/// not hung.** MvC's boot spends a ~960-frame timed delay (hundreds of millions of instructions) parked
/// in a tiny loop whose only forward progress is a frame counter the VBlank handler bumps — there is no
/// new code and no TTY for that whole stretch, yet it is perfectly healthy. So we do NOT treat a code/TTY
/// plateau as a stall on its own; we only flag a machine that is *frozen* — no new code, no TTY, AND no
/// interrupt taken for a long stretch (interrupts off, or genuinely wedged). The two hard walls — an
/// un-emulated GTE/COP2 op, and a fault that repeats on the same EPC — are caught immediately regardless,
/// since those are the real "we don't emulate this yet" signals. Exceptions are otherwise NORMAL (every
/// BIOS syscall transiently lands at 0x80000080 before the handler returns via RFE).
fn run_until_stall(cpu: &mut Cpu) -> StallReport {
    // Generous: must outlast realistic multi-second timed waits (≈282k instrs/frame, so ~960 frames is
    // ~270M instrs) so we run *past* them to the real wall rather than mistaking the wait for a stall.
    const DISC_BUDGET: u64 = 600_000_000;
    const FAULT_LIMIT: u64 = 2_048; // the same instruction faulting this many times == stuck
    // No new code, no new TTY, AND no interrupt taken for this long == frozen (a live machine waiting on
    // VBlank trips its interrupt every ~282k instrs, far under this, so a healthy wait never frozes out).
    const FROZEN_LIMIT: u64 = 4_000_000;
    let mut fault_epc = u32::MAX;
    let mut fault_reps = 0u64;
    let mut max_ram_pc = 0u32;
    let mut frozen = 0u64;
    let mut tty_len = cpu.bus.tty_out.len();
    let mut last_irq = cpu.irq_taken;
    for i in 0..DISC_BUDGET {
        let pc = cpu.pc;

        // STOP 1: null sentinel (the injected program returned / jumped to 0).
        if pc == SENTINEL_RA {
            return StallReport {
                pc,
                instr: 0,
                reason: "returned to the null sentinel (program exited / jumped to 0)".into(),
                steps: i,
                poll: None,
            };
        }

        // STOP 2: an un-emulated GTE/COP2 op — the real next wall; caught before it runs so we name it.
        let instr = cpu.bus.read32(pc);
        let gte = match instr >> 26 {
            0x12 => Some("un-emulated COP2/GTE instruction — selects the GTE (COP2) milestone"),
            0x32 => Some("un-emulated LWC2 (GTE load) — selects the GTE (COP2) milestone"),
            0x3A => Some("un-emulated SWC2 (GTE store) — selects the GTE (COP2) milestone"),
            _ => None,
        };
        if let Some(reason) = gte {
            return StallReport { pc, instr, reason: reason.into(), steps: i, poll: None };
        }

        // Liveness: new RAM code, fresh TTY, OR an interrupt taken since the last step all count as
        // "alive" and reset the frozen counter. Because a waiting-but-healthy machine keeps taking
        // VBlank interrupts, a long timed wait stays alive here and never trips the frozen stall — only
        // a wedged machine (interrupts off / no progress at all) accumulates to the limit.
        let in_ram_code = (0x8001_0000..0x8020_0000).contains(&pc);
        let tty_now = cpu.bus.tty_out.len();
        let alive = (in_ram_code && pc > max_ram_pc) || tty_now != tty_len || cpu.irq_taken != last_irq;
        if alive {
            if in_ram_code && pc > max_ram_pc {
                max_ram_pc = pc;
            }
            tty_len = tty_now;
            last_irq = cpu.irq_taken;
            frozen = 0;
        } else {
            frozen += 1;
            if frozen > FROZEN_LIMIT {
                let poll = diagnose_poll(cpu); // tap the MMIO reads to name what the loop spins on
                return StallReport {
                    pc,
                    instr,
                    reason: format!(
                        "frozen: no new code, TTY, or interrupt for >{FROZEN_LIMIT} instrs (furthest reached 0x{max_ram_pc:08X}) — a hard hang (interrupts disabled, or wedged waiting on a device that never fires)"
                    ),
                    steps: i,
                    poll: Some(poll),
                };
            }
        }

        cpu.step();

        // STOP 4: a fault that keeps re-firing on the same EPC is unrecoverable: the handler returns and
        // the game immediately re-faults on something we don't handle. Report the faulting instruction.
        if cpu.pc == 0x8000_0080 {
            let epc = cpu.cop0.epc;
            if epc == fault_epc {
                fault_reps += 1;
            } else {
                fault_epc = epc;
                fault_reps = 1;
            }
            if fault_reps > FAULT_LIMIT {
                let code = (cpu.cop0.cause >> 2) & 0x1F;
                return StallReport {
                    pc: epc,
                    instr: cpu.bus.read32(epc),
                    reason: format!(
                        "repeated {} (ExcCode 0x{code:02X}) faulting at this EPC — the game hit something we don't handle",
                        exc_name(code)
                    ),
                    steps: i,
                    poll: None,
                };
            }
        }
    }
    // Budget exhausted while still alive: most likely an extremely long wait, or a wait on an event we
    // never deliver. Diagnose what it polls so we can tell which.
    let poll = diagnose_poll(cpu);
    StallReport {
        pc: cpu.pc,
        instr: cpu.bus.read32(cpu.pc),
        reason: format!(
            "ran the full {DISC_BUDGET}-instruction budget without a hard wall (furthest reached 0x{max_ram_pc:08X}) — still alive; an extremely long wait or an event we never deliver"
        ),
        steps: DISC_BUDGET,
        poll: Some(poll),
    }
}

