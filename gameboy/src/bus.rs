//! The memory bus / MMU — the hub the CPU talks to.
//!
//! The CPU sees the world *only* as 16-bit addresses, through `read`/`write`. The bus
//! decodes each address to the right device. It **owns every subsystem except the
//! CPU** (cartridge, PPU, timer, joypad, interrupts, plus work + high RAM); the CPU,
//! the one active component, owns the bus. That one-way ownership is what keeps Rust's
//! borrow checker happy — no two devices ever borrow each other.
//!
//! DMG memory map:
//! ```
//!   0x0000-0x7FFF  cartridge ROM (bank 0 + switchable bank)   -> cartridge
//!   0x8000-0x9FFF  VRAM                                       -> ppu
//!   0xA000-0xBFFF  cartridge RAM (external)                   -> cartridge
//!   0xC000-0xDFFF  WRAM
//!   0xE000-0xFDFF  Echo RAM (mirror of C000-DDFF)
//!   0xFE00-0xFE9F  OAM (sprite attributes)                    -> ppu
//!   0xFEA0-0xFEFF  unusable
//!   0xFF00         joypad                                     -> joypad
//!   0xFF01-0xFF02  serial  (SB/SC — Blargg test output!)
//!   0xFF04-0xFF07  timer                                      -> timer
//!   0xFF0F         IF (interrupt flags)                       -> interrupts
//!   0xFF10-0xFF3F  APU (audio) — stubbed, deferred
//!   0xFF40-0xFF4B  LCD / PPU registers                        -> ppu
//!   0xFF50         boot-ROM disable
//!   0xFF80-0xFFFE  HRAM
//!   0xFFFF         IE (interrupt enable)                      -> interrupts
//! ```

use crate::cartridge::{Cartridge, Header};
use crate::interrupts::Interrupts;
use crate::joypad::Joypad;
use crate::ppu::Ppu;
use crate::timer::Timer;

pub struct Bus {
    cart: Cartridge,
    pub ppu: Ppu,
    pub timer: Timer,
    pub joypad: Joypad,
    pub ints: Interrupts,

    wram: [u8; 0x2000],   // 0xC000-0xDFFF (mirrored as Echo RAM)
    hram: [u8; 0x7F],     // 0xFF80-0xFFFE
    apu_regs: [u8; 0x30], // 0xFF10-0xFF3F — audio deferred; keep reads/writes harmless

    serial_sb: u8,          // 0xFF01 latched serial byte
    pub serial_out: String, // everything the ROM has printed over serial (Blargg results)

    boot_rom_disabled: bool, // 0xFF50 — we HLE the boot, so this starts true
}

impl Bus {
    pub fn new(cart: Cartridge) -> Self {
        Self {
            cart,
            ppu: Ppu::new(),
            timer: Timer::new(),
            joypad: Joypad::new(),
            ints: Interrupts::new(),
            wram: [0; 0x2000],
            hram: [0; 0x7F],
            apu_regs: [0; 0x30],
            serial_sb: 0,
            serial_out: String::new(),
            boot_rom_disabled: true,
        }
    }

    pub fn header(&self) -> &Header {
        &self.cart.header
    }

    /// Advance the time-based subsystems by the T-cycles the last instruction took.
    ///
    /// This is the "catch-up" timing seam (see the plan): the CPU runs one whole
    /// instruction, returns its cycle count, and we tick everything else by that much.
    /// To move to M-cycle accuracy later, call `tick(4)` from *inside* `read`/`write`
    /// instead — the CPU never has to change.
    pub fn tick(&mut self, t_cycles: u8) {
        self.timer.step(t_cycles, &mut self.ints);
        self.ppu.step(t_cycles, &mut self.ints);
    }

    pub fn read(&self, addr: u16) -> u8 {
        match addr {
            0x0000..=0x7FFF => self.cart.read(addr),
            0x8000..=0x9FFF => self.ppu.read(addr),
            0xA000..=0xBFFF => self.cart.read(addr),
            0xC000..=0xDFFF => self.wram[(addr - 0xC000) as usize],
            0xE000..=0xFDFF => self.wram[(addr - 0xE000) as usize], // Echo RAM
            0xFE00..=0xFE9F => self.ppu.read(addr),
            0xFEA0..=0xFEFF => 0xFF, // unusable region
            0xFF00 => self.joypad.read(),
            0xFF01 => self.serial_sb,
            0xFF02 => 0x7E, // serial control — stubbed (bit 7 clear = not transferring)
            0xFF04..=0xFF07 => self.timer.read(addr),
            0xFF0F => self.ints.read_flag(),
            0xFF10..=0xFF3F => self.apu_regs[(addr - 0xFF10) as usize],
            0xFF40..=0xFF4B => self.ppu.read(addr),
            0xFF80..=0xFFFE => self.hram[(addr - 0xFF80) as usize],
            0xFFFF => self.ints.read_enable(),
            _ => 0xFF, // 0xFF03, 0xFF08-0xFF0E, 0xFF4C-0xFF7F: unmapped on DMG
        }
    }

    pub fn write(&mut self, addr: u16, val: u8) {
        match addr {
            0x0000..=0x7FFF => self.cart.write(addr, val),
            0x8000..=0x9FFF => self.ppu.write(addr, val),
            0xA000..=0xBFFF => self.cart.write(addr, val),
            0xC000..=0xDFFF => self.wram[(addr - 0xC000) as usize] = val,
            0xE000..=0xFDFF => self.wram[(addr - 0xE000) as usize] = val,
            0xFE00..=0xFE9F => self.ppu.write(addr, val),
            0xFEA0..=0xFEFF => {}
            0xFF00 => self.joypad.write(val),
            0xFF01 => self.serial_sb = val,
            0xFF02 => {
                // Blargg's CPU tests print results over serial: they put a byte in SB
                // (0xFF01) then write 0x81 here (start transfer, internal clock). We
                // grab the byte and log it — this is how we read pass/fail headlessly,
                // before the PPU can draw anything. The same trick will serve the PS1.
                if val == 0x81 {
                    let ch = self.serial_sb as char;
                    self.serial_out.push(ch);
                    print!("{ch}");
                    use std::io::Write;
                    let _ = std::io::stdout().flush();
                }
            }
            0xFF04..=0xFF07 => self.timer.write(addr, val),
            0xFF0F => self.ints.write_flag(val),
            0xFF10..=0xFF3F => self.apu_regs[(addr - 0xFF10) as usize] = val,
            0xFF40..=0xFF4B => self.ppu.write(addr, val), // incl. 0xFF46 OAM DMA — wired in M5
            0xFF50 => self.boot_rom_disabled = true,
            0xFF80..=0xFFFE => self.hram[(addr - 0xFF80) as usize] = val,
            0xFFFF => self.ints.write_enable(val),
            _ => {}
        }
    }
}
