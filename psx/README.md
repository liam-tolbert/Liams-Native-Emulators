# PlayStation 1 (`psx/`)

A from-scratch PlayStation 1 (MIPS R3000A) emulator in Rust — the third and most ambitious
emulator in this collection, and the current focus. It is built clean-room from primary
hardware documentation ([Nocash psx-spx](https://problemkaputt.de/psx-spx.htm)) and test ROMs.

> **Status: M2 — memory map, MMIO & exceptions.** On top of the M1 CPU core, the bus is
> complete (segment-masked memory map, the `0x1F801xxx` I/O block, byte/half/word access) and
> the exception/COP0 unit is now exercised end-to-end: misaligned-load/store address errors, the
> full interrupt-delivery chain (a source raises `I_STAT` → masked by `I_MASK` → COP0 `Cause.IP2`
> → gated by `SR.IEc`/`SR.IM` → the `Interrupt` exception → `RFE`), the SR mode/IRQ stack, and
> cache-isolation. All of it is gated by the built-in, ROM-free `selftest`. The BIOS-boot TTY
> harness (M3) is next.

## Roadmap

| Milestone | Scope | State |
|-----------|-------|-------|
| **M0** | Crate scaffold: module layout, run-mode skeleton, reset wiring | **done** |
| **M1** | MIPS R3000A interpreter (branch- & load-delay slots, the full integer set) | **done** |
| **M2** | Memory map + MMIO + exceptions / COP0 | **done** |
| **M3** | BIOS boot + PS-EXE sideload + headless TTY harness → pass the CPU test ROMs | next |
| M4 | GPU: VRAM, GP0/GP1 FIFO, software rasterizer → first rendered frame | later |
| M5+ | GTE, CD-ROM, SPU audio, controllers, then a dynamic recompiler (JIT) | later |

The guiding discipline is *correctness before graphics*: the CPU is validated headlessly by
capturing the BIOS TTY output and running the amidog CPU test ROMs — the same trick the Game
Boy emulator used with Blargg's serial-port tests.

## Building

```
cd psx
cargo build --release
```

Only dependency is [`minifb`](https://crates.io/crates/minifb) (pure-Rust window/framebuffer),
used once the GPU lands in M4. The foundation milestones are all headless.

## Running

```
cd psx
cargo run --release -- <bios.bin> [mode]
```

| Argument | Mode | Milestone |
|----------|------|-----------|
| `<bios.bin>` | boot the BIOS headless, echoing kernel TTY | M3 |
| `<bios.bin> <N>` | single-step `N` instructions with a register trace | M1 |
| `selftest` | run the built-in, ROM-free CPU self-test | M1 |
| `<bios.bin> <game.exe>` | sideload a PS-EXE and run until it prints a pass/fail verdict | M3 |
| `<bios.bin> dump` | run headless, then print an ASCII thumbnail of the frame | M4 |

## BIOS and ROMs

Neither the BIOS nor any game/test image is included — `psx/bios/` and `psx/roms/` are
git-ignored, exactly like the other emulators' `roms/` folders. Supply your own locally:

- a PS1 BIOS dump (e.g. `SCPH1001.bin`, 512 KiB) in `psx/bios/`,
- PS-EXE test programs (e.g. the amidog CPU tests) in `psx/roms/`.

## License

MIT — see the repository [LICENSE](../LICENSE).
