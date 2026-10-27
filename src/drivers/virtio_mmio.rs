//! The virtio-mmio block device: modern virtio 1.x (transport version 2), polled.
//!
//! QEMU `virt` puts a virtio-mmio transport in each of the eight register
//! windows its device tree declares (`virtio,mmio` nodes at
//! `0x1000_1000 … 0x1000_8000`), and `-device virtio-blk-device,drive=…` plugs a
//! block device into the first free one. This module drives that device: it
//! probes each window [`Fdt::virtio_mmio`](crate::fdt::Fdt::virtio_mmio)
//! enumerated, negotiates features, brings up exactly one virtqueue, and
//! publishes the disk through [`block::register`](crate::drivers::block::register)
//! — so the shell's `disk` command and the filesystem above the seam know only
//! "a block device", not "virtio".
//!
//! The choices that shape the code, each with the reason it is not the
//! alternative:
//!
//! * **Modern (version 2) only.** A legacy transport (version 1) has a different
//!   register layout and different feature/queue semantics; the driver says so on
//!   the serial line and refuses the window rather than half-driving it.
//! * **Polled completion.** The kernel has no PLIC and no timer yet
//!   (`docs/architecture.md` §10), so no queue interrupt is enabled — the
//!   available ring's `NO_INTERRUPT` flag is set and the driver spins on the used
//!   ring. A wedged device would therefore hang the caller; the poll is bounded by
//!   [`POLL_LIMIT`] iterations, which is a liveness heuristic, not a deadline
//!   (without a timer there is no deadline to state). A timeout marks the device
//!   FAILED and refuses later requests, because the queue's two indices can no
//!   longer be trusted.
//! * **Split virtqueue, direct descriptors.** Indirect descriptors are an
//!   optional feature this driver does not negotiate; a request is at most three
//!   descriptors, so there is nothing to save. The used ring's `len` must match
//!   what the device was asked to write — a short transfer is an error, never a
//!   short success (the block seam's contract).
//! * **One queue, one request in flight.** The filesystem path is synchronous and
//!   the kernel has no scheduler, so a second queue or multiple outstanding
//!   requests would be unexercised state. `QueueNum` is a fixed small power of two
//!   (capped by the device), just enough for a three-descriptor chain.
//! * **The identity map is load-bearing.** Every address given to the device —
//!   the queue rings, the request header, the caller's data buffer — is the
//!   Rust pointer's value, because `memory::paging` maps physical RAM at its
//!   physical address. The same assumption is documented in `fw_cfg.rs` and
//!   `ramfb.rs`; a non-identity kernel/user split would have to translate here.
//! * **Ring storage from the heap.** The driver initializes after `memory::init`,
//!   so the allocator exists; a heap block gives the one property the device needs
//!   — a stable address for as long as the `Box` lives — and every block the
//!   allocator hands out is 16-byte aligned, which is exactly the descriptor
//!   table's requirement. The code checks that alignment instead of trusting a
//!   promise two layers away, because a misaligned table is silent corruption,
//!   not a fault.
//!
//! What this driver deliberately does not do is in `src/drivers/README.md`
//! (interrupts, indirect descriptors, multiple queues, packed virtqueues, legacy
//! transports, cache maintenance — the last is a real-hardware concern that QEMU
//! does not model).

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec;
use core::fmt;
use core::sync::atomic::{fence, AtomicBool, Ordering};

use crate::drivers::block::{self, BlockDevice, SECTOR_SIZE};
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
/// Start of the device-specific configuration space; for `virtio-blk` its first
/// field is `capacity`, a 64-bit little-endian count of 512-byte sectors.
const CONFIG: usize = 0x100;

/// `"virt"` as the little-endian magic-value register spells it.
const MAGIC: u32 = 0x7472_6976;
/// The transport version this driver implements (virtio 1.x, non-transitional).
const VERSION_MODERN: u32 = 2;
/// Virtio device ID of a block device.
const DEVICE_ID_BLOCK: u32 = 2;

// Device status bits (§2.1).
const STATUS_ACKNOWLEDGE: u32 = 1 << 0;
const STATUS_DRIVER: u32 = 1 << 1;
const STATUS_DRIVER_OK: u32 = 1 << 2;
const STATUS_FEATURES_OK: u32 = 1 << 3;
const STATUS_FAILED: u32 = 1 << 7;

// Feature bits, in the 64-bit feature space: selector 0 holds bits 0..31 and
// selector 1 holds bits 32..63.
const FEATURE_BLK_FLUSH: u64 = 1 << 9;
const FEATURE_VERSION_1: u64 = 1 << 32;

// Request types (`virtio_blk_req.type`, §5.2.6).
const BLK_T_IN: u32 = 0;
const BLK_T_OUT: u32 = 1;
const BLK_T_FLUSH: u32 = 4;
/// `virtio_blk_req_status`: the only value that means success.
const BLK_S_OK: u8 = 0;

// Descriptor flags (§2.6.5).
const DESC_F_NEXT: u16 = 1;
const DESC_F_WRITE: u16 = 2;

/// The available ring's "do not interrupt me" flag (§2.6.7).
const AVAIL_F_NO_INTERRUPT: u16 = 1;

// Virtqueue geometry (§2.6). The sizes are the spec's, not this driver's:
// descriptor table 16 bytes per entry, available ring `6 + 2*size`, used ring
// `6 + 8*size`; the fixed parts are the two header halves.
const DESC_BYTES: usize = 16;
const AVAIL_FIXED: usize = 4; // flags (u16) + idx (u16)
const AVAIL_TRAILER: usize = 2; // used_event (u16, meaningful only with EVENT_IDX)
const USED_FIXED: usize = 4; // flags (u16) + idx (u16)
const USED_TRAILER: usize = 2; // avail_event (u16, meaningful only with EVENT_IDX)

/// Bytes of a `virtio_blk_req` header: `type`, `reserved`, `sector`.
const HEADER_BYTES: usize = 16;

/// Queue 0 is the block device's only request queue (§5.2.2).
const QUEUE_INDEX: u32 = 0;
/// Descriptors this driver brings up: a small power of two, enough for the
/// three-descriptor chain and more than enough for one request in flight.
const QUEUE_SIZE: u16 = 8;
/// Smallest queue this driver accepts: a 3-descriptor chain needs 4 slots
/// (queue sizes are powers of two).
const MIN_QUEUE_SIZE: u16 = 4;

/// Polls before a request is declared "the device is not answering".
///
/// This is not a timeout in seconds — the kernel has no timer — but it turns a
/// wedged device into an error line instead of a silent hang, which is the most
/// a timerless polled driver can promise.
const POLL_LIMIT: u32 = 1 << 24;

/// Attempts to get a consistent `capacity` read through the configuration
/// generation counter before giving up.
const CONFIG_READ_ATTEMPTS: u32 = 16;

/// 512-byte sectors per MiB — capacity is reported in sectors, the log in MiB.
const SECTORS_PER_MIB: u64 = (1024 * 1024) / SECTOR_SIZE as u64;

/// The name this device publishes at the block seam.
const NAME: &str = "virtio-blk0";

/// Why a transport window could not be used, or a request failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The window does not answer with the virtio-mmio magic word.
    NotVirtioMmio,
    /// A legacy (version 1) transport; this driver speaks version 2 only.
    Legacy(u32),
    /// An empty transport slot: `DeviceID` reads 0 (QEMU `virt` reports all
    /// eight windows even when no device is plugged into them).
    Empty,
    /// The transport holds some other virtio device.
    NotBlock(u32),
    /// `VIRTIO_F_VERSION_1` was not offered.
    NoVersion1,
    /// `VIRTIO_BLK_F_FLUSH` was not offered, so `flush` could not be honest.
    NoFlush,
    /// The device did not accept the feature set (no `FEATURES_OK`).
    FeatureRejected(u32),
    /// The device would not come back to status 0 for a reset.
    Reset(u32),
    /// The device reported `FAILED` during initialization.
    DeviceFailed(u32),
    /// The device exposes no queue 0.
    NoQueue,
    /// Queue 0 is too small for a three-descriptor request.
    QueueTooSmall(u16),
    /// `QueueReady` did not read back as 1.
    QueueNotReady,
    /// The heap returned ring storage with an address this driver cannot give
    /// the device (the descriptor table must be 16-byte aligned).
    Layout(usize),
    /// The configuration-generation counter kept moving under the capacity read.
    UnstableConfig,
    /// The device reports zero capacity — nothing to mount.
    NoCapacity,
    /// The request was not completed within [`POLL_LIMIT`] polls.
    Timeout,
    /// An earlier timeout marked the device FAILED; the queue is not reusable.
    Failed,
    /// The used ring named a descriptor this driver never published.
    Protocol,
    /// The device wrote a different number of bytes than the request asked for.
    Short {
        /// Bytes the writable part of the chain can take.
        expected: u32,
        /// Bytes the device says it wrote.
        actual: u32,
    },
    /// The per-request status byte was not OK (1 = I/O error, 2 = unsupported).
    Io(u8),
    /// A buffer that is empty, not whole sectors, or too large for one
    /// descriptor's 32-bit length.
    Buffer(usize),
    /// A request outside the device's addressable sectors.
    Range {
        /// First sector requested.
        block: u64,
        /// Sectors requested.
        sectors: u64,
        /// Sectors the device has.
        capacity: u64,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotVirtioMmio => write!(f, "no virtio-mmio magic word"),
            Error::Legacy(version) => {
                write!(
                    f,
                    "legacy virtio-mmio version {version} (this driver speaks 2)"
                )
            }
            Error::Empty => write!(f, "empty transport slot"),
            Error::NotBlock(id) => write!(f, "virtio device id {id} is not a block device"),
            Error::NoVersion1 => write!(f, "the device does not offer VIRTIO_F_VERSION_1"),
            Error::NoFlush => write!(f, "the device does not offer VIRTIO_BLK_F_FLUSH"),
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
            Error::Layout(address) => {
                write!(f, "ring storage at {address:#x} is not 16-byte aligned")
            }
            Error::UnstableConfig => write!(f, "the configuration space never read consistently"),
            Error::NoCapacity => write!(f, "the device reports zero capacity"),
            Error::Timeout => write!(f, "the device did not complete the request"),
            Error::Failed => write!(f, "the device is marked FAILED after an earlier timeout"),
            Error::Protocol => write!(f, "the used ring named an unpublised descriptor"),
            Error::Short { expected, actual } => {
                write!(f, "short transfer: {actual} of {expected} bytes")
            }
            Error::Io(status) => write!(f, "the device reported status {status}"),
            Error::Buffer(len) => {
                write!(f, "{len} bytes is not a non-empty, whole-sector buffer")
            }
            Error::Range {
                block,
                sectors,
                capacity,
            } => write!(
                f,
                "sectors {block}..{} are outside the {capacity}-sector device",
                block.saturating_add(*sectors)
            ),
        }
    }
}

/// Read one 32-bit virtio-mmio register.
#[inline]
fn read32(base: usize, offset: usize) -> u32 {
    // SAFETY: `base` is a virtio-mmio transport window the device tree reported
    // and `main` mapped as device memory for the whole run; `offset` is one of
    // the spec's 32-bit registers, so the access is aligned inside the window
    // and touches no RAM.
    unsafe { ((base + offset) as *const u32).read_volatile() }
}

/// Write one 32-bit virtio-mmio register.
#[inline]
fn write32(base: usize, offset: usize, value: u32) {
    // SAFETY: as `read32` — a single aligned 32-bit store to a spec register.
    unsafe { ((base + offset) as *mut u32).write_volatile(value) }
}

/// Write the low/high pair of a 64-bit MMIO register.
fn write64(base: usize, offset: usize, value: u64) {
    write32(base, offset, value as u32);
    write32(base, offset + 4, (value >> 32) as u32);
}

/// The feature set the device offers, as one 64-bit word.
///
/// The two halves are read through their selector; the selector is left on 0 so
/// the driver-feature writes below start from the same place the spec's
/// initialization sequence describes.
fn device_features(base: usize) -> u64 {
    write32(base, DEVICE_FEATURES_SEL, 0);
    let low = read32(base, DEVICE_FEATURES) as u64;
    write32(base, DEVICE_FEATURES_SEL, 1);
    let high = read32(base, DEVICE_FEATURES) as u64;
    write32(base, DEVICE_FEATURES_SEL, 0);
    low | (high << 32)
}

/// Tell the device which features this driver accepts.
fn write_driver_features(base: usize, features: u64) {
    write32(base, DRIVER_FEATURES_SEL, 0);
    write32(base, DRIVER_FEATURES, features as u32);
    write32(base, DRIVER_FEATURES_SEL, 1);
    write32(base, DRIVER_FEATURES, (features >> 32) as u32);
}

/// Where the three parts of a split virtqueue sit inside one allocation.
///
/// `desc` is 16-byte aligned (the allocation starts 16-byte aligned and is the
/// first part), `avail` follows the descriptor table and stays 2-byte aligned,
/// and `used` is rounded up to its 4-byte requirement — the only part that ever
/// needs padding.
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
        // Round the whole allocation to the descriptor table's alignment unit,
        // so a second queue added later would start on a legal boundary too.
        total: (total + DESC_BYTES - 1) & !(DESC_BYTES - 1),
    }
}

/// Write one descriptor (16 bytes, every field little-endian).
///
/// Volatile: the device reads this memory, and a plain store to memory the
/// compiler cannot see a reader for may be elided. Ordering with the available
/// ring is [`Queue::submit`]'s `fence(Release)`.
fn write_desc(table: usize, index: u16, address: u64, len: u32, flags: u16, next: u16) {
    let at = table + index as usize * DESC_BYTES;
    // SAFETY: `table` is this queue's descriptor table (inside its heap-backed
    // ring storage), `index < num` is the caller's contract, and each field is
    // written at its spec offset with its spec width; the table is 16-byte
    // aligned, so the 64-bit store is aligned.
    unsafe {
        core::ptr::write_volatile(at as *mut u64, address.to_le());
        core::ptr::write_volatile((at + 8) as *mut u32, len.to_le());
        core::ptr::write_volatile((at + 12) as *mut u16, flags.to_le());
        core::ptr::write_volatile((at + 14) as *mut u16, next.to_le());
    }
}

/// One request's buffers, as addresses the device may use.
struct Request {
    /// The 16-byte `virtio_blk_req` header (device-readable).
    header: usize,
    /// The data buffer; ignored when `data_len == 0` (a flush request).
    data: usize,
    data_len: usize,
    /// Whether the device writes the data buffer (`VIRTIO_BLK_T_IN`).
    data_writable: bool,
    /// The one-byte status (device-writable).
    status: usize,
}

/// The driver's half of the virtqueue: the rings, plus the two indices the
/// split-virtqueue protocol keeps on this side.
struct Queue {
    num: u16,
    /// Ring storage. The `Box` gives the device a stable physical address for as
    /// long as the driver lives; `desc`/`avail`/`used` point inside it.
    #[allow(dead_code)] // kept for ownership and address stability, not reads
    rings: Box<[u8]>,
    desc: usize,
    avail: usize,
    used: usize,
    /// Next index this driver publishes in the available ring.
    avail_idx: u16,
    /// Last used-ring index this driver has consumed.
    last_used: u16,
}

impl Queue {
    /// Program the queue's descriptors into the device and hand it the rings.
    ///
    /// `rings` must be at least [`ring_layout`]'s `total` bytes and 16-byte
    /// aligned; the caller has already checked both.
    fn new(num: u16, rings: Box<[u8]>) -> Self {
        let storage = rings.as_ptr() as usize;
        let layout = ring_layout(num);
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

    /// Hand one request to the device and poll until it is used.
    ///
    /// The chain is `header (device reads) → data (reads or writes) → status
    /// (device writes)`; a flush has no data descriptor, so its chain is two
    /// descriptors — the protocol carries no data for it.
    fn submit(&mut self, base: usize, request: Request) -> Result<(), Error> {
        // The writable part of the chain: the data buffer (read requests only)
        // plus the status byte. The used ring's `len` counts exactly these.
        let expected = 1 + if request.data_writable {
            request.data_len as u32
        } else {
            0
        };

        let mut next = 1u16;
        write_desc(
            self.desc,
            0,
            request.header as u64,
            HEADER_BYTES as u32,
            DESC_F_NEXT,
            1,
        );
        if request.data_len != 0 {
            let flags = if request.data_writable {
                DESC_F_NEXT | DESC_F_WRITE
            } else {
                DESC_F_NEXT
            };
            write_desc(
                self.desc,
                1,
                request.data as u64,
                request.data_len as u32,
                flags,
                2,
            );
            next = 2;
        }
        write_desc(self.desc, next, request.status as u64, 1, DESC_F_WRITE, 0);

        // Publish descriptor 0 in the available ring. The ring entry must be
        // written before the index that makes it visible: the release fence
        // between them is what the spec's §2.6.13 "memory barriers" are.
        let slot = self.avail_idx % self.num;
        // SAFETY: `avail` is this queue's available ring (inside `rings`), the
        // `slot` is in `0..num`, and the u16 field is 2-byte aligned by the
        // layout above.
        unsafe {
            core::ptr::write_volatile(
                (self.avail + AVAIL_FIXED + 2 * slot as usize) as *mut u16,
                0u16.to_le(),
            );
        }
        fence(Ordering::Release);
        self.avail_idx = self.avail_idx.wrapping_add(1);
        // SAFETY: as above, the ring's `idx` field.
        unsafe {
            core::ptr::write_volatile((self.avail + 2) as *mut u16, self.avail_idx.to_le());
        }
        // The index must be visible before the device is told to look at it.
        fence(Ordering::Release);
        write32(base, QUEUE_NOTIFY, QUEUE_INDEX);

        self.wait_used(base, expected)
    }

    /// Poll the used ring until the device reports the request completed.
    fn wait_used(&mut self, base: usize, expected: u32) -> Result<(), Error> {
        let mut spins = 0u32;
        loop {
            // SAFETY: `used` is this queue's used ring inside its ring storage;
            // `idx` is a 16-bit field the device writes and this read observes.
            let used_idx =
                u16::from_le(unsafe { core::ptr::read_volatile((self.used + 2) as *const u16) });
            if used_idx != self.last_used {
                // The index moved, so the entry for `last_used` (and the status
                // byte the caller will read) is complete; the acquire fence
                // publishes those writes to this hart.
                fence(Ordering::Acquire);
                let slot = self.last_used % self.num;
                // SAFETY: `slot` is in `0..num` and both fields are 4-byte
                // aligned entries of the used ring, which the device wrote
                // before advancing `idx` (observed just above).
                let (id, len) = unsafe {
                    let at = self.used + USED_FIXED + 8 * slot as usize;
                    (
                        u32::from_le(core::ptr::read_volatile(at as *const u32)),
                        u32::from_le(core::ptr::read_volatile((at + 4) as *const u32)),
                    )
                };
                self.last_used = used_idx;

                // No interrupt is delivered today (no PLIC is wired), but leaving
                // the transport's interrupt status set would be a trap for the
                // first interrupt-driven successor: acknowledge it here.
                let interrupt = read32(base, INTERRUPT_STATUS);
                if interrupt != 0 {
                    write32(base, INTERRUPT_ACK, interrupt);
                }

                if id != 0 {
                    // Only descriptor 0 is ever published as a chain head.
                    return Err(Error::Protocol);
                }
                if len != expected {
                    return Err(Error::Short {
                        expected,
                        actual: len,
                    });
                }
                return Ok(());
            }
            spins += 1;
            if spins == POLL_LIMIT {
                return Err(Error::Timeout);
            }
            core::hint::spin_loop();
        }
    }
}

/// A probed virtio-blk device, published at the [`BlockDevice`] seam.
pub struct VirtioBlk {
    base: usize,
    capacity: u64,
    queue: Spinlock<Queue>,
    /// Set when a request timed out: the queue's indices may now disagree with
    /// the device's, so no later request may reuse the queue.
    failed: AtomicBool,
}

impl VirtioBlk {
    /// Whole sectors, inside the device: the block seam's contract, checked once.
    fn check_buffer(&self, block: u64, bytes: usize) -> Result<(), Error> {
        if bytes == 0 || bytes % SECTOR_SIZE != 0 || bytes > u32::MAX as usize {
            return Err(Error::Buffer(bytes));
        }
        let sectors = (bytes / SECTOR_SIZE) as u64;
        let end = block.checked_add(sectors).ok_or(Error::Range {
            block,
            sectors,
            capacity: self.capacity,
        })?;
        if end > self.capacity {
            return Err(Error::Range {
                block,
                sectors,
                capacity: self.capacity,
            });
        }
        Ok(())
    }

    /// Issue one request and wait for it.
    ///
    /// `data` is the buffer's device-visible address (the pointer itself, under
    /// the identity map) and `data_writable` says whether the device writes it.
    /// The header and status live in this stack frame: the driver polls until the
    /// device has used the chain, so the device is done with them before this
    /// returns. An interrupt-driven version would have to give them a longer life.
    fn transfer(
        &self,
        kind: u32,
        sector: u64,
        data: usize,
        data_len: usize,
        data_writable: bool,
    ) -> Result<(), Error> {
        if self.failed.load(Ordering::Relaxed) {
            return Err(Error::Failed);
        }
        let mut header = [0u8; HEADER_BYTES];
        header[0..4].copy_from_slice(&kind.to_le_bytes());
        header[4..8].copy_from_slice(&0u32.to_le_bytes()); // reserved, must be zero
        header[8..16].copy_from_slice(&sector.to_le_bytes());
        // 0xff is not a valid `virtio_blk_req_status`: a device that forgets to
        // write the byte then shows up as an I/O error, not as a silent success.
        let mut status = 0xffu8;

        let request = Request {
            header: header.as_ptr() as usize,
            data,
            data_len,
            data_writable,
            status: core::ptr::addr_of_mut!(status) as usize,
        };
        let mut queue = self.queue.lock();
        if let Err(error) = queue.submit(self.base, request) {
            if error == Error::Timeout {
                self.failed.store(true, Ordering::Relaxed);
                write32(self.base, STATUS, read32(self.base, STATUS) | STATUS_FAILED);
            }
            return Err(error);
        }
        // SAFETY: `status` is this frame's byte and the device wrote it before
        // advancing the used index observed in `submit`; the acquire fence there
        // is what makes this read see the device's write.
        let status = unsafe { core::ptr::read_volatile(core::ptr::addr_of!(status)) };
        if status == BLK_S_OK {
            Ok(())
        } else {
            Err(Error::Io(status))
        }
    }
}

impl BlockDevice for VirtioBlk {
    fn name(&self) -> &str {
        NAME
    }

    fn read_block(&self, block: u64, buf: &mut [u8]) -> Result<usize, ()> {
        let len = buf.len();
        self.check_buffer(block, len)
            .map_err(|error| self.report(error))?;
        self.transfer(BLK_T_IN, block, buf.as_mut_ptr() as usize, len, true)
            .map_err(|error| self.report(error))?;
        Ok(len)
    }

    fn write_block(&self, block: u64, buf: &[u8]) -> Result<usize, ()> {
        let len = buf.len();
        self.check_buffer(block, len)
            .map_err(|error| self.report(error))?;
        self.transfer(BLK_T_OUT, block, buf.as_ptr() as usize, len, false)
            .map_err(|error| self.report(error))?;
        Ok(len)
    }

    fn sector_count(&self) -> u64 {
        self.capacity
    }

    fn flush(&self) -> Result<(), ()> {
        // A flush carries no data: the header names the request type and the
        // status byte reports the outcome.
        self.transfer(BLK_T_FLUSH, 0, 0, 0, false)
            .map_err(|error| self.report(error))
    }
}

impl VirtioBlk {
    /// Log a failed request and hand the block seam its reason-less `()`.
    ///
    /// Every failure prints: the trait cannot carry a reason, and a filesystem
    /// that refuses to mount must be explainable from the serial log alone.
    fn report(&self, error: Error) {
        crate::kprintln!("virtio-blk: request failed: {}", error);
    }
}

/// Probe one transport window.
///
/// The checks are in the order the protocol makes meaningful: identify the
/// transport (magic), the device (a block device — an *empty* slot has id 0 and
/// is the normal state of the seven transports QEMU declares but does not use),
/// the version (modern only), then features, then the queue. Empty-before-version
/// matters for the log: QEMU's legacy transports report version 1 even with no
/// device plugged in, and eight "legacy transport" warnings for eight empty
/// slots on every boot is noise that hides a real one (a disk attached without
/// `-global virtio-mmio.force-legacy=false`).
fn probe(base: usize) -> Result<VirtioBlk, Error> {
    if read32(base, MAGIC_VALUE) != MAGIC {
        return Err(Error::NotVirtioMmio);
    }
    let device_id = read32(base, DEVICE_ID);
    if device_id == 0 {
        return Err(Error::Empty);
    }
    if device_id != DEVICE_ID_BLOCK {
        return Err(Error::NotBlock(device_id));
    }
    let version = read32(base, VERSION);
    if version != VERSION_MODERN {
        return Err(Error::Legacy(version));
    }

    // Reset, then the initialization sequence of §3.1: ACKNOWLEDGE, DRIVER,
    // features, FEATURES_OK (the device may refuse, which is what the read-back
    // checks), queue setup, DRIVER_OK last. `FAILED` is never cleared by the
    // driver, so it is checked as a refusal at every step that can set it.
    write32(base, STATUS, 0);
    let reset = read32(base, STATUS);
    if reset != 0 {
        return Err(Error::Reset(reset));
    }
    write32(base, STATUS, STATUS_ACKNOWLEDGE);
    write32(base, STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);

    let offered = device_features(base);
    // Both of these are hard requirements rather than niceties. Version 1 says
    // the device is non-transitional modern virtio; without it the register
    // layout and queue semantics this driver was written against do not hold.
    // Flush is what makes a write durable, and the block seam promises a flush;
    // accepting a device that cannot flush would make that promise a lie.
    if offered & FEATURE_VERSION_1 == 0 {
        return Err(Error::NoVersion1);
    }
    if offered & FEATURE_BLK_FLUSH == 0 {
        return Err(Error::NoFlush);
    }
    let accepted = FEATURE_VERSION_1 | FEATURE_BLK_FLUSH;
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

    let queue = setup_queue(base)?;

    write32(
        base,
        STATUS,
        STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK,
    );
    let status = read32(base, STATUS);
    if status & STATUS_FAILED != 0 {
        return Err(Error::DeviceFailed(status));
    }

    let capacity = capacity(base)?;
    if capacity == 0 {
        return Err(Error::NoCapacity);
    }

    Ok(VirtioBlk {
        base,
        capacity,
        queue: Spinlock::new(queue),
        failed: AtomicBool::new(false),
    })
}

/// Bring up queue 0 and program its rings into the device.
fn setup_queue(base: usize) -> Result<Queue, Error> {
    write32(base, QUEUE_SEL, QUEUE_INDEX);
    let max = read32(base, QUEUE_NUM_MAX);
    if max == 0 {
        return Err(Error::NoQueue);
    }
    let max = max.min(u16::MAX as u32) as u16;
    if max < MIN_QUEUE_SIZE {
        return Err(Error::QueueTooSmall(max));
    }
    let num = QUEUE_SIZE.min(max);
    write32(base, QUEUE_NUM, num as u32);

    let layout = ring_layout(num);
    let rings = vec![0u8; layout.total].into_boxed_slice();
    let storage = rings.as_ptr() as usize;
    // The allocator promises 16-byte alignment; check it here rather than trust a
    // promise made two modules away, because a misaligned descriptor table is
    // silent corruption rather than a fault.
    if storage % DESC_BYTES != 0 {
        return Err(Error::Layout(storage));
    }
    let queue = Queue::new(num, rings);

    // The queue addresses are physical addresses. That is the identity map's
    // promise (see the module comment): the pointer's value is what the device
    // must be given.
    write64(base, QUEUE_DESC_LOW, queue.desc as u64);
    write64(base, QUEUE_DRIVER_LOW, queue.avail as u64);
    write64(base, QUEUE_DEVICE_LOW, queue.used as u64);
    // A polled driver asks the device not to raise the queue interrupt at all.
    // SAFETY: `avail` is this queue's available ring inside its boxed storage,
    // and its first field is the 16-bit flags word (2-byte aligned by layout).
    unsafe {
        core::ptr::write_volatile(queue.avail as *mut u16, AVAIL_F_NO_INTERRUPT.to_le());
    }
    write32(base, QUEUE_READY, 1);
    if read32(base, QUEUE_READY) != 1 {
        return Err(Error::QueueNotReady);
    }
    Ok(queue)
}

/// The block device's `capacity`: addressable 512-byte sectors.
///
/// The field is 64-bit little-endian, and the virtio spec says a driver must not
/// assume a configuration read wider than 32 bits is atomic. The transport's
/// answer is the configuration-generation counter (§2.4.1): read it before and
/// after the field, and retry while it moves. Two 32-bit reads assemble the value,
/// which additionally keeps this working on a transport that implements only
/// 32-bit configuration accesses.
fn capacity(base: usize) -> Result<u64, Error> {
    for _ in 0..CONFIG_READ_ATTEMPTS {
        let before = read32(base, CONFIG_GENERATION);
        let low = read32(base, CONFIG) as u64;
        let high = read32(base, CONFIG + 4) as u64;
        let after = read32(base, CONFIG_GENERATION);
        if before == after {
            return Ok(low | (high << 32));
        }
    }
    Err(Error::UnstableConfig)
}

/// Probe every transport window the device tree reported, register the first
/// block device found, and say on the serial line what happened.
///
/// Absence is not an error: a headless CI boot has no disk attached, and the
/// kernel is still a kernel without one. A present-but-unusable device is logged
/// with its reason and skipped; if nothing usable remains the absent line is
/// printed too, so the log answers "why is there no disk?" in one place.
pub fn init(tree: &Fdt) {
    let mut windows = [VirtioMmio::NONE; crate::fdt::MAX_VIRTIO_MMIO];
    let count = tree.virtio_mmio(&mut windows);
    let mut registered = false;

    for window in &windows[..count] {
        match probe(window.base) {
            Ok(device) => {
                crate::kprintln!(
                    "virtio-blk: {} at {:#x}: {} sectors ({} MiB)",
                    NAME,
                    window.base,
                    device.capacity,
                    device.capacity / SECTORS_PER_MIB
                );
                block::register(Arc::new(device));
                registered = true;
                // The block seam has one slot (see `block.rs`): the first usable
                // device is the system disk, and pretending to support a second
                // one would be a map the filesystem has no way to use.
                break;
            }
            // An empty slot is the normal case on a machine with fewer devices
            // than transports; QEMU `virt` always reports all eight.
            Err(Error::Empty) => {}
            Err(error) => {
                crate::kprintln!("virtio-mmio: {:#x}: {}", window.base, error);
            }
        }
    }

    if !registered {
        crate::kprintln!("virtio-blk: no device");
    }
}
