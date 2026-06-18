# PlayStation 1 (`psx/`)

A from-scratch PlayStation 1 (MIPS R3000A) emulator in Rust — the third and most ambitious
emulator in this collection, and the current focus. It is built clean-room from primary
hardware documentation ([Nocash psx-spx](https://problemkaputt.de/psx-spx.htm)) and test ROMs.

> **Status: M3 — BIOS boot & PS-EXE test harness.** The emulator now runs real software. It boots
> the actual PS1 BIOS headless to the shell hand-off point (`0x80030000`) — the kernel banner comes
> out through a TTY hook that snoops `std_out_putchar` at the BIOS call vectors — and then
> *sideloads* a PS-EXE: it injects the program at the hand-off, sets PC/GP/SP the way the BIOS
> loader would, runs it, and diffs the captured TTY against the test's golden `psx.log`. The
> JaCzekanski `ps1-tests` **`cpu/cop`** test passes (matches its reference log); reaching that also
> fixed a real coprocessor bug — coprocessor ops are now gated on the `SR.CU` "usable" bits instead
> of always faulting. M4 (the GPU) is next.

## Roadmap

| Milestone | Scope | State |
|-----------|-------|-------|
| **M0** | Crate scaffold: module layout, run-mode skeleton, reset wiring | **done** |
| **M1** | MIPS R3000A interpreter (branch- & load-delay slots, the full integer set) | **done** |
| **M2** | Memory map + MMIO + exceptions / COP0 | **done** |
| **M3** | BIOS boot + PS-EXE sideload + headless TTY harness → pass the CPU test ROMs | **done** |
| **M4** | GPU: VRAM, GP0/GP1 FIFO, software rasterizer → first rendered frame | next |
| M5+ | GTE, CD-ROM, SPU audio, controllers, then a dynamic recompiler (JIT) | later |

The guiding discipline is *correctness before graphics*: the CPU is validated headlessly by
capturing the BIOS TTY output and running CPU test ROMs (the JaCzekanski `ps1-tests` suite) — the
same trick the Game Boy emulator used with Blargg's serial-port tests. `cpu/cop` passes today; the
remaining `cpu/` tests are deferred — they probe behaviour modelled loosely on purpose
(cycle-accurate access *timing*, and per-access-width MMIO), or need the GPU/DMA that arrive in M4.

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
