# PlayStation 1 (`psx/`)

A from-scratch PlayStation 1 (MIPS R3000A) emulator in Rust, built clean-room from primary hardware
documentation ([Nocash psx-spx](https://problemkaputt.de/psx-spx.htm)) and test ROMs. It boots a real
PS1 BIOS to its on-screen Sony Computer Entertainment logo.

## Building

```
cd psx
cargo build --release
```

Use `--release` — debug builds are far too slow to run the BIOS at a usable speed. Dependencies:
[`minifb`](https://crates.io/crates/minifb) (pure-Rust window/framebuffer, drives the `window` mode)
and [`png`](https://crates.io/crates/png) (test-harness only — decodes the `ps1-tests` reference VRAM
images for pixel-diffs; the emulated machine never touches it).

## Running

```
cd psx
cargo run --release -- <bios.bin> [mode]
```

| Argument | What it does |
|----------|--------------|
| `<bios.bin>` | boot the BIOS headless, echoing the kernel TTY |
| `<bios.bin> window` | open a window and run at ~60 Hz — the BIOS-logo demo (**Esc** quits) |
| `<bios.bin> dump [N]` | run headless for `N` frames (default 120), then dump VRAM → `vram_dump.png` + an ASCII thumbnail |
| `<bios.bin> <game.exe>` | sideload a PS-EXE and run until it prints a pass/fail verdict |
| `<bios.bin> <N>` | single-step `N` instructions with a register trace |
| `selftest` | run the built-in, ROM-free correctness self-test (no BIOS needed) |

The headline demo:

```
cargo run --release -- bios\SCPH1001.bin window
```

## BIOS and ROMs

Neither the BIOS nor any game/test image is included — `psx/bios/` and `psx/roms/` are git-ignored,
like the other emulators' `roms/` folders. Supply your own locally:

- a PS1 BIOS dump (e.g. `SCPH1001.bin`, 512 KiB) in `psx/bios/`,
- PS-EXE programs and the JaCzekanski `ps1-tests` suite in `psx/roms/`.

On Windows/PowerShell, `[ ]` in a path are treated as wildcards — quote paths or use `-LiteralPath`.

## License

MIT — see the repository [LICENSE](../LICENSE).
