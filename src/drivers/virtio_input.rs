//! The virtio-input pointer: a working mouse cursor on the RISC-V console.
//!
//! QEMU's `virt` machine has no PS/2 controller, so the pointer arrives through
//! a **virtio-mmio input device** (`-device virtio-tablet-device` or
//! `-device virtio-mouse-device`) instead. This driver brings the device up,
//! subscribes to the events a pointer needs, keeps the device's event queue fed
//! with buffers, and turns the events into a screen position; the software
//! cursor itself lives in `drivers::framebuffer` (it is part of the console's
//! frame, not a second window).
//!
//! The choices that shape the code:
//!
//! * **Tablet first, mouse second.** A tablet sends *absolute* coordinates
//!   (`EV_ABS`/`ABS_X`,`ABS_Y`) scaled to its declared range, which map to screen
//!   pixels without any accumulation; a mouse sends relative deltas (`EV_REL`).
//!   The probe accepts both, and the declared range is read from the device
//!   (`CFG_ABS_INFO`) rather than assumed 0..32767.
//! * **Polled, like the block and net drivers.** The kernel's PLIC currently
//!   routes only the UART; a second external source would need a shared claim
//!   loop and a per-device IRQ line. The shell's idle loop already wakes at
//!   100 Hz for the scheduler, the network stack and the UART ring, so the input
//!   device is drained there — the pointer updates up to 25 times a second
//!   without an interrupt, which is enough for a text console.
//! * **The queue stays fed.** The device writes into driver-provided buffers and
//!   stops when the available ring is empty. Every consumed buffer is put back
//!   before the loop ends, so a burst of events cannot starve the device; the
//!   ring is sized to the device's maximum (capped at [`QUEUE_SIZE`]).
//! * **Absence is normal.** A headless CI boot has no input device; the driver
//!   logs one line, the cursor never appears, and everything else works exactly
//!   as before. Nothing above this module may assume a pointer exists.
//!
//! The transport set-up is deliberately a sibling of [`super::virtio_mmio`]
//! rather than a refactor of it: the block driver's queue is request-shaped
//! (build one chain, wait for its completion), while this one is a streaming
//! ring (post buffers once, drain used entries forever). Sharing a "queue" type
//! between the two would have to abstract over both, and the shared part would
//! be the register constants and the ring geometry — which is what is duplicated
//! here, with the spec citation in one place.

use alloc::boxed::Box;
use alloc::vec;
use core::sync::atomic::Ordering;

use crate::drivers::framebuffer;
use crate::fdt::{Fdt, VirtioMmio};
use crate::sync::Spinlock;

// --- virtio-mmio register offsets (virtio 1.2 §4.2.2) ----------------------

const MAGIC_VALUE: usize = 0x000;
const VERSION: usize = 0x004;
const DEVICE_ID: usize = 0x008;
const DEVICE_FEATURES: usize = 0x010;
const DEVICE_FEATURES_SEL: usize = 0x014;
const DRIVER_FEATURES: usize = 0x020;
const DRIVER_FEATURES_SEL: usize = 0x024;
const QUEUE_SEL: usize = 0x030;
const QUEUE_NUM_MAX: usize = 0x034;
const QUEUE_NUM: usize = 0x038;
const QUEUE_READY: usize = 0x044;
const QUEUE_NOTIFY: usize = 0x050;
const INTERRUPT_STATUS: usize = 0x060;
const INTERRUPT_ACK: usize = 0x064;
const STATUS: usize = 0x070;
const QUEUE_DESC_LOW: usize = 0x080;
const QUEUE_DRIVER_LOW: usize = 0x090;
const QUEUE_DEVICE_LOW: usize = 0x0a0;
const CONFIG: usize = 0x100;

/// `"virt"` as the little-endian magic-value register spells it.
const MAGIC: u32 = 0x7472_6976;
/// The transport version this driver implements (virtio 1.x, non-transitional).
const VERSION_MODERN: u32 = 2;

/// Virtio device ID of an input device (`VIRTIO_ID_INPUT`, §5.8.2).
const DEVICE_ID_INPUT: u32 = 18;

// Device status bits (§2.1).
const STATUS_ACKNOWLEDGE: u32 = 1 << 0;
const STATUS_DRIVER: u32 = 1 << 1;
const STATUS_DRIVER_OK: u32 = 1 << 2;
const STATUS_FEATURES_OK: u32 = 1 << 3;
const STATUS_FAILED: u32 = 1 << 7;

/// `VIRTIO_F_VERSION_1`, bit 32 of the 64-bit feature space.
const FEATURE_VERSION_1: u64 = 1 << 32;

// Queue geometry (§2.6): descriptor 16 B, available ring `4 + 2*num` plus a
// 2-byte trailer, used ring `4 + 8*num` plus a 2-byte trailer.
const DESC_BYTES: usize = 16;
const AVAIL_FIXED: usize = 4;
const AVAIL_TRAILER: usize = 2;
const USED_FIXED: usize = 4;
const USED_TRAILER: usize = 2;
const DESC_F_WRITE: u16 = 2;

/// Queue 0 is the event queue (§5.8.3); the input device has exactly one.
const QUEUE_INDEX: u32 = 0;
/// Event buffers this driver keeps in flight. Each event is 8 bytes; 64 buffers
/// is far more than the device produces between two polls at 100 Hz.
const QUEUE_SIZE: u16 = 64;
/// Smallest queue this driver accepts (a power of two, per the spec).
const MIN_QUEUE_SIZE: u16 = 8;

/// Polls before giving up on one `poll` call. The device answers synchronously
/// in QEMU; the bound keeps a wedged device from hanging the shell.
const POLL_LIMIT: usize = 10_000;

// --- virtio-input configuration (virtio 1.2 §5.8.4) -----------------------

/// Configuration selectors.
const CFG_EV_BITS: u8 = 0x11;
const CFG_ABS_INFO: u8 = 0x12;
/// The union's offset inside `virtio_input_config` (`select`, `subsel`, `size`,
/// `reserved[5]`).
const CFG_DATA: usize = 8;
/// Largest configuration bitmap the driver will write/read (128 bytes, §5.8.4).
const CFG_BITMAP_MAX: usize = 128;
/// Size of `struct virtio_input_absinfo`: min, max, fuzz, flat, reserved.
const ABS_INFO_BYTES: usize = 20;

/// Event types and codes this driver cares about (Linux input-event-codes).
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;
const SYN_REPORT: u16 = 0x00;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const BTN_LEFT: u16 = 0x110;

/// A pointer device's flavour, decided at probe time.
#[derive(Clone, Copy)]
enum Kind {
    /// Absolute coordinates in the device's declared range.
    Tablet { x: Axis, y: Axis },
    /// Relative deltas, accumulated by the driver.
    Mouse,
}

/// One absolute axis: the range the device reports for it.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Axis {
    min: u32,
    max: u32,
}

/// The pointer state consumers can read: framebuffer pixels and buttons.
#[derive(Clone, Copy)]
pub struct MouseState {
    pub x: usize,
    pub y: usize,
    pub left: bool,
}

/// Why a transport window could not be used, named for the serial log.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Error {
    NotVirtioMmio,
    Legacy(u32),
    NotBlock(u32),
    NoVersion1,
    FeatureRejected(u32),
    Reset(u32),
    DeviceFailed(u32),
    NoQueue,
    QueueTooSmall(u16),
    QueueNotReady,
    Layout,
    Timeout,
    /// A virtio-input device, but no event type this driver understands.
    NotPointer,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::NotVirtioMmio => write!(f, "no virtio-mmio magic word"),
            Error::Legacy(version) => {
                write!(
                    f,
                    "legacy virtio-mmio version {version} (this driver speaks 2)"
                )
            }
            Error::NotBlock(id) => write!(f, "virtio device id {id} is not an input device"),
            Error::NoVersion1 => write!(f, "the device does not offer VIRTIO_F_VERSION_1"),
            Error::FeatureRejected(status) => {
                write!(
                    f,
                    "the device did not accept the features (status {status:#x})"
                )
            }
            Error::Reset(status) => write!(f, "the device would not reset (status {status:#x})"),
            Error::DeviceFailed(status) => {
                write!(f, "the device reported FAILED (status {status:#x})")
            }
            Error::NoQueue => write!(f, "the device exposes no queue 0"),
            Error::QueueTooSmall(size) => write!(f, "queue 0 is too small ({size} entries)"),
            Error::QueueNotReady => write!(f, "queue 0 did not become ready"),
            Error::Layout => write!(f, "ring storage is not 8-byte aligned"),
            Error::Timeout => write!(f, "the device did not complete in time"),
            Error::NotPointer => write!(f, "the input device offers neither ABS nor REL axes"),
        }
    }
}

// --- MMIO helpers ---------------------------------------------------------

#[inline]
fn read32(base: usize, offset: usize) -> u32 {
    // SAFETY: every caller passes a virtio-mmio register inside a transport
    // window the boot page table maps as device memory (RW). A 32-bit volatile
    // load is the width the specification defines.
    unsafe { core::ptr::read_volatile((base + offset) as *const u32) }
}

#[inline]
fn write32(base: usize, offset: usize, value: u32) {
    // SAFETY: as above; device memory, no aliasing with kernel RAM.
    unsafe { core::ptr::write_volatile((base + offset) as *mut u32, value) }
}

#[inline]
fn write64(base: usize, offset: usize, value: u64) {
    // SAFETY: the two 32-bit halves of a 64-bit register; the transport requires
    // the low half first, which is the order these writes happen in.
    write32(base, offset, value as u32);
    write32(base, offset + 4, (value >> 32) as u32);
}

#[inline]
fn read8(base: usize, offset: usize) -> u8 {
    // SAFETY: byte-wide configuration access, same contract as `read32`.
    unsafe { core::ptr::read_volatile((base + offset) as *const u8) }
}

#[inline]
fn write8(base: usize, offset: usize, value: u8) {
    // SAFETY: byte-wide configuration access, same contract as `write32`.
    unsafe { core::ptr::write_volatile((base + offset) as *mut u8, value) }
}

/// The 64-bit feature space: selector 0 is bits 0..31, selector 1 bits 32..63.
fn device_features(base: usize) -> u64 {
    write32(base, DEVICE_FEATURES_SEL, 0);
    let low = read32(base, DEVICE_FEATURES) as u64;
    write32(base, DEVICE_FEATURES_SEL, 1);
    let high = read32(base, DEVICE_FEATURES) as u64;
    low | (high << 32)
}

fn write_driver_features(base: usize, features: u64) {
    write32(base, DRIVER_FEATURES_SEL, 0);
    write32(base, DRIVER_FEATURES, features as u32);
    write32(base, DRIVER_FEATURES_SEL, 1);
    write32(base, DRIVER_FEATURES, (features >> 32) as u32);
}

// --- virtio-input configuration ------------------------------------------

/// Point the configuration selector at `(select, subsel)`.
fn cfg_select(base: usize, select: u8, subsel: u8) {
    write8(base, CONFIG, select);
    write8(base, CONFIG + 1, subsel);
}

/// The configuration item's size in bytes, after a `cfg_select`.
fn cfg_size(base: usize) -> usize {
    read8(base, CONFIG + 2) as usize
}

fn cfg_read_u32(base: usize, index: usize) -> u32 {
    let at = CONFIG + CFG_DATA + index * 4;
    (read8(base, at) as u32)
        | ((read8(base, at + 1) as u32) << 8)
        | ((read8(base, at + 2) as u32) << 16)
        | ((read8(base, at + 3) as u32) << 24)
}

/// Read the `virtio_input_absinfo` of `code` (an `ABS_*` axis).
fn cfg_abs_info(base: usize, code: u16) -> Option<Axis> {
    cfg_select(base, CFG_ABS_INFO, code as u8);
    if cfg_size(base) != ABS_INFO_BYTES {
        return None;
    }
    let min = cfg_read_u32(base, 0);
    let max = cfg_read_u32(base, 1);
    // fuzz/flat are advisory; the reserved word must be ignored.
    if max <= min {
        return None;
    }
    Some(Axis { min, max })
}

/// Whether the device advertises any event code of `event_type` (its `EV_BITS`
/// bitmap is non-zero). Used only for capability detection.
fn cfg_has_event_bits(base: usize, event_type: u16) -> bool {
    cfg_select(base, CFG_EV_BITS, event_type as u8);
    let size = cfg_size(base).min(CFG_BITMAP_MAX);
    (0..size).any(|i| read8(base, CONFIG + CFG_DATA + i) != 0)
}

/// Subscribe to `codes` of `event_type`: the device filters its event stream by
/// this bitmap, so a code not listed here is simply never delivered.
fn cfg_subscribe(base: usize, event_type: u16, codes: &[u16]) -> Result<(), Error> {
    cfg_select(base, CFG_EV_BITS, event_type as u8);
    let size = cfg_size(base);
    if size == 0 || size > CFG_BITMAP_MAX {
        return Err(Error::NotPointer);
    }
    let mut bitmap = [0u8; CFG_BITMAP_MAX];
    for &code in codes {
        let byte = code as usize / 8;
        let bit = code as usize % 8;
        if byte < size {
            bitmap[byte] |= 1 << bit;
        }
    }
    for (i, byte) in bitmap[..size].iter().enumerate() {
        write8(base, CONFIG + CFG_DATA + i, *byte);
    }
    Ok(())
}

// --- virtqueue ------------------------------------------------------------

struct RingLayout {
    desc: usize,
    avail: usize,
    used: usize,
    total: usize,
}

fn ring_layout(num: u16) -> RingLayout {
    let desc = 0;
    let avail = desc + DESC_BYTES * num as usize;
    let used = (avail + AVAIL_FIXED + 2 * num as usize + AVAIL_TRAILER + 3) & !3;
    let total = used + USED_FIXED + 8 * num as usize + USED_TRAILER;
    RingLayout {
        desc,
        avail,
        used,
        total: (total + DESC_BYTES - 1) & !(DESC_BYTES - 1),
    }
}

/// One `virtio_input_event`: `type`, `code`, `value`, all little-endian.
#[derive(Clone, Copy)]
struct Event {
    kind: u16,
    code: u16,
    value: u32,
}

impl Event {
    fn parse(bytes: &[u8]) -> Self {
        let u16_at = |o: usize| (bytes[o] as u16) | ((bytes[o + 1] as u16) << 8);
        let u32_at = |o: usize| {
            (bytes[o] as u32)
                | ((bytes[o + 1] as u32) << 8)
                | ((bytes[o + 2] as u32) << 16)
                | ((bytes[o + 3] as u32) << 24)
        };
        Event {
            kind: u16_at(0),
            code: u16_at(2),
            value: u32_at(4),
        }
    }
}

/// The streaming event queue: buffers are posted once and returned to the ring
/// after each consumed event, so the device never runs dry.
struct Queue {
    num: u16,
    /// Ring storage. The `Box` gives the device a stable physical address for as
    /// long as the driver lives; `desc`/`avail`/`used` point inside it.
    #[allow(dead_code)] // kept for ownership and address stability
    rings: Box<[u8]>,
    /// One 8-byte event buffer per descriptor (device-writable).
    #[allow(dead_code)] // kept for ownership and address stability
    buffers: Box<[u8]>,
    desc: usize,
    avail: usize,
    used: usize,
    avail_idx: u16,
    last_used: u16,
}

impl Queue {
    fn new(num: u16) -> Result<Self, Error> {
        let layout = ring_layout(num);
        let rings = vec![0u8; layout.total].into_boxed_slice();
        let buffers = vec![0u8; num as usize * 8].into_boxed_slice();
        let storage = rings.as_ptr() as usize;
        if storage % DESC_BYTES != 0 || buffers.as_ptr() as usize % 8 != 0 {
            return Err(Error::Layout);
        }
        Ok(Queue {
            num,
            desc: storage + layout.desc,
            avail: storage + layout.avail,
            used: storage + layout.used,
            rings,
            buffers,
            avail_idx: 0,
            last_used: 0,
        })
    }

    /// Write one descriptor at `slot`: an 8-byte device-writable buffer.
    fn set_descriptor(&mut self, slot: u16, buffer: usize) {
        let at = self.desc + DESC_BYTES * slot as usize;
        let addr = buffer as u64;
        // SAFETY: `at` is inside this queue's descriptor table; the fields are
        // the specification's layout (addr u64, len u32, flags u16, next u16) and
        // the device reads them after the release fence in `bring_up`.
        unsafe {
            core::ptr::write_volatile(at as *mut u64, addr.to_le());
            core::ptr::write_volatile((at + 8) as *mut u32, 8u32.to_le());
            core::ptr::write_volatile((at + 12) as *mut u16, DESC_F_WRITE.to_le());
            core::ptr::write_volatile((at + 14) as *mut u16, 0u16.to_le());
        }
    }

    /// Put descriptor `slot` back on the available ring and bump its index.
    fn publish(&mut self, slot: u16) {
        let ring = self.avail_idx % self.num;
        let at = self.avail + AVAIL_FIXED + 2 * ring as usize;
        // SAFETY: `at` is inside the available ring for this queue.
        unsafe {
            core::ptr::write_volatile(at as *mut u16, slot.to_le());
        }
        self.avail_idx = self.avail_idx.wrapping_add(1);
    }

    /// Program the queue into the device and post every event buffer.
    fn bring_up(&mut self, base: usize) -> Result<(), Error> {
        write32(base, QUEUE_NUM, self.num as u32);

        // The queue addresses are physical addresses: the identity map's promise
        // (see `memory::paging`), so a pointer's value is what the device gets.
        write64(base, QUEUE_DESC_LOW, self.desc as u64);
        write64(base, QUEUE_DRIVER_LOW, self.avail as u64);
        write64(base, QUEUE_DEVICE_LOW, self.used as u64);

        // Posting every buffer once: the device writes into them and this driver
        // returns each descriptor as it consumes the event.
        for slot in 0..self.num {
            let buffer = self.buffers.as_ptr() as usize + slot as usize * 8;
            self.set_descriptor(slot, buffer);
            self.publish(slot);
        }
        // The device may not read the descriptors before the writes are visible:
        // `amoswap` is a full barrier on RISC-V, used here for its ordering.
        core::sync::atomic::fence(Ordering::Release);
        // SAFETY: the available ring's `idx` field is the u16 at its start + 2.
        unsafe {
            core::ptr::write_volatile((self.avail + 2) as *mut u16, self.avail_idx.to_le());
        }
        // A polled driver asks the device not to raise the queue interrupt.
        // SAFETY: the available ring's `flags` field is the u16 at its start.
        unsafe {
            core::ptr::write_volatile(self.avail as *mut u16, 1u16.to_le()); // NO_INTERRUPT
        }
        write32(base, QUEUE_READY, 1);
        if read32(base, QUEUE_READY) != 1 {
            return Err(Error::QueueNotReady);
        }
        write32(base, QUEUE_NOTIFY, 0);
        Ok(())
    }

    /// Drain every completed event into `out`, returning the count.
    ///
    /// Every consumed descriptor is republished before returning, so a device
    /// that produced more events than `out` can hold simply keeps them for the
    /// next call instead of stalling on an empty ring.
    fn drain(&mut self, base: usize, out: &mut [Event]) -> usize {
        // Acknowledge the transport's interrupt status even though polling never
        // enables it: leaving it set would be a trap for an interrupt-driven
        // successor (the same reasoning as the block driver).
        let interrupt = read32(base, INTERRUPT_STATUS);
        if interrupt != 0 {
            write32(base, INTERRUPT_ACK, interrupt);
        }

        let mut count = 0usize;
        let mut spins = 0usize;
        loop {
            // SAFETY: the used ring's `idx` is the u16 at its start + 2; the
            // device advances it after writing an entry, so the acquire fence
            // below orders the entry's contents before this read.
            let used_idx =
                u16::from_le(unsafe { core::ptr::read_volatile((self.used + 2) as *const u16) });
            if used_idx == self.last_used {
                break;
            }
            core::sync::atomic::fence(Ordering::Acquire);
            let slot = self.last_used % self.num;
            let at = self.used + USED_FIXED + 8 * slot as usize;
            // SAFETY: `at` is inside the used ring and the device wrote the entry
            // before advancing `idx`; both fields are 4-byte aligned.
            let (id, _len) = unsafe {
                (
                    u32::from_le(core::ptr::read_volatile(at as *const u32)),
                    u32::from_le(core::ptr::read_volatile((at + 4) as *const u32)),
                )
            };
            self.last_used = self.last_used.wrapping_add(1);
            if id >= self.num as u32 {
                continue; // a malformed entry: ignore it rather than index out of bounds
            }
            let event_at = self.buffers.as_ptr() as usize + id as usize * 8;
            // SAFETY: `event_at` is the 8-byte buffer of descriptor `id`, which
            // the device just wrote; reading it as bytes keeps the parse
            // alignment-independent.
            let event = {
                let bytes = unsafe { core::slice::from_raw_parts(event_at as *const u8, 8) };
                Event::parse(bytes)
            };
            // Return the buffer before the ring can dry up, then publish it.
            self.set_descriptor(id as u16, event_at);
            self.publish(id as u16);
            if count < out.len() {
                out[count] = event;
                count += 1;
            }
            spins += 1;
            if spins >= POLL_LIMIT {
                break;
            }
        }
        if spins != 0 {
            core::sync::atomic::fence(Ordering::Release);
            // SAFETY: as in `bring_up`; the available ring's `idx`.
            unsafe {
                core::ptr::write_volatile((self.avail + 2) as *mut u16, self.avail_idx.to_le());
            }
            write32(base, QUEUE_NOTIFY, 0);
        }
        count
    }
}

// --- the device -----------------------------------------------------------

/// The probed pointer device and the state the events build up.
struct Input {
    base: usize,
    kind: Kind,
    queue: Queue,
    /// Framebuffer geometry, cached so an event does not have to take the
    /// framebuffer lock just to read two numbers.
    screen: (usize, usize),
    /// Current pointer state (framebuffer pixels).
    x: i32,
    y: i32,
    left: bool,
    /// Pending absolute coordinates from `EV_ABS` events, applied on `SYN_REPORT`
    /// so one report is one pointer move.
    pending_x: Option<u32>,
    pending_y: Option<u32>,
}

/// The one pointer device the kernel knows about; `None` on a pointer-less boot.
static DEVICE: Spinlock<Option<Input>> = Spinlock::new(None);

/// Linux input-event keycodes this driver translates. Only the keys a text
/// console needs are named; everything else is ignored (a full keymap is a
/// keyboard-layout project, not a console one).
const KEY_BACKSPACE: u16 = 14;
const KEY_TAB: u16 = 15;
const KEY_ENTER: u16 = 28;
const KEY_LEFTCTRL: u16 = 29;
const KEY_LEFTSHIFT: u16 = 42;
const KEY_RIGHTSHIFT: u16 = 54;
const KEY_SPACE: u16 = 57;
const KEY_CAPSLOCK: u16 = 58;
const KEY_RIGHTCTRL: u16 = 97;

/// How many translated bytes the keyboard ring holds. A human types far slower
/// than the shell's 100 Hz idle poll, so this is a safety margin, not a buffer
/// for burst traffic.
const KEY_RING: usize = 128;

/// The probed keyboard: its queue and the modifier state between events.
struct Keyboard {
    base: usize,
    queue: Queue,
    shift: bool,
    caps: bool,
    ctrl: bool,
    keys: [u8; KEY_RING],
    head: usize,
    tail: usize,
}

/// The one keyboard the kernel knows about; `None` on a keyboard-less boot.
static KEYBOARD: Spinlock<Option<Keyboard>> = Spinlock::new(None);

/// What one transport window turned out to be.
enum Probed {
    Pointer(Input),
    Keyboard(Keyboard),
}

impl Keyboard {
    fn new(base: usize, queue: Queue) -> Self {
        Keyboard {
            base,
            queue,
            shift: false,
            caps: false,
            ctrl: false,
            keys: [0; KEY_RING],
            head: 0,
            tail: 0,
        }
    }

    /// Queue one translated byte; a full ring drops the byte (the alternative —
    /// blocking the idle loop — would freeze the cursor and the network).
    fn push(&mut self, byte: u8) {
        let next = (self.head + 1) % KEY_RING;
        if next != self.tail {
            self.keys[self.head] = byte;
            self.head = next;
        }
    }

    fn pop(&mut self) -> Option<u8> {
        if self.tail == self.head {
            return None;
        }
        let byte = self.keys[self.tail];
        self.tail = (self.tail + 1) % KEY_RING;
        Some(byte)
    }

    /// Translate one input event. Modifier keys change state on press/release;
    /// every other key produces a byte on press (and on autorepeat, so holding a
    /// key repeats like a terminal).
    fn handle(&mut self, event: &Event) {
        if event.kind != EV_KEY {
            return;
        }
        let pressed = event.value != 0;
        let repeat = event.value == 2;
        match event.code {
            KEY_LEFTSHIFT | KEY_RIGHTSHIFT => {
                self.shift = pressed;
                return;
            }
            KEY_LEFTCTRL | KEY_RIGHTCTRL => {
                self.ctrl = pressed;
                return;
            }
            KEY_CAPSLOCK => {
                if pressed && !repeat {
                    self.caps = !self.caps;
                }
                return;
            }
            _ => {}
        }
        if !pressed && !repeat {
            return;
        }
        match event.code {
            KEY_ENTER => self.push(b'\r'),
            KEY_BACKSPACE => self.push(0x7f),
            KEY_TAB => self.push(b'\t'),
            KEY_SPACE => self.push(b' '),
            // Ctrl is tracked but only Ctrl+C has a consumer in the shell; keep
            // it out of the byte stream (the shell ignores control bytes).
            _ if self.ctrl => {}
            code => {
                if let Some(byte) = key_to_ascii(code, self.shift, self.caps) {
                    self.push(byte);
                }
            }
        }
    }
}

/// Whether a pointer device was found (the cursor is enabled iff this is true).
pub fn present() -> bool {
    DEVICE.lock().is_some()
}

/// Whether a keyboard device was found.
pub fn keyboard_present() -> bool {
    KEYBOARD.lock().is_some()
}

/// The pointer device's name, for the boot log and `selftest`.
pub fn kind_name() -> &'static str {
    match DEVICE.lock().as_ref().map(|input| input.kind) {
        Some(Kind::Tablet { .. }) => "tablet",
        Some(Kind::Mouse) => "mouse",
        None => "none",
    }
}

/// Current pointer state (framebuffer pixels and buttons).
pub fn state() -> Option<MouseState> {
    DEVICE.lock().as_ref().map(|input| MouseState {
        x: input.x.max(0) as usize,
        y: input.y.max(0) as usize,
        left: input.left,
    })
}

/// The next byte typed on the keyboard, if any. The shell's idle loop consumes
/// these exactly like UART bytes, which is what makes the QEMU window a usable
/// console instead of a display for the serial terminal.
pub fn try_key() -> Option<u8> {
    KEYBOARD.lock().as_mut().and_then(|keyboard| keyboard.pop())
}

/// Drain both device queues: pointer events move the cursor, key events fill the
/// keyboard ring. Called from the shell's idle loop.
pub fn poll() -> usize {
    let mut events = [Event {
        kind: 0,
        code: 0,
        value: 0,
    }; 32];
    let mut total = 0usize;

    // Pointer first: its events are coalesced on SYN_REPORT into one move.
    let mut moved: Option<(usize, usize)> = None;
    {
        let mut guard = DEVICE.lock();
        if let Some(input) = guard.as_mut() {
            let count = input.queue.drain(input.base, &mut events);
            total += count;
            for event in &events[..count] {
                match (event.kind, event.code) {
                    (EV_ABS, ABS_X) => input.pending_x = Some(event.value),
                    (EV_ABS, ABS_Y) => input.pending_y = Some(event.value),
                    (EV_REL, REL_X) => input.x = input.x.saturating_add(event.value as i32),
                    (EV_REL, REL_Y) => input.y = input.y.saturating_sub(event.value as i32),
                    (EV_KEY, BTN_LEFT) => input.left = event.value != 0,
                    (EV_SYN, SYN_REPORT) => {
                        if let (Some(raw_x), Some(raw_y)) = (input.pending_x, input.pending_y) {
                            input.pending_x = None;
                            input.pending_y = None;
                            if let Kind::Tablet { x, y } = input.kind {
                                let (w, h) = input.screen;
                                input.x = scale_axis(raw_x, x, w);
                                input.y = scale_axis(raw_y, y, h);
                            }
                        }
                        let (w, h) = input.screen;
                        input.x = input.x.clamp(0, w.saturating_sub(1) as i32);
                        input.y = input.y.clamp(0, h.saturating_sub(1) as i32);
                        moved = Some((input.x as usize, input.y as usize));
                    }
                    _ => {}
                }
            }
        }
    }
    if let Some((x, y)) = moved {
        framebuffer::cursor_move_to(x, y);
    }

    // Then the keyboard: its events are translated in place and queued as bytes.
    let mut guard = KEYBOARD.lock();
    if let Some(keyboard) = guard.as_mut() {
        let count = keyboard.queue.drain(keyboard.base, &mut events);
        total += count;
        for event in &events[..count] {
            keyboard.handle(event);
        }
    }
    total
}

/// Map an absolute device coordinate to a framebuffer pixel.
///
/// The device's declared range is used, not an assumed 0..32767: a tablet that
/// reports 0..1023 must not put the pointer in the middle of the screen at
/// value 1023. `max` is exclusive in effect (the rightmost column is `w - 1`).
fn scale_axis(raw: u32, axis: Axis, pixels: usize) -> i32 {
    if pixels == 0 || axis.max <= axis.min {
        return 0;
    }
    let span = (axis.max - axis.min) as u64;
    let value = (raw.saturating_sub(axis.min) as u64).min(span);
    let last = (pixels - 1) as u64;
    ((value * last + span / 2) / span) as i32
}

/// US-layout keycode → ASCII, for the keys a console uses. Shift swaps the
/// symbol row; Caps Lock affects letters only (as on every terminal).
///
/// This is deliberately a table, not a layout engine: one console layout is a
/// documented limitation, and every array here is checked by the compiler.
fn key_to_ascii(code: u16, shift: bool, caps: bool) -> Option<u8> {
    // Unshifted - shifted pairs for the number/symbol rows.
    const ROWS: [(u16, u8, u8); 13] = [
        (2, b'1', b'!'),
        (3, b'2', b'@'),
        (4, b'3', b'#'),
        (5, b'4', b'$'),
        (6, b'5', b'%'),
        (7, b'6', b'^'),
        (8, b'7', b'&'),
        (9, b'8', b'*'),
        (10, b'9', b'('),
        (11, b'0', b')'),
        (12, b'-', b'_'),
        (13, b'=', b'+'),
        (41, b'`', b'~'),
    ];
    if let Some(&(_, plain, shifted)) = ROWS.iter().find(|(key, _, _)| *key == code) {
        return Some(if shift { shifted } else { plain });
    }
    const PUNCT: [(u16, u8, u8); 9] = [
        (26, b'[', b'{'),
        (27, b']', b'}'),
        (39, b';', b':'),
        (40, b'\'', b'"'),
        (43, b'\\', b'|'),
        (51, b',', b'<'),
        (52, b'.', b'>'),
        (53, b'/', b'?'),
        (86, b'\\', b'|'), // the extra key between Shift and Z on ISO boards
    ];
    if let Some(&(_, plain, shifted)) = PUNCT.iter().find(|(key, _, _)| *key == code) {
        return Some(if shift { shifted } else { plain });
    }
    const LETTERS: [(u16, u8); 26] = [
        (16, b'q'),
        (17, b'w'),
        (18, b'e'),
        (19, b'r'),
        (20, b't'),
        (21, b'y'),
        (22, b'u'),
        (23, b'i'),
        (24, b'o'),
        (25, b'p'),
        (30, b'a'),
        (31, b's'),
        (32, b'd'),
        (33, b'f'),
        (34, b'g'),
        (35, b'h'),
        (36, b'j'),
        (37, b'k'),
        (38, b'l'),
        (44, b'z'),
        (45, b'x'),
        (46, b'c'),
        (47, b'v'),
        (48, b'b'),
        (49, b'n'),
        (50, b'm'),
    ];
    LETTERS
        .iter()
        .find(|(key, _)| *key == code)
        .map(|&(_, letter)| {
            if shift ^ caps {
                letter.to_ascii_uppercase()
            } else {
                letter
            }
        })
}

/// Probe every transport window the device tree reported and bring up every
/// input device found (one pointer and one keyboard; QEMU exposes them as two
/// virtio-mmio devices with the same device id).
///
/// Returns whether at least one device is present. Absence is not an error (a
/// headless CI boot has none): it logs one line and leaves the console on serial.
pub fn init(tree: &Fdt) -> bool {
    let mut windows = [VirtioMmio::NONE; crate::fdt::MAX_VIRTIO_MMIO];
    let count = tree.virtio_mmio(&mut windows);
    let (width, height) = framebuffer::dimensions();
    // Devices are still probed on a machine without a screen (a serial-only
    // boot): the log line and the state are useful, and the cursor is simply not
    // enabled. The screen dimensions are only needed to scale absolute
    // coordinates, so `(0, 0)` disables the cursor, not the driver.

    let mut found = false;
    for window in &windows[..count] {
        match probe(window.base, (width, height)) {
            Ok(Probed::Pointer(input)) => {
                match input.kind {
                    Kind::Tablet { x, y } => crate::kprintln!(
                        "virtio-input: tablet at {:#x}, abs X {}..{}, Y {}..{}",
                        window.base,
                        x.min,
                        x.max,
                        y.min,
                        y.max
                    ),
                    Kind::Mouse => {
                        crate::kprintln!("virtio-input: mouse at {:#x} (relative)", window.base)
                    }
                }
                *DEVICE.lock() = Some(input);
                // The cursor is part of the console's frame from now on: centred
                // until the first report moves it, and drawn on every frame. With
                // no screen this is a no-op, but the device line above still
                // tells a serial-only log that a pointer exists.
                if width != 0 && height != 0 {
                    framebuffer::cursor_enable();
                }
                found = true;
            }
            Ok(Probed::Keyboard(keyboard)) => {
                crate::kprintln!("virtio-input: keyboard at {:#x}", window.base);
                *KEYBOARD.lock() = Some(keyboard);
                found = true;
            }
            // An empty slot is the normal case on a machine with fewer devices
            // than transports; only a device that exists but cannot be used is
            // worth a line.
            Err(Error::NotVirtioMmio) | Err(Error::NotBlock(_)) => {}
            Err(error) => {
                crate::kprintln!("virtio-input: {:#x}: {}", window.base, error);
            }
        }
    }
    if !found {
        crate::kprintln!("virtio-input: no device");
    }
    found
}

/// Subscribe to every code of `event_type` (the keyboard wants them all; the
/// pointer subscribes only to the axes and the button it consumes).
fn cfg_subscribe_all(base: usize, event_type: u16) -> Result<(), Error> {
    cfg_select(base, CFG_EV_BITS, event_type as u8);
    let size = cfg_size(base);
    if size == 0 || size > CFG_BITMAP_MAX {
        return Err(Error::NotPointer);
    }
    for i in 0..size {
        write8(base, CONFIG + CFG_DATA + i, 0xff);
    }
    Ok(())
}

/// Probe one transport window for an input device we can use.
fn probe(base: usize, screen: (usize, usize)) -> Result<Probed, Error> {
    if read32(base, MAGIC_VALUE) != MAGIC {
        return Err(Error::NotVirtioMmio);
    }
    let device_id = read32(base, DEVICE_ID);
    if device_id == 0 {
        return Err(Error::NotBlock(0));
    }
    if device_id != DEVICE_ID_INPUT {
        return Err(Error::NotBlock(device_id));
    }
    let version = read32(base, VERSION);
    if version != VERSION_MODERN {
        return Err(Error::Legacy(version));
    }

    write32(base, STATUS, 0);
    if read32(base, STATUS) != 0 {
        return Err(Error::Reset(read32(base, STATUS)));
    }
    write32(base, STATUS, STATUS_ACKNOWLEDGE);
    write32(base, STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);

    let offered = device_features(base);
    if offered & FEATURE_VERSION_1 == 0 {
        return Err(Error::NoVersion1);
    }
    write_driver_features(base, FEATURE_VERSION_1);
    let status = STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK;
    write32(base, STATUS, status);
    let readback = read32(base, STATUS);
    if readback & STATUS_FEATURES_OK == 0 {
        return Err(Error::FeatureRejected(readback));
    }
    if readback & STATUS_FAILED != 0 {
        return Err(Error::DeviceFailed(readback));
    }

    // Classify the device: absolute axes = tablet, relative axes = mouse,
    // otherwise EV_KEY capability = keyboard. QEMU gives both input devices the
    // same virtio id, so the capability bitmaps are the only discriminator.
    let abs_x = cfg_abs_info(base, ABS_X);
    let abs_y = cfg_abs_info(base, ABS_Y);
    let is_pointer = abs_x.is_some() && abs_y.is_some();
    let is_mouse = !is_pointer && cfg_has_event_bits(base, EV_REL);
    let is_keyboard = !is_pointer && !is_mouse && cfg_has_event_bits(base, EV_KEY);
    if !is_pointer && !is_mouse && !is_keyboard {
        return Err(Error::NotPointer);
    }

    if is_pointer {
        cfg_subscribe(base, EV_ABS, &[ABS_X, ABS_Y])?;
        cfg_subscribe(base, EV_KEY, &[BTN_LEFT])?;
    } else if is_mouse {
        cfg_subscribe(base, EV_REL, &[REL_X, REL_Y])?;
        cfg_subscribe(base, EV_KEY, &[BTN_LEFT])?;
    } else {
        cfg_subscribe_all(base, EV_KEY)?;
    }

    write32(base, QUEUE_SEL, QUEUE_INDEX);
    let max = read32(base, QUEUE_NUM_MAX).min(u16::MAX as u32) as u16;
    if max == 0 {
        return Err(Error::NoQueue);
    }
    if max < MIN_QUEUE_SIZE {
        return Err(Error::QueueTooSmall(max));
    }
    let num = QUEUE_SIZE.min(max);
    let mut queue = Queue::new(num)?;
    queue.bring_up(base)?;

    write32(
        base,
        STATUS,
        STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK,
    );
    if read32(base, STATUS) & STATUS_FAILED != 0 {
        return Err(Error::DeviceFailed(read32(base, STATUS)));
    }

    if is_keyboard {
        return Ok(Probed::Keyboard(Keyboard::new(base, queue)));
    }
    let kind = if is_pointer {
        Kind::Tablet {
            x: abs_x.unwrap(),
            y: abs_y.unwrap(),
        }
    } else {
        Kind::Mouse
    };
    Ok(Probed::Pointer(Input {
        base,
        kind,
        queue,
        screen,
        x: (screen.0 / 2) as i32,
        y: (screen.1 / 2) as i32,
        left: false,
        pending_x: None,
        pending_y: None,
    }))
}

/// The interrupt-layer selftest hook: is a pointer device present and sane?
///
/// `true` when a device exists (the shell loop proves it answers); with none, the
/// check reports SKIP rather than fail, because a headless CI boot is a valid
/// configuration. The queue itself is exercised by the running cursor.
pub fn selftest_present() -> bool {
    present()
}
