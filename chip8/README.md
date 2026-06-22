# CHIP-8 (`chip8/`)

A complete, from-scratch CHIP-8 interpreter in Rust — the warm-up emulator in this collection. All 35
opcodes, the sprite/XOR renderer, and the 60 Hz delay/sound timers; it builds clean and runs games.

## Building

```
cd chip8
cargo build --release
```

Only dependency: [`minifb`](https://crates.io/crates/minifb) (pure-Rust window/framebuffer/keyboard —
no SDL or native-linking hassle on Windows).

## Running

```
cd chip8
cargo run --release -- <rom.ch8> [cycles-per-frame]
```

- `cycles-per-frame` — how many instructions to execute per 60 Hz frame (default **10**); raise it to
  speed games up, lower it to slow them down.
- **Esc** quits.

Example:

```
cargo run --release -- "roms\Pong (1 player).ch8" 20
```

## Controls

CHIP-8's 16-key hex keypad maps to the left block of the keyboard:

```
   keyboard        CHIP-8
   1 2 3 4         1 2 3 C
   Q W E R   -->   4 5 6 D
   A S D F         7 8 9 E
   Z X C V         A 0 B F
```

## ROMs

ROMs aren't included — `chip8/roms/` is git-ignored. Drop `.ch8` files there and pass the path. On
Windows/PowerShell, `[ ]` in a filename are treated as wildcards (e.g.
`Space Invaders [David Winter].ch8`) — quote the path or use `-LiteralPath`.

## License

MIT — see the repository [LICENSE](../LICENSE).
