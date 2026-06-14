# Liam's Native Emulators

A collection of console emulators written from scratch in Rust. Each emulator is a
standalone Cargo crate. Window, framebuffer, and keyboard input are handled by the
pure-Rust [`minifb`](https://crates.io/crates/minifb) crate, so there is no SDL or other
media library to install.

## Emulators

### CHIP-8 (`chip8/`)

A complete CHIP-8 interpreter: all 35 opcodes, a 64×32 monochrome display, the 60 Hz
delay/sound timers, and the 16-key hex keypad. Runs the standard test ROMs and classic
games.

### Game Boy (`gameboy/`)

A Game Boy (DMG) emulator. Implemented so far: the Sharp LR35902 (SM83) CPU — which passes
Blargg's `cpu_instrs` and `instr_timing` test ROMs — plus the hardware timer, interrupts,
cartridge loading (no-MBC and basic MBC1 banking), and a scanline PPU that renders the
background and window layers. Sprite rendering and keyboard input are in progress.

## Requirements

- [Rust](https://www.rust-lang.org/tools/install) (stable, 2024 edition — Rust 1.85 or
  newer), installed via `rustup`.
- No other setup; `cargo` fetches `minifb` automatically on the first build.

## Building

Each emulator is its own crate, so build from inside its directory:

```
cd chip8        # or: cd gameboy
cargo build --release
```

## Running

### CHIP-8

```
cd chip8
cargo run --release -- "roms/<rom>.ch8"
```

An optional second argument sets the number of CPU instructions per frame (default 10,
≈600 Hz). Press `Esc` to quit.

Keypad mapping:

```
CHIP-8          Keyboard
1 2 3 C         1 2 3 4
4 5 6 D         Q W E R
7 8 9 E         A S D F
A 0 B F         Z X C V
```

### Game Boy

```
cd gameboy
cargo run --release -- roms/<rom>.gb
```

With no second argument this opens a window and runs the ROM. A second argument selects an
alternate mode:

| Argument | Mode |
|----------|------|
| *(none)* | open a window and run the ROM |
| `<N>`    | print a single-step CPU trace for N instructions |
| `run`    | run headless until the ROM reports a result over the serial port |
| `dump`   | run headless, then print an ASCII thumbnail of the rendered frame |

Keyboard input is not yet implemented.

## ROMs

ROMs are **not** included in this repository — the `roms/` directories are git-ignored.
Place your own `.ch8` / `.gb` files in the relevant emulator's `roms/` folder.

## License

MIT — see [LICENSE](LICENSE).
