# Game Boy / DMG (`gameboy/`)

A from-scratch Game Boy (original DMG) emulator in Rust — playable, screenshot-able Tetris. CPU + PPU
(background / window / sprites) + timer + joypad + interrupts; passes Blargg's `cpu_instrs` and
`instr_timing` and the `dmg-acid2` PPU test.

## Building

```
cd gameboy
cargo build --release
```

Use `--release` for playable speed. Only dependency:
[`minifb`](https://crates.io/crates/minifb) (pure-Rust window/framebuffer/keyboard).

## Running

```
cd gameboy
cargo run --release -- <rom.gb> [mode]
```

| Mode | What it does |
|------|--------------|
| *(none)* | open a window and play |
| `<N>` | single-step `N` instructions, printing a register trace |
| `run` | free-run headless until the ROM prints a `Passed`/`Failed` verdict (Blargg serial tests) |
| `dump` | run headless, then print an ASCII thumbnail of the frame (`dmg-acid2`) |

Examples:

```
cargo run --release -- roms\Tetris.gb
cargo run --release -- roms\cpu_instrs.gb run
cargo run --release -- roms\dmg-acid2.gb dump
```

## Controls (window mode)

| Key | Button |
|-----|--------|
| Arrow keys | D-pad |
| `Z` | A |
| `X` | B |
| `Enter` | Start |
| `Backspace` | Select |
| `Esc` | quit |

## ROMs

ROMs aren't included — `gameboy/roms/` is git-ignored. Drop `.gb` files there (e.g. `Tetris.gb`,
`cpu_instrs.gb`, `dmg-acid2.gb`) and pass the path.

## License

MIT — see the repository [LICENSE](../LICENSE).
