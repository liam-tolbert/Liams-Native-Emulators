//! Host shell for the Game Boy (DMG) emulator.
//!
//! Everything that isn't the emulated machine lives here — the same role
//! `chip8/src/main.rs` played for CHIP-8. Right now (milestone M0) the shell only
//! loads a ROM and prints its cartridge header. The minifb window, the keyboard
//! mapping, and the ~60 Hz frame loop arrive once there's a CPU and a PPU to drive
//! them (M4/M6).

// Scaffold-in-progress: subsystems are filled in milestone by milestone, so many
// struct fields and methods are defined before their first caller exists. This keeps
// the build warning-free while we grow into the scaffold; remove it near M6 once the
// emulator is feature-complete and every field is actually used.
#![allow(dead_code)]

mod bus;
mod cartridge;
mod cpu;
mod interrupts;
mod joypad;
mod ppu;
mod timer;

use bus::Bus;
use cartridge::Cartridge;
use cpu::Cpu;

fn main() {
    // --- command line: a single ROM path ---
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <path-to-rom.gb>", args[0]);
        std::process::exit(1);
    }
    let rom_path = &args[1];

    // --- load the ROM image off disk ---
    let rom = std::fs::read(rom_path).unwrap_or_else(|e| {
        eprintln!("Failed to read ROM '{rom_path}': {e}");
        std::process::exit(1);
    });

    // Optional 2nd arg: how many instructions to single-step (0 = just load + header).
    let trace_steps: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);

    // --- build the machine: cartridge -> bus -> cpu (the CPU owns the bus) ---
    let cart = Cartridge::new(rom);
    let bus = Bus::new(cart);

    // --- M0: dump what we parsed out of the 0x0100-0x014F header ---
    // (Scoped so the borrow ends before the bus moves into the CPU below.)
    {
        let h = bus.header();
        println!("Loaded: {rom_path}");
        println!("  Title      : {}", h.title);
        println!("  Cart type  : 0x{:02X} ({})", h.cart_type, h.mapper);
        println!("  ROM        : {} banks ({} KiB)", h.rom_banks, h.rom_banks * 16);
        println!("  Cart RAM   : {} bytes", h.ram_bytes);
    }

    // --- M1 single-step harness: `cargo run -- <rom> <N>` traces N instructions ---
    // Each line shows the next opcode and the register file *before* it executes; the
    // CPU panics on the first opcode you haven't implemented yet (telling you which).
    if trace_steps > 0 {
        let mut cpu = Cpu::new(bus);
        println!("\n-- single-step trace ({trace_steps} instructions) --");
        for _ in 0..trace_steps {
            let pc = cpu.pc;
            let op = cpu.bus.read(pc);
            println!(
                "PC:{:04X}  op:{:02X}   AF:{:04X} BC:{:04X} DE:{:04X} HL:{:04X} SP:{:04X}  ime:{}",
                pc,
                op,
                cpu.af(),
                cpu.bc(),
                cpu.de(),
                cpu.hl(),
                cpu.sp,
                cpu.ime as u8
            );
            cpu.step();
        }
    }
}
