//! Host shell for the Game Boy (DMG) emulator.
//!
//! Everything that isn't the emulated machine lives here — the same role
//! `chip8/src/main.rs` played for CHIP-8. It loads a ROM, prints the cartridge
//! header, then dispatches on an optional 2nd arg: no arg opens the minifb window
//! and plays the ROM (the ~60 Hz frame loop plus keyboard -> joypad input); `<N>`
//! single-steps with a register trace; `run` drives the Blargg serial harness;
//! `dump` renders headlessly to an ASCII thumbnail.

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
use minifb::{Key, Scale, Window, WindowOptions};
use std::time::{Duration, Instant};

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

    // Optional 2nd arg selects what to do after loading the ROM:
    //   (none)  -> just print the header
    //   <N>     -> single-step N instructions with a register trace
    //   run     -> free-run until the ROM prints a serial verdict  (Blargg cpu_instrs)
    let mode = args.get(2).map(String::as_str);

    // --- build the machine: cartridge -> bus -> cpu (the CPU owns the bus) ---
    let cart = Cartridge::new(rom);
    let bus = Bus::new(cart);

    // --- dump what we parsed out of the 0x0100-0x014F header ---
    // (Scoped so the borrow ends before the bus moves into the CPU below.)
    {
        let h = bus.header();
        println!("Loaded: {rom_path}");
        println!("  Title      : {}", h.title);
        println!("  Cart type  : 0x{:02X} ({})", h.cart_type, h.mapper);
        println!("  ROM        : {} banks ({} KiB)", h.rom_banks, h.rom_banks * 16);
        println!("  Cart RAM   : {} bytes", h.ram_bytes);
    }

    match mode {
        // --- single-step harness: `cargo run -- <rom> <N>` traces N instructions ---
        // Each line shows the next opcode and the register file *before* it executes; the
        // CPU panics on the first illegal opcode (telling you which + the PC).
        Some(n) if n.bytes().all(|b| b.is_ascii_digit()) => {
            let trace_steps: u32 = n.parse().unwrap_or(0);
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

        // --- Headless test harness: `cargo run --release -- <rom> run` -----------------
        // Blargg's CPU test ROMs write their results to the serial port (the bus prints
        // each character as it arrives). We just run the CPU until that text contains a
        // "Passed" / "Failed" verdict, with a generous instruction cap so a buggy core
        // can't spin forever. This is the whole point of CPU-correctness-before-graphics:
        // we validate the core with no PPU, exactly the trick we'll reuse on the PS1.
        Some("run") => {
            let mut cpu = Cpu::new(bus);
            println!("\n-- running (serial output below) --");
            const MAX_INSTRS: u64 = 300_000_000;
            let mut last_len = 0usize;
            let mut verdict = false;
            for _ in 0..MAX_INSTRS {
                cpu.step();
                // Only scan the (tiny) serial string when it actually grew.
                let out = &cpu.bus.serial_out;
                if out.len() != last_len {
                    last_len = out.len();
                    if out.contains("Passed") || out.contains("Failed") {
                        verdict = true;
                        break;
                    }
                }
            }
            println!();
            if !verdict {
                eprintln!(
                    "(stopped after {MAX_INSTRS} instructions with no Passed/Failed verdict — \
                     the ROM may be stuck; try a single-step trace to see where)"
                );
            }
        }

        // --- Headless PPU check: `cargo run --release -- <rom> dump` --------------------
        // Run a few hundred frames with no window, then report how many framebuffer pixels
        // are non-blank and print a coarse ASCII thumbnail. Confirms the PPU is drawing
        // (and roughly *what*) without needing a display.
        Some("dump") => {
            let mut cpu = Cpu::new(bus);
            let frames = 600u32;
            for _ in 0..frames {
                let mut guard: u32 = 0;
                loop {
                    cpu.step();
                    if cpu.bus.ppu.frame_ready {
                        cpu.bus.ppu.frame_ready = false;
                        break;
                    }
                    guard += 1;
                    if guard > 2_000_000 {
                        break;
                    }
                }
            }
            let fb = &cpu.bus.ppu.framebuffer;
            let nonzero = fb.iter().filter(|&&p| p != 0).count();
            println!("\nafter {frames} frames: {nonzero}/{} framebuffer pixels non-blank", fb.len());
            let glyphs = [' ', '.', '+', '#'];
            for y in (0..ppu::SCREEN_H).step_by(4) {
                let mut line = String::new();
                for x in (0..ppu::SCREEN_W).step_by(2) {
                    line.push(glyphs[(fb[y * ppu::SCREEN_W + x] & 3) as usize]);
                }
                println!("{line}");
            }
        }

        // --- Windowed run (no 2nd arg): open a window and play the ROM ------------------
        // Run the CPU until the PPU signals a finished frame, blit the framebuffer, hold
        // ~59.7 Hz, and sample the keyboard into the joypad once per frame (below). This is
        // the playable path: Tetris boots to its title screen and responds to the controls.
        None => {
            const SHADES: [u32; 4] = [0x00FF_FFFF, 0x00AA_AAAA, 0x0055_5555, 0x0000_0000];
            let mut cpu = Cpu::new(bus);

            let mut window = Window::new(
                "Game Boy  —  [Esc] to quit",
                ppu::SCREEN_W,
                ppu::SCREEN_H,
                WindowOptions { scale: Scale::X4, ..WindowOptions::default() },
            )
            .expect("failed to create window");

            let mut buffer: Vec<u32> = vec![0; ppu::SCREEN_W * ppu::SCREEN_H];
            let frame_time = Duration::from_micros(16_743); // ~59.7 Hz, the real DMG rate

            while window.is_open() && !window.is_key_down(Key::Escape) {
                let frame_start = Instant::now();

                // Run one whole emulated frame: step the CPU until the PPU finishes a
                // frame. The guard stops us wedging the window if a game parks the LCD off.
                let mut guard: u32 = 0;
                loop {
                    cpu.step();
                    if cpu.bus.ppu.frame_ready {
                        cpu.bus.ppu.frame_ready = false;
                        break;
                    }
                    guard += 1;
                    if guard > 2_000_000 {
                        break;
                    }
                }

                // Blit: each palette index (0..=3) -> a gray shade.
                for (px, &shade) in buffer.iter_mut().zip(cpu.bus.ppu.framebuffer.iter()) {
                    *px = SHADES[(shade & 0b11) as usize];
                }
                window
                    .update_with_buffer(&buffer, ppu::SCREEN_W, ppu::SCREEN_H)
                    .expect("failed to update window");

                // --- sample the keyboard into the joypad, once per frame ---
                // Build a BTN_* bitmask (1 = pressed) from whichever host keys are down,
                // then hand it to the bus, which forwards it to the joypad. All the
                // hardware quirks (active-low, the 2x4 matrix, the press interrupt) live
                // inside the joypad — out here we only report which keys are held.
                let mut pressed = 0u8;
                // d-pad -> arrow keys; A=Z, B=X, Select=Backspace, Start=Enter. One OR per
                // held key assembles the BTN_* bitmask the joypad expects.
                if window.is_key_down(Key::Right) {
                    pressed |= joypad::BTN_RIGHT;
                }
                if window.is_key_down(Key::Left) {
                    pressed |= joypad::BTN_LEFT;
                }
                if window.is_key_down(Key::Up) {
                    pressed |= joypad::BTN_UP;
                }
                if window.is_key_down(Key::Down) {
                    pressed |= joypad::BTN_DOWN;
                }
                if window.is_key_down(Key::Z) {
                    pressed |= joypad::BTN_A;
                }
                if window.is_key_down(Key::X) {
                    pressed |= joypad::BTN_B;
                }
                if window.is_key_down(Key::Backspace) {
                    pressed |= joypad::BTN_SELECT;
                }
                if window.is_key_down(Key::Enter) {
                    pressed |= joypad::BTN_START;
                }

                cpu.bus.set_buttons(pressed);

                let elapsed = frame_start.elapsed();
                if elapsed < frame_time {
                    std::thread::sleep(frame_time - elapsed);
                }
            }
        }

        // Any other 2nd arg: unrecognized.
        Some(other) => {
            eprintln!("unknown mode '{other}' — use a number (single-step trace) or 'run' (headless serial)");
        }
    }
}
