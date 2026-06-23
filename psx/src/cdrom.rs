//! The CD-ROM drive controller — a bus device at 0x1F801800-0x1F801803.
//!
//! This is the gate to running a real disc game: the BIOS talks to this controller to spin the
//! drive, identify the disc, seek, and stream sectors — and only once it can read the disc does it
//! find `SYSTEM.CNF`, load the game's boot executable, and jump into it. (Wiring that boot path to a
//! real disc is the *next* stage; this stage builds the drive and proves it correct with a self-test.)
//!
//! **The drive is a second processor.** You don't call it and get an answer; you hand it a command and
//! it answers *later*, by raising an interrupt and leaving bytes in a response FIFO — and it will not
//! send the next part of its answer until you **acknowledge** the previous one. That request/ack
//! handshake (modelled here as a delayed-event queue gated on the acknowledge) is the whole character
//! of CD-ROM emulation; get it wrong and the drive looks "hung" when it's really waiting for you.
//!
//! **Four registers behind a 2-bit index.** 0x1800 is a status byte *and* an index latch; its low two
//! bits reselect what 0x1801-0x1803 mean. So a "write a command" is really: set index 0, push the
//! parameters, then write the command byte — at the wrong index those same addresses are volume knobs.
//!
//! Register map (8-bit; the CPU reaches them with `lb`/`sb`):
//! ```
//!   0x1800  status / index     write: index (bits 0-1)        read: status bits (FIFO-ready flags)
//!   0x1801  command / response  write@idx0: Command            read: Response FIFO (pop a byte)
//!   0x1802  param / IE / data   write@idx0: Parameter FIFO     read: Data FIFO (pop a sector byte)
//!                               write@idx1: Interrupt Enable
//!   0x1803  request / IF        write@idx0: Request (want-data) read@idx0: IE
//!                               write@idx1: Interrupt Flag (write-1-to-CLEAR = acknowledge)
//!                                                              read@idx1: IF (low 3 bits = INT number)
//! ```
//!
//! Timing is *plausible, not cycle-exact* (our flat ~2-cycle/instruction model can't be exact, and
//! the suite's `timing` golden log isn't a goal): responses arrive after a delay, and during a read,
//! sectors stream at ~the real rate (1 sector / 451,584 CPU cycles at single speed = 75 Hz).

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

// ===== timing constants (CPU cycles; plausibility anchors, never randomised) ===================
/// Single / double speed sector period: the 33.8688 MHz CPU clock / 75 (or 150) sectors per second.
const SECTOR_CYCLES_1X: u32 = 451_584;
const SECTOR_CYCLES_2X: u32 = 225_792;
/// Generic first-response (INT3 "ack") latency, and the longer Init two-phase / seek latencies.
const ACK_DELAY: u32 = 50_000;
const INIT_ACK: u32 = 75_000;
const INIT_DONE: u32 = 475_000;
const SEEK_DELAY: u32 = 1_000_000;

/// A raw CD sector is 2352 bytes (12 sync + 4 header + 2336 of subheader/data/ECC).
const SECTOR_RAW_LEN: usize = 2352;

// ===== drive status byte (the first response byte of most commands) =============================
const ST_ERROR: u8 = 0x01;
const ST_MOTOR: u8 = 0x02; // spindle motor on
const ST_SEEK: u8 = 0x40; // currently seeking
const ST_READ: u8 = 0x20; // currently reading data sectors
const ST_IDERR: u8 = 0x08; // disc-identification error
// (unused-for-now bits: 0x04 seek-error, 0x10 shell-open, 0x80 playing CD-DA)

/// One scheduled piece of a command's reply. A command enqueues one or more of these; the queue is
/// drained one event at a time, each gated on the host acknowledging the previous interrupt.
struct CdEvent {
    /// Which interrupt this delivers: 1 = data-sector-ready, 2 = second/complete, 3 = first ack,
    /// 5 = error.
    int: u8,
    /// Bytes pushed into the Response FIFO when this event fires.
    response: Vec<u8>,
    /// CPU cycles to wait (after it arms) before firing.
    delay: u32,
    /// Before firing, load the next sector into `sector_buf` and advance `read_lba` (ReadN's INT1).
    load_sector: bool,
    /// After firing, queue the *next* streamed sector (only while still `reading`) — the ReadN loop.
    reschedule: bool,
}

pub struct Cdrom {
    // ----- register-window state -----
    /// Low 2 bits of 0x1800 — selects what 0x1801-0x1803 mean.
    index: u8,
    /// Parameter FIFO (write 0x1802@idx0) — arguments for the next command. Max 16.
    param: VecDeque<u8>,
    /// Response FIFO (read 0x1801, any index). Popped on a `&self` register read, so it lives behind
    /// a `RefCell` — the same "a read that mutates" wrinkle the GPU's `dl_px` cursor has.
    response: RefCell<VecDeque<u8>>,
    /// Data FIFO (read 0x1802, any index; also drained by DMA channel 3). Filled by a Request
    /// (want-data) from the current sector. Behind `RefCell`/`Cell` for the same read-mutates reason.
    data: RefCell<Vec<u8>>,
    data_pos: Cell<usize>,
    /// Interrupt Enable (write 0x1803@idx1) — which INT numbers may pull the CPU's CDROM line.
    ie: u8,
    /// Interrupt Flag (read/write 0x1803@idx1) — low 3 bits hold the *pending* INT number (0 = none).
    /// Software writes a 1 to those bits to acknowledge.
    iflag: u8,

    // ----- drive state -----
    status: u8,        // the persistent status byte (motor / reading / ...)
    mode: u8,          // last Setmode byte (bit 7 = double speed, bit 5 = whole-sector size)
    double_speed: bool, // cached `mode & 0x80`
    seek_target: u32,  // LBA from the last Setloc
    read_lba: u32,     // LBA of the next sector to stream
    reading: bool,     // a ReadN/ReadS is active
    sector_buf: [u8; SECTOR_RAW_LEN], // the most-recently-loaded raw sector

    // ----- the request/response engine -----
    queue: VecDeque<CdEvent>, // events not yet armed
    countdown: u32,           // cycles left on the armed head
    armed: bool,              // is an event counting down?
    irq_pending: bool,        // an INT fired and hasn't been acked — gates arming the next event

    // ----- the disc -----
    disc: Option<CdImage>,
}

impl Cdrom {
    /// Power-on idle. **Documented choice:** real hardware starts motor-off and spins up on the first
    /// command; we start at steady idle (`ST_MOTOR`) so a self-contained device test sees stable
    /// Getstat values. Spin-up dynamics are an M5b refinement.
    pub fn new() -> Self {
        Self {
            index: 0,
            param: VecDeque::new(),
            response: RefCell::new(VecDeque::new()),
            data: RefCell::new(Vec::new()),
            data_pos: Cell::new(0),
            ie: 0,
            iflag: 0,
            status: ST_MOTOR,
            mode: 0,
            double_speed: false,
            seek_target: 0,
            read_lba: 0,
            reading: false,
            sector_buf: [0; SECTOR_RAW_LEN],
            queue: VecDeque::new(),
            countdown: 0,
            armed: false,
            irq_pending: false,
            disc: None,
        }
    }

    /// Insert a disc (used by the self-test and the `disc` run mode).
    pub fn load_disc(&mut self, img: CdImage) {
        self.disc = Some(img);
    }

    /// Borrow the inserted disc for *host-side* reads. The HLE disc-boot loader walks the disc's
    /// ISO9660 filesystem through this (to find `SYSTEM.CNF` and the boot executable) while the disc
    /// stays inserted, so the running game can still stream sectors through the drive proper.
    pub fn disc_ref(&self) -> Option<&CdImage> {
        self.disc.as_ref()
    }

    /// Test-only peek at the read mode (no hardware register exposes it directly).
    pub fn debug_mode(&self) -> u8 {
        self.mode
    }

    // ===== register reads (the `&self` path; FIFO pops go through interior mutability) ===========
    pub fn read(&self, offset: u32) -> u32 {
        match offset {
            0 => self.status_register() as u32,
            1 => self.response.borrow_mut().pop_front().unwrap_or(0) as u32, // Response FIFO
            2 => self.data_pop() as u32,                                     // Data FIFO
            3 => match self.index {
                0 | 2 => (self.ie | 0xE0) as u32,    // IE readback (upper bits read 1)
                _ => (self.iflag | 0xE0) as u32,     // IF: low 3 bits = pending INT number
            },
            _ => 0,
        }
    }

    /// Assemble the 0x1800 status byte. The FIFO-ready bits are what a polling BIOS spins on before it
    /// writes a command or reads a response — if they read 0 (the old stub) the boot hangs immediately.
    fn status_register(&self) -> u8 {
        let mut s = self.index & 3;
        if self.param.is_empty() {
            s |= 1 << 3; // PRMEMPT — parameter FIFO empty
        }
        if self.param.len() < 16 {
            s |= 1 << 4; // PRMWRDY — parameter FIFO has room
        }
        if !self.response.borrow().is_empty() {
            s |= 1 << 5; // RSLRRDY — a response byte is waiting
        }
        if self.data_pos.get() < self.data.borrow().len() {
            s |= 1 << 6; // DRQSTS — data is waiting in the data FIFO
        }
        if self.armed {
            s |= 1 << 7; // BUSYSTS — a command is in flight (approximate)
        }
        s
    }

    fn data_pop(&self) -> u8 {
        let pos = self.data_pos.get();
        let data = self.data.borrow();
        if pos < data.len() {
            self.data_pos.set(pos + 1);
            data[pos]
        } else {
            0
        }
    }

    // ===== register writes (the `&mut self` path) ===============================================
    pub fn write(&mut self, offset: u32, val: u32) {
        let v = val as u8;
        match offset {
            0 => self.index = v & 3,
            1 => {
                if self.index == 0 {
                    self.command(v); // index 1-3 writes are sound-map/volume — deferred
                }
            }
            2 => match self.index {
                0 => {
                    if self.param.len() < 16 {
                        self.param.push_back(v);
                    }
                }
                1 => self.ie = v & 0x1F,
                _ => {} // volume — deferred
            },
            3 => match self.index {
                0 => self.request(v),
                1 => self.ack_iflag(v),
                _ => {} // volume — deferred
            },
            _ => {}
        }
    }

    /// Request register (0x1803@idx0). Bit 7 (BFRD, "want data") copies the current sector's user-data
    /// slice into the data FIFO so the CPU/DMA can read it; otherwise it clears the FIFO. (Bit 5 SMEN
    /// for the sound-map is ignored — no audio yet.)
    fn request(&mut self, v: u8) {
        let bytes = if v & 0x80 != 0 {
            let (start, len) = self.sector_data_range();
            self.sector_buf[start..start + len].to_vec()
        } else {
            Vec::new()
        };
        *self.data.borrow_mut() = bytes;
        self.data_pos.set(0);
    }

    /// Interrupt Flag write (0x1803@idx1) — the **acknowledge**. Writing a 1 to a low bit *clears* that
    /// pending interrupt (opposite polarity from I_STAT's write-0-to-clear). Bit 6 resets the parameter
    /// FIFO. Once the INT field is clear the engine is free to deliver the next queued event.
    fn ack_iflag(&mut self, v: u8) {
        self.iflag &= !(v & 0x07);
        if v & 0x40 != 0 {
            self.param.clear();
        }
        if self.iflag & 0x07 == 0 {
            self.irq_pending = false;
        }
    }

    /// The user-data window inside the 2352-byte sector, by Setmode bit 5. Default (0x800 mode) skips
    /// 12 sync + 4 header + 8 subheader = 24 bytes and delivers 2048; whole-sector (0x924) skips only
    /// the 12-byte sync and delivers 2340. Mixing the two corrupts every read.
    fn sector_data_range(&self) -> (usize, usize) {
        if self.mode & 0x20 != 0 {
            (12, 2340)
        } else {
            (24, 2048)
        }
    }

    /// Pop one little-endian word (4 bytes) from the data FIFO — DMA channel 3 calls this per word.
    pub fn dma_read_word(&mut self) -> u32 {
        let b = [self.data_pop(), self.data_pop(), self.data_pop(), self.data_pop()];
        u32::from_le_bytes(b)
    }

    // ===== command decode -> schedule the reply =================================================
    /// Decode a command and enqueue its interrupt sequence. The reply isn't delivered here — `tick`
    /// arms and fires the queued events after their delays, gated on the host's acknowledges.
    fn command(&mut self, cmd: u8) {
        let p: Vec<u8> = self.param.drain(..).collect();
        let ack = ACK_DELAY;
        match cmd {
            0x01 => self.enqueue(3, vec![self.status], ack), // Getstat
            0x19 => {
                // Test — sub-function in param[0]. 0x20 returns the controller version/date.
                if p.first() == Some(&0x20) {
                    self.enqueue(3, vec![0x94, 0x09, 0x19, 0xC0], ack);
                } else {
                    self.enqueue(3, vec![self.status], ack);
                }
            }
            0x02 => {
                // Setloc — store the target position (params are BCD MM:SS:FF).
                if p.len() >= 3 {
                    self.seek_target = msf_bcd_to_lba(p[0], p[1], p[2]);
                }
                self.enqueue(3, vec![self.status], ack);
            }
            0x0E => {
                // Setmode — speed (bit 7), sector size (bit 5), etc.
                if let Some(&m) = p.first() {
                    self.mode = m;
                    self.double_speed = m & 0x80 != 0;
                }
                self.enqueue(3, vec![self.status], ack);
            }
            0x0A => {
                // Init — reset mode + motor; the canonical two-interrupt command.
                self.mode = 0;
                self.double_speed = false;
                self.reading = false;
                self.status = ST_MOTOR;
                self.queue.clear();
                self.enqueue(3, vec![self.status], INIT_ACK);
                self.enqueue(2, vec![self.status], INIT_DONE);
            }
            0x09 | 0x08 => {
                // Pause / Stop — halt streaming (and, for Stop, the motor). Clearing the queue drops
                // any in-flight ReadN sector so the stream really stops.
                self.reading = false;
                self.status &= !ST_READ;
                if cmd == 0x08 {
                    self.status &= !ST_MOTOR;
                }
                self.queue.clear();
                self.enqueue(3, vec![self.status], ack);
                self.enqueue(2, vec![self.status], ack);
            }
            0x0B | 0x0C => self.enqueue(3, vec![self.status], ack), // Mute / Demute (no audio yet)
            0x15 | 0x16 => {
                // SeekL / SeekP — move to the Setloc target; two-phase (ack, then complete).
                self.read_lba = self.seek_target;
                self.enqueue(3, vec![self.status | ST_SEEK], ack);
                self.enqueue(2, vec![self.status], SEEK_DELAY);
            }
            0x06 | 0x1B => {
                // ReadN / ReadS — start streaming sectors from the Setloc target. INT3 ack, then an
                // INT1 per sector (the first after a seek latency), forever until Pause.
                self.read_lba = self.seek_target;
                self.reading = true;
                self.status |= ST_READ;
                self.enqueue(3, vec![self.status], ack);
                self.queue.push_back(CdEvent {
                    int: 1,
                    response: vec![self.status],
                    delay: SEEK_DELAY,
                    load_sector: true,
                    reschedule: true,
                });
            }
            0x1A => {
                // GetID — disc identity + region. The BIOS checks the region string ('SCEA' = USA).
                if self.disc.is_some() {
                    self.enqueue(3, vec![self.status], ack);
                    self.enqueue(2, vec![0x02, 0x00, 0x20, 0x00, b'S', b'C', b'E', b'A'], ack);
                } else {
                    self.enqueue(3, vec![self.status], ack);
                    self.enqueue(5, vec![self.status | ST_IDERR, 0x40], ack);
                }
            }
            0x13 => self.enqueue(3, vec![self.status, 0x01, 0x01], ack), // GetTN — first/last track (BCD)
            0x1E => self.enqueue(3, vec![self.status], ack),            // GetTD / ReadTOC — no-op stub
            0x10 => {
                // GetlocL — position from the current sector header (synthesised from read_lba).
                let (m, s, f) = lba_to_msf_bcd(self.read_lba);
                self.enqueue(3, vec![m, s, f, 0x02, 0x00, 0x00, 0x00, 0x00], ack);
            }
            0x11 => {
                // GetlocP — subchannel-Q position (synthesised: single track, abs == rel here).
                let (m, s, f) = lba_to_msf_bcd(self.read_lba);
                self.enqueue(3, vec![0x01, 0x01, m, s, f, m, s, f], ack);
            }
            _ => self.enqueue(5, vec![self.status | ST_ERROR, 0x40], ack), // unknown -> error INT5
        }
    }

    fn enqueue(&mut self, int: u8, response: Vec<u8>, delay: u32) {
        self.queue.push_back(CdEvent { int, response, delay, load_sector: false, reschedule: false });
    }

    // ===== the catch-up tick — advance the event queue ==========================================
    /// Called from `bus.tick`. Returns whether the CDROM interrupt line should be pulled this step
    /// (the bus turns that into I_STAT bit 2). The gate: an event *arms* only when nothing is armed
    /// AND no interrupt is awaiting acknowledge, so the next reply can't even start counting down
    /// until the host has acked the previous one — exactly the hardware handshake.
    pub fn tick(&mut self, cycles: u32) -> bool {
        if !self.armed && !self.irq_pending {
            if let Some(ev) = self.queue.front() {
                self.countdown = ev.delay;
                self.armed = true;
            }
        }
        if !self.armed {
            return false;
        }
        if cycles >= self.countdown {
            let ev = self.queue.pop_front().unwrap();
            self.fire(ev);
            self.armed = false; // the next event arms on a later tick, once this INT is acked
            (self.iflag & self.ie & 0x07) != 0
        } else {
            self.countdown -= cycles;
            false
        }
    }

    /// Deliver one event: load the sector if asked, push the response bytes, latch the interrupt
    /// number, and (for a streaming read) queue the next sector.
    fn fire(&mut self, ev: CdEvent) {
        if ev.load_sector {
            let lba = self.read_lba;
            if let Some(s) = self.disc.as_ref().map(|d| d.read_sector(lba)) {
                self.sector_buf = s;
            }
            self.read_lba = lba.wrapping_add(1);
        }
        {
            // Each interrupt delivers a *fresh* response — the FIFO holds only the current reply, so
            // an unread previous response is discarded (as on hardware), not prepended to this one.
            let mut resp = self.response.borrow_mut();
            resp.clear();
            for b in &ev.response {
                if resp.len() < 16 {
                    resp.push_back(*b);
                }
            }
        }
        self.iflag = (self.iflag & !0x07) | (ev.int & 0x07);
        self.irq_pending = true;
        if ev.reschedule && self.reading {
            let period = if self.double_speed { SECTOR_CYCLES_2X } else { SECTOR_CYCLES_1X };
            let status = self.status;
            self.queue.push_back(CdEvent {
                int: 1,
                response: vec![status],
                delay: period,
                load_sector: true,
                reschedule: true,
            });
        }
    }
}

// ===== the disc image ==========================================================================
/// How a `CdImage`'s sectors are stored. The self-test builds a tiny image entirely in memory; a real
/// disc is far too big for that (the MvC `.bin` is ~408 MiB), so it streams one sector at a time from
/// the open file. `read_sector` is on the drive's `&self` read path, so the `File` handle lives behind
/// a `RefCell` — `seek`+`read` mutate it, the same "a read that mutates" wrinkle the FIFOs have above.
enum Backing {
    Memory(Vec<u8>),
    File(RefCell<File>),
}

/// A CD image as a flat run of 2352-byte raw sectors (MODE2/2352, the standard PS1 layout). The
/// drive seeks by LBA and reads forward; the user-data slice within each sector is carved out by the
/// drive per its sector-size mode.
pub struct CdImage {
    backing: Backing,
    sector_count: usize,
}

impl CdImage {
    /// Build directly from raw `.bin` bytes, held in memory (the self-test uses a synthetic image
    /// this way). Unchanged from before the streaming split — the self-test gate rides on this path.
    pub fn from_bin(bytes: Vec<u8>) -> Self {
        let sector_count = bytes.len() / SECTOR_RAW_LEN;
        Self {
            backing: Backing::Memory(bytes),
            sector_count,
        }
    }

    /// Parse a `.cue` and open its `.bin` (single MODE2/2352 data track) for streaming. Resolves the
    /// binary by the cue's `FILE` name, falling back to the only sibling `*.bin` if that name is
    /// missing — which is how the project's `MvC.bin` (whose cue names a different file) auto-resolves.
    /// The file is *opened*, not read into memory: a real disc is hundreds of MiB, and the drive only
    /// ever needs one 2352-byte sector at a time, so `read_sector` seeks per sector instead.
    pub fn from_cue(path: &Path) -> std::io::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let named = text.lines().find_map(|l| {
            let l = l.trim();
            if l.starts_with("FILE") {
                let a = l.find('"')?;
                let b = l[a + 1..].find('"')? + a + 1;
                Some(l[a + 1..b].to_string())
            } else {
                None
            }
        });
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        let bin = named
            .map(|n| dir.join(n))
            .filter(|p| p.exists())
            .or_else(|| single_bin_sibling(dir))
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "no .bin found for .cue")
            })?;
        let file = File::open(bin)?;
        let sector_count = file.metadata()?.len() as usize / SECTOR_RAW_LEN;
        Ok(Self {
            backing: Backing::File(RefCell::new(file)),
            sector_count,
        })
    }

    /// Read one raw 2352-byte sector by LBA. Past the end of the disc it reads as zeros (a malformed
    /// pointer shouldn't panic the emulator). A memory image copies the slice; a file image seeks to
    /// the sector and reads it — `read_exact` leaves the tail zeroed for a short final sector.
    pub fn read_sector(&self, lba: u32) -> [u8; SECTOR_RAW_LEN] {
        let mut out = [0u8; SECTOR_RAW_LEN];
        let off = lba as usize * SECTOR_RAW_LEN;
        match &self.backing {
            Backing::Memory(bytes) => {
                if off + SECTOR_RAW_LEN <= bytes.len() {
                    out.copy_from_slice(&bytes[off..off + SECTOR_RAW_LEN]);
                }
            }
            Backing::File(f) => {
                if (lba as usize) < self.sector_count {
                    let mut f = f.borrow_mut();
                    if f.seek(SeekFrom::Start(off as u64)).is_ok() {
                        let _ = f.read_exact(&mut out);
                    }
                }
            }
        }
        out
    }

    pub fn sector_count(&self) -> usize {
        self.sector_count
    }
}

/// The single `*.bin` next to a `.cue`, if exactly one exists (else ambiguous -> `None`).
fn single_bin_sibling(dir: &Path) -> Option<PathBuf> {
    let mut found = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let p = entry.path();
        if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("bin")) {
            if found.is_some() {
                return None; // more than one .bin — can't choose
            }
            found = Some(p);
        }
    }
    found
}

// ===== MSF <-> LBA (BCD minute:second:frame <-> linear sector) =================================
// A CD addresses sectors as minute:second:frame at 75 frames/second, in BCD. The 150-frame (2-second)
// pre-gap is mandatory: LBA 0 is at 00:02:00, so the conversion subtracts 150. Drop that and every
// read lands 150 sectors late — the classic "boot reads garbage" bug.
fn bcd_to_bin(b: u8) -> u8 {
    (b >> 4) * 10 + (b & 0x0F)
}
fn bin_to_bcd(v: u8) -> u8 {
    ((v / 10) << 4) | (v % 10)
}
pub(crate) fn msf_bcd_to_lba(m: u8, s: u8, f: u8) -> u32 {
    let m = bcd_to_bin(m) as u32;
    let s = bcd_to_bin(s) as u32;
    let f = bcd_to_bin(f) as u32;
    ((m * 60 + s) * 75 + f).saturating_sub(150)
}
pub(crate) fn lba_to_msf_bcd(lba: u32) -> (u8, u8, u8) {
    let v = lba + 150;
    (
        bin_to_bcd((v / 75 / 60) as u8),
        bin_to_bcd((v / 75 % 60) as u8),
        bin_to_bcd((v % 75) as u8),
    )
}
