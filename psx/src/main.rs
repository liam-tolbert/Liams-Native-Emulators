//! Host shell for the PlayStation 1 (MIPS R3000A) emulator.
//!
//! Everything that isn't the emulated machine lives here — the same role `main.rs` plays in
//! the CHIP-8 and Game Boy crates. It loads a file, reports what it is, builds the machine
//! (bus -> cpu, the CPU owning the bus), then dispatches on an optional 2nd arg into one of
//! the run modes.
//!
//! **This is the M0 scaffold.** The machine builds and the modes are stubbed out with a note
//! pointing at the milestone that fills each one in:
//!   * `<N>`    single-step register trace            -> M1 (CPU core)
//!   * `dump`   headless GPU frame thumbnail          -> M4 (GPU)
//!   * `<exe>`  sideload a PS-EXE and run to a verdict -> M3 (boot + TTY harness)
//!   * (none)   boot the BIOS headless, echoing TTY    -> M3
//!
//! Mode dispatch deliberately mirrors the Game Boy host shell so the two read the same.

// Scaffold: the device modules are only partly wired until M1-M3 fill them in, so a lot is
// "written but not yet called". Mirrors how the DMG crate carried this allow during build-out
// and dropped it once the scaffold was filled. Remove once M1-M3 are real.
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
    if args.len() < 2 {
        let me = &args[0];
        eprintln!("Usage:");
        eprintln!("  {me} <bios.bin>             boot the BIOS headless, echo TTY      [M3]");
        eprintln!("  {me} <bios.bin> <N>         single-step N instructions w/ trace   [M1]");
        eprintln!("  {me} <bios.bin> <game.exe>  sideload a PS-EXE, run to a verdict    [M3]");
        eprintln!("  {me} <bios.bin> dump        headless GPU frame thumbnail          [M4]");
        std::process::exit(1);
    }

    let path = &args[1];
    let bytes = std::fs::read(path).unwrap_or_else(|e| {
        eprintln!("failed to read '{path}': {e}");
        std::process::exit(1);
    });

    // --- M0 check: report what we loaded -------------------------------------------------
    // A real BIOS is exactly 512 KiB; a PS-EXE starts with the "PS-X EXE" magic and carries
    // its entry point / load address in its header. Recognise and summarise both.
    println!("Loaded: {path}  ({} bytes)", bytes.len());
    if let Some(exe) = PsxExe::parse(&bytes) {
        println!(
            "  PS-EXE  pc=0x{:08X}  gp=0x{:08X}  load=0x{:08X}  sp=0x{:08X}  image={} bytes",
            exe.initial_pc, exe.initial_gp, exe.load_addr, exe.initial_sp, exe.data.len()
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
    let cpu = Cpu::new(bus);
    println!(
        "Machine built.  reset PC = 0x{:08X}   BIOS loaded = {}",
        cpu.pc,
        cpu.bus.bios_loaded()
    );

    // --- run-mode dispatch (each arm filled in over M1-M4) -------------------------------
    match args.get(2).map(String::as_str) {
        Some(n) if n.bytes().all(|b| b.is_ascii_digit()) => {
            println!("\n[single-step trace] — arrives with the M1 CPU core.");
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
