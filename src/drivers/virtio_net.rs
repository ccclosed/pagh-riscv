//! The virtio-mmio network device: modern virtio 1.x (transport version 2),
//! polled, two split virtqueues (receive and transmit).
//!
//! QEMU `virt` exposes one virtio-mmio transport per device-tree window, and
//! `-device virtio-net-device,netdev=…` plugs a NIC into one of them. This
//! driver claims the first window whose `DeviceID` is 1 (`VIRTIO_ID_NET`),
//! negotiates `VIRTIO_F_VERSION_1` (+ `VIRTIO_NET_F_MAC` when offered), posts
//! receive buffers, and publishes a tiny polled interface:
//! [`send`], [`recv`], [`mac`], [`present`].
//!
//! Everything the block driver's header says about the transport applies here
//! too (modern only, polled completion, split virtqueue with direct
//! descriptors, identity-mapped physical addresses, heap-backed rings); the
//! differences are the device-specific ones:
//!
//! * **Two queues.** Receive is queue 0, transmit is queue 1. Receive buffers
//!   are posted up front and re-posted as completions are drained, so a burst
//!   of broadcast traffic cannot starve the queue.
//! * **The virtio-net header.** Every buffer starts with a 10-byte
//!   `virtio_net_hdr` (flags, gso type, header length, gso size, checksum
//!   start/offset). This driver negotiates neither GSO nor checksum offload, so
//!   the header is all zeroes in each direction and the driver never looks at
//!   it beyond skipping it; `MRG_RXBUF` is not negotiated either, so one buffer
//!   is exactly one frame.
//! * **Copies, not zero-copy.** Completions are copied out of the receive
//!   buffers into a small frame ring before the buffer is re-posted. A zero-copy
//!   scheme would have to keep buffers out of the device's hands for an
//!   unbounded time, and this stack is synchronous: the network layer consumes
//!   one frame at a time from the ring.
//! * **No interrupt.** There is no PLIC setup for virtio (the block driver made
//!   the same choice); the available ring carries `NO_INTERRUPT` and every wait
//!   is a bounded poll. A wedged device therefore reports `false`, never hangs
//!   the machine forever.
//!
//! What the driver deliberately does not do is listed in
//! `src/drivers/README.md` (interrupts, mergeable buffers, GSO, checksum
//! offload, packed rings, multiple queue pairs, VLAN, MAC filtering).

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec;
use core::sync::atomic::{fence, AtomicBool, Ordering};

use crate::fdt::{Fdt, VirtioMmio};
use crate::sync::Spinlock;

// --- virtio-mmio register offsets (virtio 1.1 §4.2.2) ----------------------

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
const CONFIG_GENERATION: usize = 0x0fc;
/// Device-specific configuration: for `virtio-net` it starts with the MAC
/// address when `VIRTIO_NET_F_MAC` is negotiated.
const CONFIG: usize = 0x100;

/// `"virt"` as the little-endian magic-value register spells it.
const MAGIC: u32 = 0x7472_6976;
/// Transport version this driver implements (virtio 1.x, non-transitional).
const VERSION_MODERN: u32 = 2;
/// Virtio device ID of a network card.
const DEVICE_ID_NET: u32 = 1;

// Device status bits (§2.1).
const STATUS_ACKNOWLEDGE: u32 = 1 << 0;
const STATUS_DRIVER: u32 = 1 << 1;
const STATUS_DRIVER_OK: u32 = 1 << 2;
const STATUS_FEATURES_OK: u32 = 1 << 3;
const STATUS_FAILED: u32 = 1 << 7;

// Feature bits (64-bit space: selector 0 = bits 0..31, selector 1 = 32..63).
const FEATURE_NET_MAC: u64 = 1 << 5;
/// Mergeable receive buffers: with it the RX header carries `num_buffers`; the
/// driver posts buffers large enough that one buffer always holds one frame.
const FEATURE_NET_MRG_RXBUF: u64 = 1 << 15;
const FEATURE_VERSION_1: u64 = 1 << 32;

/// Receive queue index (virtio-net §5.1.2).
const QUEUE_RX: u32 = 0;
/// Transmit queue index.
const QUEUE_TX: u32 = 1;

// Descriptor flags (§2.6.5). Transmits use one descriptor (header + frame are
// contiguous), so only the write bit is needed; a chained request would set
// NEXT as well.
const DESC_F_WRITE: u16 = 2;
/// The available ring's "do not interrupt me" flag (§2.6.7).
const AVAIL_F_NO_INTERRUPT: u16 = 1;

const DESC_BYTES: usize = 16;
const AVAIL_FIXED: usize = 4;
const AVAIL_TRAILER: usize = 2;
const USED_FIXED: usize = 4;
const USED_TRAILER: usize = 2;

/// Bytes of the virtio-net header this driver negotiates.
///
/// `VIRTIO_NET_F_MRG_RXBUF` is negotiated (QEMU offers it, and the driver uses
/// one 2 KiB buffer per frame so merging never has to split a packet). With
/// mergeable buffers the header is `struct virtio_net_hdr_mrg_rxbuf`: the
/// 10-byte base header plus the little-endian `num_buffers` field, i.e. 12
/// bytes. QEMU's modern virtio-net transport uses that layout even before the
/// negotiation is consulted, which is how the first version of this driver got
/// a two-byte shift between TX frames and the captured wire traffic.
pub const NET_HEADER_BYTES: usize = 12;
/// Largest frame the driver accepts: a 1500-byte MTU plus the Ethernet header.
pub const MAX_FRAME_BYTES: usize = 1514;
/// One receive buffer. 2048 is the conventional virtio-net size and leaves
/// room for the header and a jumbo frame if the MTU ever grows.
const RX_BUFFER_BYTES: usize = 2048;
/// Receive buffers posted at once. A power of two so the ring arithmetic is
/// exact; eight buffers absorb a broadcast burst while the stack polls.
const RX_BUFFERS: usize = 8;
/// Transmit buffers. Transmits are synchronous (submit + wait), so a small
/// rotating set only exists so the previous buffer is never reused while the
/// device might still be reading it.
const TX_BUFFERS: usize = 4;
/// Entries per virtqueue: enough for [`RX_BUFFERS`]/[`TX_BUFFERS`] plus the
/// descriptor chain a transmit uses.
const QUEUE_SIZE: u16 = 16;
const MIN_QUEUE_SIZE: u16 = 4;
/// Bounded poll: a device that never completes a request is declared dead for
/// that call instead of hanging the machine.
const POLL_LIMIT: u32 = 1 << 22;
/// Received frames copied out of the device buffers before the stack drains
/// them.
const FRAME_SLOTS: usize = 8;
/// Attempts at a stable configuration-space read.
const CONFIG_READ_ATTEMPTS: u32 = 16;

/// Why a window could not be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The window does not answer with the virtio-mmio magic word.
    NotVirtioMmio,
    /// A legacy (version 1) transport; this driver speaks version 2 only.
    Legacy(u32),
    /// An empty transport slot (`DeviceID` reads 0).
    Empty,
    /// The transport holds some other virtio device.
    NotNet(u32),
    /// `VIRTIO_F_VERSION_1` was not offered.
    NoVersion1,
    /// `VIRTIO_NET_F_MRG_RXBUF` was not offered; this driver's fixed 12-byte
    /// header (see [`NET_HEADER_BYTES`]) depends on it.
    NoMergeableRxBuffers,
    /// The device did not accept the feature set.
    FeatureRejected(u32),
    /// The device would not reset to status 0.
    Reset(u32),
    /// The device reported `FAILED`.
    DeviceFailed(u32),
    /// A queue the driver needs is absent or too small.
    NoQueue(u32, u16),
    /// `QueueReady` did not read back as 1.
    QueueNotReady,
    /// Ring storage was not 16-byte aligned as the descriptor table requires.
    Layout(usize),
    /// A transmit frame is empty or larger than [`MAX_FRAME_BYTES`].
    Frame(usize),
    /// The device did not complete the request within the poll limit.
    Timeout,
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
            Error::Empty => write!(f, "empty transport slot"),
            Error::NotNet(id) => write!(f, "virtio device id {id} is not a network card"),
            Error::NoVersion1 => write!(f, "the device does not offer VIRTIO_F_VERSION_1"),
            Error::NoMergeableRxBuffers => {
                write!(f, "the device does not offer VIRTIO_NET_F_MRG_RXBUF")
            }
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
            Error::NoQueue(index, size) => {
                write!(f, "queue {index} is absent or too small ({size} entries)")
            }
            Error::QueueNotReady => write!(f, "the queue did not become ready"),
            Error::Layout(address) => {
                write!(f, "ring storage at {address:#x} is not 16-byte aligned")
            }
            Error::Frame(len) => {
                write!(f, "{len} bytes is not a frame this NIC can transmit")
            }
            Error::Timeout => write!(f, "the device did not complete the request"),
        }
    }
}

// --- transport helpers -----------------------------------------------------

#[inline]
fn read32(base: usize, offset: usize) -> u32 {
    // SAFETY: `base` is a virtio-mmio window the device tree reported and
    // `offset` is one of this module's register offsets (all 4-byte aligned and
    // inside the 0x1000-byte register file). The window is identity-mapped as
    // device memory by `memory::paging`.
    unsafe { ((base + offset) as *const u32).read_volatile() }
}

#[inline]
fn write32(base: usize, offset: usize, value: u32) {
    // SAFETY: as in `read32`: a device-tree window, a known register offset, and
    // the identity map makes the pointer the physical address the device sees.
    unsafe { ((base + offset) as *mut u32).write_volatile(value) }
}

#[inline]
fn read8(base: usize, offset: usize) -> u8 {
    // SAFETY: byte accesses are naturally aligned inside the same window.
    unsafe { ((base + offset) as *const u8).read_volatile() }
}

fn write64(base: usize, offset: usize, value: u64) {
    // SAFETY: the queue-address registers are two 32-bit halves; each store is
    // 4-byte aligned inside the window.
    unsafe {
        ((base + offset) as *mut u32).write_volatile(value as u32);
        ((base + offset + 4) as *mut u32).write_volatile((value >> 32) as u32);
    }
}

/// Read the 64-bit feature space: selector 0 carries bits 0..31, selector 1 the
/// upper half.
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

/// Byte offsets of one split-virtqueue's three arrays inside its ring block.
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

/// Write one 16-byte descriptor.
fn write_desc(table: usize, index: u16, address: u64, len: u32, flags: u16, next: u16) {
    let at = table + index as usize * DESC_BYTES;
    // SAFETY: `table` is this queue's descriptor table (inside heap storage),
    // `index < num`, and every field is written at its spec offset with its spec
    // width. The table is 16-byte aligned, so the 64-bit store is aligned.
    unsafe {
        core::ptr::write_volatile(at as *mut u64, address.to_le());
        core::ptr::write_volatile((at + 8) as *mut u32, len.to_le());
        core::ptr::write_volatile((at + 12) as *mut u16, flags.to_le());
        core::ptr::write_volatile((at + 14) as *mut u16, next.to_le());
    }
}

/// One split virtqueue with its own ring block and the driver-side indices.
struct Queue {
    num: u16,
    #[allow(dead_code)] // ownership keeps the device's addresses stable
    rings: Box<[u8]>,
    desc: usize,
    avail: usize,
    used: usize,
    avail_idx: u16,
    last_used: u16,
}

impl Queue {
    fn new(num: u16, rings: Box<[u8]>) -> Self {
        let layout = ring_layout(num);
        let storage = rings.as_ptr() as usize;
        Queue {
            num,
            desc: storage + layout.desc,
            avail: storage + layout.avail,
            used: storage + layout.used,
            avail_idx: 0,
            last_used: 0,
            rings,
        }
    }

    /// Publish descriptor `id` in the available ring and ring the doorbell.
    fn publish(&mut self, base: usize, queue: u32, id: u16) {
        let slot = self.avail_idx % self.num;
        // SAFETY: `avail` is this queue's available ring inside `rings`, `slot`
        // is in range, and the u16 entry is 2-byte aligned by the layout.
        unsafe {
            core::ptr::write_volatile(
                (self.avail + AVAIL_FIXED + 2 * slot as usize) as *mut u16,
                id,
            );
        }
        fence(Ordering::Release);
        self.avail_idx = self.avail_idx.wrapping_add(1);
        // SAFETY: the `idx` field lives at offset 2 of the same ring.
        unsafe {
            core::ptr::write_volatile((self.avail + 2) as *mut u16, self.avail_idx);
        }
        fence(Ordering::Release);
        write32(base, QUEUE_NOTIFY, queue);
    }

    /// Pop one used-ring entry: `(descriptor id, bytes written by the device)`.
    fn take_used(&mut self, base: usize) -> Option<(u16, u32)> {
        // SAFETY: `used` is this queue's used ring inside `rings`; `idx` is the
        // 16-bit field the device writes.
        let used_idx = unsafe { core::ptr::read_volatile((self.used + 2) as *const u16) };
        if used_idx == self.last_used {
            return None;
        }
        // The device wrote the entry before advancing `idx`.
        fence(Ordering::Acquire);
        let slot = self.last_used % self.num;
        // SAFETY: `slot` is in range; both fields are 4-byte aligned entries the
        // device wrote before advancing `idx` (observed above).
        let (id, len) = unsafe {
            let at = self.used + USED_FIXED + 8 * slot as usize;
            (
                core::ptr::read_volatile(at as *const u32) as u16,
                core::ptr::read_volatile((at + 4) as *const u32),
            )
        };
        self.last_used = used_idx;
        // Acknowledge any interrupt the transport latched even though this
        // driver asked for none (the block driver does the same for the same
        // reason: a future interrupt-driven device must not find it set).
        let interrupt = read32(base, INTERRUPT_STATUS);
        if interrupt != 0 {
            write32(base, INTERRUPT_ACK, interrupt);
        }
        Some((id, len))
    }
}

/// Receive side: the posted buffers, the queue, and the completed frames that
/// the network layer has not consumed yet.
struct Rx {
    queue: Queue,
    /// `RX_BUFFERS` buffers, each [`RX_BUFFER_BYTES`].
    buffers: Box<[u8]>,
    /// Frame copies, oldest at `head`; flat `FRAME_SLOTS × MAX_FRAME_BYTES`.
    ///
    /// Heap storage, not an inline array: `VirtioNet` is built on the boot stack
    /// and moved into its `Arc`, and a 12 KiB inline frame ring made that path
    /// deep enough to overflow the 64 KiB boot stack (the failure looked like a
    /// corrupted heap: the overflow landed on the allocator's state below it).
    frames: Box<[u8]>,
    lens: [u16; FRAME_SLOTS],
    head: u8,
    tail: u8,
    count: u8,
}

impl Rx {
    fn new(queue: Queue) -> Self {
        let buffers = vec![0u8; RX_BUFFERS * RX_BUFFER_BYTES].into_boxed_slice();
        let frames = vec![0u8; FRAME_SLOTS * MAX_FRAME_BYTES].into_boxed_slice();
        Rx {
            queue,
            buffers,
            frames,
            lens: [0u16; FRAME_SLOTS],
            head: 0,
            tail: 0,
            count: 0,
        }
    }

    fn buffer(&self, index: usize) -> usize {
        self.buffers.as_ptr() as usize + index * RX_BUFFER_BYTES
    }

    /// Post all receive buffers. Called once during bring-up.
    fn post_all(&mut self, base: usize) {
        for index in 0..RX_BUFFERS {
            write_desc(
                self.queue.desc,
                index as u16,
                self.buffer(index) as u64,
                RX_BUFFER_BYTES as u32,
                DESC_F_WRITE,
                0,
            );
        }
        for index in 0..RX_BUFFERS {
            self.queue.publish(base, QUEUE_RX, index as u16);
        }
    }

    /// Drain every completed receive buffer into the frame ring, re-posting the
    /// descriptor immediately so the device never runs out of buffers.
    fn drain(&mut self, base: usize) {
        while let Some((id, len)) = self.queue.take_used(base) {
            let index = id as usize;
            if index < RX_BUFFERS && (len as usize) > NET_HEADER_BYTES {
                let frame_len = (len as usize - NET_HEADER_BYTES).min(MAX_FRAME_BYTES);
                if self.count < FRAME_SLOTS as u8 {
                    let slot = self.tail as usize;
                    let base = slot * MAX_FRAME_BYTES;
                    let source = self.buffer(index) + NET_HEADER_BYTES;
                    // SAFETY: `source` points into this NIC's receive buffer
                    // `index`; `frame_len` is capped at the buffer's usable bytes
                    // and at `MAX_FRAME_BYTES`, and `base + frame_len` stays inside
                    // the flat frame ring (`FRAME_SLOTS * MAX_FRAME_BYTES`).
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            source as *const u8,
                            self.frames.as_mut_ptr().add(base),
                            frame_len,
                        );
                    }
                    self.lens[slot] = frame_len as u16;
                    self.tail = ((slot + 1) % FRAME_SLOTS) as u8;
                    self.count += 1;
                }
            }
            // Re-post the descriptor whether or not the frame was kept: the
            // device owns the buffer until it is published again.
            write_desc(
                self.queue.desc,
                index as u16,
                self.buffer(index) as u64,
                RX_BUFFER_BYTES as u32,
                DESC_F_WRITE,
                0,
            );
            self.queue.publish(base, QUEUE_RX, index as u16);
        }
    }

    /// Pop one frame; returns the number of bytes placed in `out`.
    fn pop(&mut self, out: &mut [u8]) -> Option<usize> {
        if self.count == 0 {
            return None;
        }
        let slot = self.head as usize;
        let base = slot * MAX_FRAME_BYTES;
        let len = (self.lens[slot] as usize).min(out.len());
        out[..len].copy_from_slice(&self.frames[base..base + len]);
        self.head = ((slot + 1) % FRAME_SLOTS) as u8;
        self.count -= 1;
        Some(len)
    }
}

/// Transmit side: a rotating set of buffers and the queue.
struct Tx {
    queue: Queue,
    /// `TX_BUFFERS` buffers of `NET_HEADER_BYTES + MAX_FRAME_BYTES`.
    buffers: Box<[u8]>,
    next: usize,
}

/// Bytes in one transmit buffer.
const TX_BUFFER_BYTES: usize = NET_HEADER_BYTES + MAX_FRAME_BYTES;

impl Tx {
    fn new(queue: Queue) -> Self {
        Tx {
            queue,
            buffers: vec![0u8; TX_BUFFERS * TX_BUFFER_BYTES].into_boxed_slice(),
            next: 0,
        }
    }

    fn buffer(&self, index: usize) -> usize {
        self.buffers.as_ptr() as usize + index * TX_BUFFER_BYTES
    }

    /// Copy `frame` behind a zeroed virtio-net header and submit it, waiting for
    /// completion.
    fn send(&mut self, base: usize, frame: &[u8]) -> Result<(), Error> {
        if frame.is_empty() || frame.len() > MAX_FRAME_BYTES {
            return Err(Error::Frame(frame.len()));
        }
        let slot = self.next;
        self.next = (self.next + 1) % TX_BUFFERS;
        let address = self.buffer(slot);
        // SAFETY: `address` is this driver's transmit buffer `slot`, sized
        // `NET_HEADER_BYTES + MAX_FRAME_BYTES`, and `frame` was length-checked
        // above. The regions cannot overlap (one is the caller's frame, the
        // other a heap allocation of this driver).
        unsafe {
            core::ptr::write_bytes(address as *mut u8, 0, NET_HEADER_BYTES);
            core::ptr::copy_nonoverlapping(
                frame.as_ptr(),
                (address + NET_HEADER_BYTES) as *mut u8,
                frame.len(),
            );
        }
        let total = NET_HEADER_BYTES + frame.len();
        write_desc(
            self.queue.desc,
            slot as u16,
            address as u64,
            total as u32,
            0,
            0,
        );
        self.queue.publish(base, QUEUE_TX, slot as u16);

        let mut spins = 0u32;
        while spins < POLL_LIMIT {
            if let Some((id, _len)) = self.queue.take_used(base) {
                if id as usize == slot {
                    return Ok(());
                }
                // A stale completion for another slot: keep waiting for ours.
            }
            spins = spins.wrapping_add(1);
            core::hint::spin_loop();
        }
        Err(Error::Timeout)
    }
}

/// One polled ethernet NIC.
pub struct VirtioNet {
    base: usize,
    mac: [u8; 6],
    rx: Spinlock<Rx>,
    tx: Spinlock<Tx>,
    failed: AtomicBool,
}

impl VirtioNet {
    /// The MAC address the device reports (or the local fallback).
    pub fn mac(&self) -> [u8; 6] {
        self.mac
    }

    /// Transmit one Ethernet frame (including its 14-byte header).
    ///
    /// Returns `false` when the frame is invalid, the device timed out, or an
    /// earlier timeout already marked it failed. The caller (the network layer)
    /// treats that as "the frame was dropped", which the protocols above are
    /// allowed to do; there is no retry here.
    pub fn send_frame(&self, frame: &[u8]) -> bool {
        if self.failed.load(Ordering::Relaxed) {
            return false;
        }
        match self.tx.lock().send(self.base, frame) {
            Ok(()) => true,
            Err(Error::Timeout) => {
                self.failed.store(true, Ordering::Relaxed);
                crate::kprintln!("virtio-net: transmit timed out; the NIC is marked FAILED");
                false
            }
            Err(_) => false,
        }
    }

    /// Receive one Ethernet frame into `out`; the driver first drains every
    /// completion the device published.
    pub fn recv_frame(&self, out: &mut [u8]) -> Option<usize> {
        let mut rx = self.rx.lock();
        rx.drain(self.base);
        rx.pop(out)
    }
}

/// The one NIC this kernel drives, if the machine has one.
static NIC: Spinlock<Option<Arc<VirtioNet>>> = Spinlock::new(None);

/// True when a network card was found and configured.
pub fn present() -> bool {
    NIC.lock().is_some()
}

/// The configured MAC address, if any.
pub fn mac() -> Option<[u8; 6]> {
    NIC.lock().as_ref().map(|nic| nic.mac())
}

/// Transmit a frame. `false` means the frame was not sent.
pub fn send(frame: &[u8]) -> bool {
    let nic = NIC.lock().as_ref().cloned();
    match nic {
        Some(nic) => nic.send_frame(frame),
        None => false,
    }
}

/// Receive a frame into `out`. `None` when no frame is pending.
pub fn recv(out: &mut [u8]) -> Option<usize> {
    let nic = NIC.lock().as_ref().cloned();
    match nic {
        Some(nic) => nic.recv_frame(out),
        None => None,
    }
}

/// Read the MAC address from the device configuration space.
///
/// The six bytes are not a 64-bit field, so the configuration-generation
/// counter is the only way the spec offers to know the read is consistent; retry
/// while it moves. A device that never settles is a device this driver does not
/// trust, but the MAC is cosmetic enough that the fallback is used instead of
/// refusing the NIC: the address is logged so a wrong one is visible.
fn config_mac(base: usize) -> [u8; 6] {
    for _ in 0..CONFIG_READ_ATTEMPTS {
        let before = read32(base, CONFIG_GENERATION);
        let mut mac = [0u8; 6];
        for (index, byte) in mac.iter_mut().enumerate() {
            *byte = read8(base, CONFIG + index);
        }
        let after = read32(base, CONFIG_GENERATION);
        if before == after {
            return mac;
        }
    }
    [0x52, 0x54, 0x00, 0x12, 0x34, 0x56]
}

/// Bring up one queue and program its rings into the device.
fn setup_queue(base: usize, index: u32) -> Result<Queue, Error> {
    write32(base, QUEUE_SEL, index);
    let max = read32(base, QUEUE_NUM_MAX);
    if max == 0 {
        return Err(Error::NoQueue(index, 0));
    }
    let max = max.min(u16::MAX as u32) as u16;
    if max < MIN_QUEUE_SIZE {
        return Err(Error::NoQueue(index, max));
    }
    let num = QUEUE_SIZE.min(max);
    write32(base, QUEUE_NUM, num as u32);

    let layout = ring_layout(num);
    let rings = vec![0u8; layout.total].into_boxed_slice();
    let storage = rings.as_ptr() as usize;
    if storage % DESC_BYTES != 0 {
        return Err(Error::Layout(storage));
    }
    let mut queue = Queue::new(num, rings);
    write64(base, QUEUE_DESC_LOW, queue.desc as u64);
    write64(base, QUEUE_DRIVER_LOW, queue.avail as u64);
    write64(base, QUEUE_DEVICE_LOW, queue.used as u64);
    // Polled driver: ask the device not to raise the queue interrupt.
    // SAFETY: `avail` is this queue's available ring inside its boxed storage;
    // its first field is the 16-bit flags word.
    unsafe {
        core::ptr::write_volatile(queue.avail as *mut u16, AVAIL_F_NO_INTERRUPT);
    }
    write32(base, QUEUE_READY, 1);
    if read32(base, QUEUE_READY) != 1 {
        return Err(Error::QueueNotReady);
    }
    // The device is allowed to write into the available ring's flags word; read
    // it once so the queue object is not optimized around the write above.
    let _ = read32(base, QUEUE_READY);
    queue.avail_idx = 0;
    queue.last_used = 0;
    Ok(queue)
}

/// Probe one transport window and bring up a NIC if the window holds one.
fn probe(base: usize) -> Result<VirtioNet, Error> {
    if read32(base, MAGIC_VALUE) != MAGIC {
        return Err(Error::NotVirtioMmio);
    }
    let version = read32(base, VERSION);
    if version != VERSION_MODERN {
        return Err(Error::Legacy(version));
    }
    let id = read32(base, DEVICE_ID);
    if id == 0 {
        return Err(Error::Empty);
    }
    if id != DEVICE_ID_NET {
        return Err(Error::NotNet(id));
    }

    write32(base, STATUS, 0);
    let reset = read32(base, STATUS);
    if reset != 0 {
        return Err(Error::Reset(reset));
    }
    write32(base, STATUS, STATUS_ACKNOWLEDGE);
    write32(base, STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);

    let offered = device_features(base);
    if offered & FEATURE_VERSION_1 == 0 {
        return Err(Error::NoVersion1);
    }
    if offered & FEATURE_NET_MRG_RXBUF == 0 {
        return Err(Error::NoMergeableRxBuffers);
    }
    let accepted = FEATURE_VERSION_1 | FEATURE_NET_MRG_RXBUF | (offered & FEATURE_NET_MAC);
    write_driver_features(base, accepted);

    let status = STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK;
    write32(base, STATUS, status);
    let status = read32(base, STATUS);
    if status & STATUS_FEATURES_OK == 0 {
        return Err(Error::FeatureRejected(status));
    }
    if status & STATUS_FAILED != 0 {
        return Err(Error::DeviceFailed(status));
    }

    let rx_queue = setup_queue(base, QUEUE_RX)?;
    let tx_queue = setup_queue(base, QUEUE_TX)?;
    let mut rx = Rx::new(rx_queue);
    rx.post_all(base);

    write32(
        base,
        STATUS,
        STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK,
    );
    let status = read32(base, STATUS);
    if status & STATUS_FAILED != 0 {
        return Err(Error::DeviceFailed(status));
    }

    let mac = if accepted & FEATURE_NET_MAC != 0 {
        config_mac(base)
    } else {
        [0x52, 0x54, 0x00, 0x12, 0x34, 0x56]
    };

    Ok(VirtioNet {
        base,
        mac,
        rx: Spinlock::new(rx),
        tx: Spinlock::new(Tx::new(tx_queue)),
        failed: AtomicBool::new(false),
    })
}

/// Probe every transport window the device tree reported and register the first
/// network card found. Absence is a log line, not an error: a machine without a
/// NIC is still a machine, and the shell keeps working.
pub fn init(tree: &Fdt) {
    let mut windows = [VirtioMmio::NONE; crate::fdt::MAX_VIRTIO_MMIO];
    let count = tree.virtio_mmio(&mut windows);
    for window in &windows[..count] {
        match probe(window.base) {
            Ok(nic) => {
                let mac = nic.mac;
                crate::kprintln!(
                    "virtio-net: virtio-net0 at {:#x}: MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                    window.base,
                    mac[0],
                    mac[1],
                    mac[2],
                    mac[3],
                    mac[4],
                    mac[5]
                );
                *NIC.lock() = Some(Arc::new(nic));
                return;
            }
            Err(Error::Empty) | Err(Error::NotNet(_)) | Err(Error::Legacy(_)) => {}
            Err(error) => {
                crate::kprintln!("virtio-net: {:#x}: {}", window.base, error);
            }
        }
    }
    crate::kprintln!("virtio-net: no device");
}

/// Forget the NIC (test-only: lets a selftest prove the absent path).
#[allow(dead_code)]
pub fn reset_for_test() {
    *NIC.lock() = None;
}
