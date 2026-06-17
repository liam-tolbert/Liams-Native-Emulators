//! Core CHIP-8 interpreter: machine state plus fetch/decode/execute.
//!
//! CHIP-8 is a 1970s virtual machine. The entire system is:
//!   * 4 KB of RAM (programs load at 0x200; 0x000-0x1FF is reserved),
//!   * 16 eight-bit registers V0-VF (VF doubles as a carry/collision flag),
//!   * a 16-bit index register `i` and program counter `pc`,
//!   * a small call stack,
//!   * two 60 Hz timers (delay + sound),
//!   * a 64x32 monochrome display and a 16-key hex keypad.
//!
//! Every instruction is 2 bytes, stored big-endian. The whole CPU is the
//! `cycle()` method: fetch two bytes, decode the nibbles, execute.

pub const WIDTH: usize = 64;
pub const HEIGHT: usize = 32;

const MEMORY_SIZE: usize = 4096;
const PROGRAM_START: usize = 0x200; // ROMs are loaded here
const FONT_START: usize = 0x50; // conventional address for the built-in font

/// Built-in font: 16 hex digits (0-F), 5 bytes each, drawn as 4x5 sprites.
/// e.g. '0' = 0xF0,0x90,0x90,0x90,0xF0 ->  ####
///                                         #  #
///                                         #  #
///                                         #  #
///                                         ####
const FONTSET: [u8; 80] = [
    0xF0, 0x90, 0x90, 0x90, 0xF0, // 0
    0x20, 0x60, 0x20, 0x20, 0x70, // 1
    0xF0, 0x10, 0xF0, 0x80, 0xF0, // 2
    0xF0, 0x10, 0xF0, 0x10, 0xF0, // 3
    0x90, 0x90, 0xF0, 0x10, 0x10, // 4
    0xF0, 0x80, 0xF0, 0x10, 0xF0, // 5
    0xF0, 0x80, 0xF0, 0x90, 0xF0, // 6
    0xF0, 0x10, 0x20, 0x40, 0x40, // 7
    0xF0, 0x90, 0xF0, 0x90, 0xF0, // 8
    0xF0, 0x90, 0xF0, 0x10, 0xF0, // 9
    0xF0, 0x90, 0xF0, 0x90, 0x90, // A
    0xE0, 0x90, 0xE0, 0x90, 0xE0, // B
    0xF0, 0x80, 0x80, 0x80, 0xF0, // C
    0xE0, 0x90, 0x90, 0x90, 0xE0, // D
    0xF0, 0x80, 0xF0, 0x80, 0xF0, // E
    0xF0, 0x80, 0xF0, 0x80, 0x80, // F
];

pub struct Chip8 {
    memory: [u8; MEMORY_SIZE],
    v: [u8; 16],      // general registers V0-VF
    i: u16,           // index/address register
    pc: u16,          // program counter
    stack: [u16; 16], // call stack (return addresses)
    sp: usize,        // stack pointer (next free slot)
    delay_timer: u8,
    sound_timer: u8,
    rng_state: u32, // state for a tiny xorshift PRNG (opcode CXNN)

    /// Display, row-major. `true` = pixel lit. Read by the host to render.
    pub display: [bool; WIDTH * HEIGHT],
    /// Keypad state, indexed 0x0..=0xF. `true` = currently pressed.
    pub keys: [bool; 16],
}

impl Chip8 {
    pub fn new() -> Self {
        let mut memory = [0u8; MEMORY_SIZE];
        memory[FONT_START..FONT_START + FONTSET.len()].copy_from_slice(&FONTSET);

        // Seed the PRNG from the clock. xorshift requires a non-zero seed.
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0x1234_5678)
            | 1;

        Chip8 {
            memory,
            v: [0; 16],
            i: 0,
            pc: PROGRAM_START as u16,
            stack: [0; 16],
            sp: 0,
            delay_timer: 0,
            sound_timer: 0,
            rng_state: seed,
            display: [false; WIDTH * HEIGHT],
            keys: [false; 16],
        }
    }

    /// Copy a ROM image into memory at the program start address.
    pub fn load_rom(&mut self, rom: &[u8]) {
        let end = PROGRAM_START + rom.len();
        assert!(end <= MEMORY_SIZE, "ROM too large to fit in 4 KB of memory");
        self.memory[PROGRAM_START..end].copy_from_slice(rom);
    }

    /// Tick the delay and sound timers. Call exactly once per 60 Hz frame.
    pub fn tick_timers(&mut self) {
        self.delay_timer = self.delay_timer.saturating_sub(1);
        self.sound_timer = self.sound_timer.saturating_sub(1);
    }

    /// True while the buzzer should sound. Wire this to audio later.
    #[allow(dead_code)] // used once audio is added (see the TODO in main.rs)
    pub fn is_beeping(&self) -> bool {
        self.sound_timer > 0
    }

    /// xorshift32 — small, fast, dependency-free pseudo-randomness.
    fn rand_byte(&mut self) -> u8 {
        let mut s = self.rng_state;
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        self.rng_state = s;
        (s & 0xFF) as u8
    }

    /// Fetch, decode and execute one instruction. This is the whole CPU.
    pub fn cycle(&mut self) {
        // --- FETCH: opcodes are 2 bytes, big-endian ---
        let pc = self.pc as usize;
        let opcode = (self.memory[pc] as u16) << 8 | self.memory[pc + 1] as u16;
        self.pc += 2;

        // --- DECODE: pull out the standard operand fields ---
        let nnn = opcode & 0x0FFF; // 12-bit address
        let nn = (opcode & 0x00FF) as u8; // 8-bit immediate
        let n = (opcode & 0x000F) as u8; // 4-bit nibble
        let x = ((opcode & 0x0F00) >> 8) as usize; // register index X
        let y = ((opcode & 0x00F0) >> 4) as usize; // register index Y

        // --- EXECUTE: dispatch on the high nibble, refine as needed ---
        match opcode & 0xF000 {
            0x0000 => match nn {
                0xE0 => self.display = [false; WIDTH * HEIGHT], // 00E0: clear screen
                0xEE => {
                    // 00EE: return from subroutine
                    self.sp -= 1;
                    self.pc = self.stack[self.sp];
                }
                _ => {} // 0NNN: call native machine code — unused on interpreters
            },
            0x1000 => self.pc = nnn,                                // 1NNN: jump to NNN
            0x2000 => {
                // 2NNN: call subroutine at NNN
                self.stack[self.sp] = self.pc;
                self.sp += 1;
                self.pc = nnn;
            }
            0x3000 => if self.v[x] == nn { self.pc += 2 },          // 3XNN: skip if VX == NN
            0x4000 => if self.v[x] != nn { self.pc += 2 },          // 4XNN: skip if VX != NN
            0x5000 => if self.v[x] == self.v[y] { self.pc += 2 },   // 5XY0: skip if VX == VY
            0x6000 => self.v[x] = nn,                               // 6XNN: VX = NN
            0x7000 => self.v[x] = self.v[x].wrapping_add(nn),       // 7XNN: VX += NN (no carry)
            0x8000 => self.exec_arithmetic(x, y, n),                // 8XY_: register ALU ops
            0x9000 => if self.v[x] != self.v[y] { self.pc += 2 },   // 9XY0: skip if VX != VY
            0xA000 => self.i = nnn,                                 // ANNN: I = NNN
            // BNNN: jump to NNN + V0. (Quirk: SUPER-CHIP reinterpreted this as BXNN — jump to
            // XNN + VX, using the register named by the high nibble. We use the original COSMAC
            // VIP behaviour, NNN + V0, which is what the classic ROMs expect.)
            0xB000 => self.pc = nnn + self.v[0] as u16,
            0xC000 => self.v[x] = self.rand_byte() & nn,            // CXNN: VX = random & NN
            0xD000 => self.draw_sprite(x, y, n),                    // DXYN: draw sprite
            0xE000 => match nn {
                0x9E => if self.keys[(self.v[x] & 0xF) as usize] { self.pc += 2 }, // EX9E: skip if key down
                0xA1 => if !self.keys[(self.v[x] & 0xF) as usize] { self.pc += 2 }, // EXA1: skip if key up
                _ => {}
            },
            0xF000 => self.exec_misc(x, nn),                        // FX__: timers, memory, input
            _ => unreachable!("masked high nibble is always 0x0..0xF"),
        }
    }

    /// The 8XY_ family: register-to-register arithmetic and logic.
    fn exec_arithmetic(&mut self, x: usize, y: usize, n: u8) {
        match n {
            0x0 => self.v[x] = self.v[y],  // 8XY0: VX = VY
            0x1 => self.v[x] |= self.v[y], // 8XY1: VX |= VY
            0x2 => self.v[x] &= self.v[y], // 8XY2: VX &= VY
            0x3 => self.v[x] ^= self.v[y], // 8XY3: VX ^= VY
            // (Quirk: original COSMAC also zeroed VF on 8XY1/2/3. We don't — most modern ROMs assume not.)
            0x4 => {
                // 8XY4: VX += VY, VF = carry
                let (res, carry) = self.v[x].overflowing_add(self.v[y]);
                self.v[x] = res;
                self.v[0xF] = carry as u8;
            }
            0x5 => {
                // 8XY5: VX -= VY, VF = NOT borrow (1 if VX >= VY)
                let (res, borrow) = self.v[x].overflowing_sub(self.v[y]);
                self.v[x] = res;
                self.v[0xF] = (!borrow) as u8;
            }
            0x6 => {
                // 8XY6: VX >>= 1, VF = bit shifted out.
                // Quirk: original set VX = VY >> 1. Most modern ROMs expect this in-place shift.
                let lsb = self.v[x] & 1;
                self.v[x] >>= 1;
                self.v[0xF] = lsb;
            }
            0x7 => {
                // 8XY7: VX = VY - VX, VF = NOT borrow
                let (res, borrow) = self.v[y].overflowing_sub(self.v[x]);
                self.v[x] = res;
                self.v[0xF] = (!borrow) as u8;
            }
            0xE => {
                // 8XYE: VX <<= 1, VF = bit shifted out (see 8XY6 quirk note).
                let msb = (self.v[x] >> 7) & 1;
                self.v[x] <<= 1;
                self.v[0xF] = msb;
            }
            _ => {}
        }
    }

    /// The FX__ family: timers, index math, BCD, register/memory load-store, key wait.
    fn exec_misc(&mut self, x: usize, nn: u8) {
        match nn {
            0x07 => self.v[x] = self.delay_timer, // FX07: VX = delay timer
            0x0A => {
                // FX0A: block until any key is pressed, then store its index in VX.
                // Implemented by re-running this instruction (rewind PC) until a key is down.
                let mut pressed = false;
                for k in 0..16 {
                    if self.keys[k] {
                        self.v[x] = k as u8;
                        pressed = true;
                        break;
                    }
                }
                if !pressed {
                    self.pc -= 2;
                }
            }
            0x15 => self.delay_timer = self.v[x],                      // FX15: delay timer = VX
            0x18 => self.sound_timer = self.v[x],                      // FX18: sound timer = VX
            0x1E => self.i = self.i.wrapping_add(self.v[x] as u16),    // FX1E: I += VX
            0x29 => self.i = FONT_START as u16 + (self.v[x] & 0xF) as u16 * 5, // FX29: I = font sprite for VX
            0x33 => {
                // FX33: store the binary-coded decimal of VX at I, I+1, I+2
                let val = self.v[x];
                let i = self.i as usize;
                self.memory[i] = val / 100;
                self.memory[i + 1] = (val / 10) % 10;
                self.memory[i + 2] = val % 10;
            }
            0x55 => {
                // FX55: dump V0..=VX into memory starting at I.
                // (Quirk: some interpreters also advance I afterwards. We leave I unchanged.)
                for r in 0..=x {
                    self.memory[self.i as usize + r] = self.v[r];
                }
            }
            0x65 => {
                // FX65: load V0..=VX from memory starting at I.
                for r in 0..=x {
                    self.v[r] = self.memory[self.i as usize + r];
                }
            }
            _ => {}
        }
    }

    /// DXYN: draw an 8-pixel-wide, N-row-tall sprite from memory[I] at (VX, VY).
    /// Pixels are XORed onto the screen; VF is set to 1 if any lit pixel is erased.
    fn draw_sprite(&mut self, x: usize, y: usize, n: u8) {
        // Starting position wraps; individual pixels past an edge are clipped.
        let x0 = self.v[x] as usize % WIDTH;
        let y0 = self.v[y] as usize % HEIGHT;
        self.v[0xF] = 0;

        for row in 0..n as usize {
            let sprite_byte = self.memory[self.i as usize + row];
            for col in 0..8usize {
                let px = x0 + col;
                let py = y0 + row;
                if px >= WIDTH || py >= HEIGHT {
                    continue; // clip at the right / bottom edges
                }
                // Each sprite bit, MSB first, toggles one screen pixel.
                if sprite_byte & (0x80 >> col) != 0 {
                    let idx = py * WIDTH + px;
                    if self.display[idx] {
                        self.v[0xF] = 1; // a lit pixel is being turned off => collision
                    }
                    self.display[idx] ^= true;
                }
            }
        }
    }
}
