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
> of always faulting. M4 (the GPU) is now underway: **M4a** — the VRAM + GP0/GP1 command model + a
> real GPUSTAT, plus the from-scratch PNG verify harness — is done; the VRAM transfers and the
> software rasterizer follow (see the M4 plan below).

## Roadmap

| Milestone | Scope | State |
|-----------|-------|-------|
| **M0** | Crate scaffold: module layout, run-mode skeleton, reset wiring | **done** |
| **M1** | MIPS R3000A interpreter (branch- & load-delay slots, the full integer set) | **done** |
| **M2** | Memory map + MMIO + exceptions / COP0 | **done** |
| **M3** | BIOS boot + PS-EXE sideload + headless TTY harness → pass the CPU test ROMs | **done** |
| **M4** | **GPU → first rendered frame** (built in the stages M4a–M4e below) | in progress |
| ↳ M4a | VRAM + GP0/GP1 command model + real GPUSTAT + the PNG verify harness | **done** |
| ↳ M4b | VRAM transfer & fill commands (`02` / `A0` / `C0` / `80`) | next |
| ↳ M4c | software rasterizer (polygons, rectangles, lines, textures) | later |
| ↳ M4d | DMA channels 2 (GPU) + 6 (OTC) + the DMA interrupt | later |
| ↳ M4e | display timing (VBlank) + the `minifb` window → BIOS-logo demo | later |
| **M4.5** | Root counters / timers (TIMER0/1/2) | later |
| M5+ | GTE, CD-ROM, SPU audio, controllers, then a dynamic recompiler (JIT) | later |

The guiding discipline is *correctness before graphics*: the CPU is validated headlessly by
capturing the BIOS TTY output and running CPU test ROMs (the JaCzekanski `ps1-tests` suite) — the
same trick the Game Boy emulator used with Blargg's serial-port tests. `cpu/cop` passes today; the
remaining `cpu/` tests are deferred — they probe behaviour modelled loosely on purpose
(cycle-accurate access *timing*, and per-access-width MMIO), or need the GPU/DMA that arrive in M4.

### M4 plan (GPU)

M4 is built in five stages, each planned and implemented on its own so the diff stays reviewable:

- **M4a — foundation + verify harness (done).** 1 MiB VRAM, the GP0/GP1 command model (the draw-state
  and display knobs fully modelled; rendering/transfer commands parsed so the command FIFO stays in
  sync), a real `GPUSTAT`, and a from-scratch PNG dump/diff harness. That harness is the graphics
  analog of the serial-port golden-file trick: render into VRAM, then diff pixel-for-pixel against the
  `ps1-tests` `gpu/` reference PNGs.
- **M4b — VRAM transfers & fill** (`02` fill, `A0`/`C0` CPU↔VRAM, `80` VRAM→VRAM).
- **M4c — the software rasterizer** (flat/Gouraud/textured polygons, rectangles/sprites, lines; plus
  semi-transparency, dithering, and the mask bit).
- **M4d — DMA** channels 2 (GPU) and 6 (OTC) — including the linked-list display lists real games
  submit frames through — plus the DMA interrupt.
- **M4e — display timing + window**: the ~60 Hz VBlank tick that drives game loops, and a `minifb`
  window. Demo target: the real BIOS booting to its **Sony Computer Entertainment logo** on screen —
  that logo is GPU-drawn, so a correct M4 renders it with no game or CD-ROM needed.

Verification stays golden-file throughout: pixel-exact VRAM-vs-reference-PNG diffs (plus the existing
TTY harness for any `gpu/` / `dma/` ROM that self-reports). **Out of scope for M4:** the GTE (COP2 —
the 3D transform unit), CD-ROM, SPU audio, and controllers — all M5+. So M4's "rendered frame" means
the BIOS logo, 2D homebrew, and the `gpu/` test ROMs, not full 3D commercial games. The root counters
(**timers**) are independent of getting pixels on screen and land just after, in **M4.5**.

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
