//! Host shell for the CHIP-8 interpreter.
//!
//! Responsibilities (everything that isn't the emulated machine itself):
//!   * parse the ROM path from the command line and load it,
//!   * open a window and translate the 1-bit display into pixels,
//!   * map the PC keyboard onto the CHIP-8 hex keypad,
//!   * drive the machine at a steady ~60 Hz.

mod chip8;

use chip8::{Chip8, HEIGHT, WIDTH};
use minifb::{Key, Scale, Window, WindowOptions};
use std::time::{Duration, Instant};

/// Maps host keyboard keys to the CHIP-8 hex keypad.
///
///   CHIP-8 keypad        Your keyboard
///    1 2 3 C              1 2 3 4
///    4 5 6 D              Q W E R
///    7 8 9 E              A S D F
///    A 0 B F              Z X C V
const KEYMAP: [(Key, usize); 16] = [
    (Key::Key1, 0x1), (Key::Key2, 0x2), (Key::Key3, 0x3), (Key::Key4, 0xC),
    (Key::Q, 0x4),    (Key::W, 0x5),    (Key::E, 0x6),    (Key::R, 0xD),
    (Key::A, 0x7),    (Key::S, 0x8),    (Key::D, 0x9),    (Key::F, 0xE),
    (Key::Z, 0xA),    (Key::X, 0x0),    (Key::C, 0xB),    (Key::V, 0xF),
];

const ON_COLOR: u32 = 0x00FF_FFFF; // white
const OFF_COLOR: u32 = 0x0000_0000; // black

fn main() {
    // --- command line ---
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <path-to-rom.ch8> [cycles-per-frame]", args[0]);
        std::process::exit(1);
    }
    let rom_path = &args[1];
    // ~10 instructions per 60 Hz frame ≈ 600 Hz. Bump it if a game feels sluggish.
    let cycles_per_frame: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(10);

    // --- load ROM into a fresh machine ---
    let rom = std::fs::read(rom_path).unwrap_or_else(|e| {
        eprintln!("Failed to read ROM '{rom_path}': {e}");
        std::process::exit(1);
    });
    let mut chip8 = Chip8::new();
    chip8.load_rom(&rom);

    // --- window: 64x32 logical pixels, scaled up 16x ---
    let mut window = Window::new(
        "CHIP-8  —  [Esc] to quit",
        WIDTH,
        HEIGHT,
        WindowOptions { scale: Scale::X16, ..WindowOptions::default() },
    )
    .expect("failed to create window");

    let mut buffer: Vec<u32> = vec![OFF_COLOR; WIDTH * HEIGHT];
    let frame_time = Duration::from_micros(16_667); // 60 Hz

    while window.is_open() && !window.is_key_down(Key::Escape) {
        let frame_start = Instant::now();

        // 1. sample the keyboard into the emulated keypad
        for (host_key, chip_key) in KEYMAP.iter() {
            chip8.keys[*chip_key] = window.is_key_down(*host_key);
        }

        // 2. run a batch of CPU instructions for this frame
        for _ in 0..cycles_per_frame {
            chip8.cycle();
        }

        // 3. the delay/sound timers tick down at a fixed 60 Hz (once per frame)
        chip8.tick_timers();

        // 4. paint the 1-bit display into the RGB framebuffer
        for (pixel, &on) in buffer.iter_mut().zip(chip8.display.iter()) {
            *pixel = if on { ON_COLOR } else { OFF_COLOR };
        }
        window
            .update_with_buffer(&buffer, WIDTH, HEIGHT)
            .expect("failed to update window");

        // 5. (TODO) beep while chip8.is_beeping() — audio left as a later exercise

        // 6. sleep off the remainder of the frame to hold ~60 Hz
        let elapsed = frame_start.elapsed();
        if elapsed < frame_time {
            std::thread::sleep(frame_time - elapsed);
        }
    }
}
