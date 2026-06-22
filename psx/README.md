# PlayStation 1 (`psx/`)

A from-scratch PlayStation 1 (MIPS R3000A) emulator in Rust — the third and most ambitious
emulator in this collection, and the current focus. It is built clean-room from primary
hardware documentation ([Nocash psx-spx](https://problemkaputt.de/psx-spx.htm)) and test ROMs.

> **Status: M4 — GPU.** The emulator boots the actual PS1 BIOS headless to the shell hand-off point
> (`0x80030000`) — the kernel banner comes out through a TTY hook that snoops `std_out_putchar` at the
> BIOS call vectors — and *sideloads* a PS-EXE: it injects the program at the hand-off, sets PC/GP/SP
> the way the BIOS loader would, runs it, and diffs the captured TTY against the test's golden
> `psx.log` (the JaCzekanski `ps1-tests` **`cpu/cop`** test passes). The GPU is now well underway:
> **M4a** (VRAM + GP0/GP1 model + GPUSTAT + the PNG verify harness), **M4b** (VRAM transfer & fill
> commands), **M4c** (DMA channels 2 + 6 + the DMA interrupt — `dma/otc-test` passes), and the **full
> rasterizer (M4d)** — flat/Gouraud/textured polygons, rectangles/sprites, lines, and the four
> semi-transparency blend modes — are done. `gpu/clipping`, `quad`, `texture-overflow`, and
> `texture-flip` render **pixel-exact**, and `gpu/gp0-e1` passes 10/10. **Next is M4e** — display
> timing (VBlank) and a `minifb` window, whose demo target is the **BIOS Sony logo on screen** (see
> the M4 plan below).

## Roadmap

| Milestone | Scope | State |
|-----------|-------|-------|
| **M0** | Crate scaffold: module layout, run-mode skeleton, reset wiring | **done** |
| **M1** | MIPS R3000A interpreter (branch- & load-delay slots, the full integer set) | **done** |
| **M2** | Memory map + MMIO + exceptions / COP0 | **done** |
| **M3** | BIOS boot + PS-EXE sideload + headless TTY harness → pass the CPU test ROMs | **done** |
| **M4** | **GPU → first rendered frame** (built in the stages M4a–M4e below) | in progress |
| ↳ M4a | VRAM + GP0/GP1 command model + real GPUSTAT + the PNG verify harness | **done** |
| ↳ M4b | VRAM transfer & fill commands (`02` / `A0` / `C0` / `80`) | **done** |
| ↳ M4c | DMA channels 2 (GPU) + 6 (OTC) + the DMA interrupt | **done** |
| ↳ M4d | software rasterizer (polygons, rectangles, lines, textures, semi-transparency) | **done** |
| ↳ M4e | display timing (VBlank) + the `minifb` window → BIOS-logo demo | next |
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
- **M4b — VRAM transfers & fill (done).** `02` fill, `A0`/`C0` CPU↔VRAM, `80` VRAM→VRAM. Validated by
  the self-test's direct-GP0 round-trips; the suite's `gpu/` reference ROMs feed the GPU through DMA,
  so they reference-validate once M4c lands. (The PNG reader is now the `png` crate, so the harness
  reads the suite's compressed references; the 5→8-bit colour expansion is calibrated to the suite's
  top-aligned form.)
- **M4c — DMA (done).** Channels 2 (GPU) and 6 (OTC) — including the linked-list display lists real
  games *and the test ROMs* submit frames through — plus the DMA interrupt. This is the keystone:
  every `gpu/` ROM drives the GPU via DMA, so it's what makes the M4b transfers (and then the M4d
  rasterizer) validate against the reference images. (Moved ahead of the rasterizer for exactly this
  reason — discovered during M4b that the ROMs never touch the GP0 port directly.)
- **M4d — the software rasterizer (done)** (flat/Gouraud/textured polygons, rectangles/sprites, lines;
  plus semi-transparency, dithering, and the mask bit). Built in three sub-stages: **M4d-1** the
  untextured primitives (polygons, rectangles, lines + the shared clip/offset/mask/dither pipeline),
  **M4d-2** textures (4/8/15bpp + CLUT, U/V interpolation, the texture window, modulation, sprite
  flips), and **M4d-3** the four semi-transparency blend modes. Validated by pixel-exact VRAM diffs
  against the `gpu/` reference PNGs (`clipping`/`quad`/`triangle`/`texture-overflow`/`texture-flip`).
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

Dependencies: [`minifb`](https://crates.io/crates/minifb) (pure-Rust window/framebuffer, wired in at
M4e) and [`png`](https://crates.io/crates/png) (**test-harness only** — decodes the `ps1-tests`
reference VRAM images for the M4 pixel-diffs; the emulated machine never touches it). The foundation
milestones are headless.

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
