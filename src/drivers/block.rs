// drivers/block.rs — block-device trait and the device registry
//
// The filesystem workstream needs a device-agnostic contract *before* a driver
// exists, and the driver workstream needs a registry to publish into. This is
// that seam: a 512-byte-sector trait plus one global slot, both hostile to the
// two easy mistakes — a driver that reports success on a short read, and a
// caller that assumes a device is present.
//
// In-memory `MemBlock` is part of the seam on purpose: the ext2 port and its
// selftests must be runnable before virtio-mmio is on the machine, and an fs
// test that only ever sees QEMU's disk image cannot be run twice in one boot
// with different contents.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use crate::sync::Spinlock;

/// Size of one addressable block, in bytes.
pub const SECTOR_SIZE: usize = 512;

/// A block device: fixed-size sectors, no byte-granular access.
///
/// `read_block`/`write_block` take a block number and a buffer whose length is
/// a whole number of sectors; they return the number of bytes transferred on
/// success. A short transfer is an error (`Err`) — the caller must never treat
/// a partial read as EOF, because ext2's metadata is all-or-nothing.
pub trait BlockDevice: Send + Sync {
    fn name(&self) -> &str;

    /// Read whole sectors starting at `block`. Returns the byte count read.
    fn read_block(&self, block: u64, buf: &mut [u8]) -> Result<usize, ()>;

    /// Write whole sectors starting at `block`. Returns the byte count written.
    fn write_block(&self, block: u64, buf: &[u8]) -> Result<usize, ()>;

    /// Addressable sectors, or 0 when the device cannot report capacity.
    fn sector_count(&self) -> u64 {
        0
    }

    /// Flush the device's volatile write cache, when it has one.
    fn flush(&self) -> Result<(), ()> {
        Ok(())
    }
}

/// The one block device the kernel knows about.
///
/// Single slot, like the console: the boot path registers the device it found
/// and the filesystem mounts it; a second disk is not part of M5 and adding a
/// map here would pretend it is.
static BLOCK: Spinlock<Option<Arc<dyn BlockDevice>>> = Spinlock::new(None);

/// Publish `dev` as the system block device, replacing any previous one.
pub fn register(dev: Arc<dyn BlockDevice>) {
    *BLOCK.lock() = Some(dev);
}

/// The registered block device, if any.
pub fn get() -> Option<Arc<dyn BlockDevice>> {
    BLOCK.lock().as_ref().map(Arc::clone)
}

/// A RAM-backed block device for tests.
///
/// Contents start as `0x00`; `with_pattern` fills them instead so a test can
/// prove it read the *device's* bytes and not a zeroed local buffer.
pub struct MemBlock {
    name: &'static str,
    bytes: Spinlock<Vec<u8>>,
}

impl MemBlock {
    /// Create a device of `sectors` 512-byte sectors, zero-filled.
    pub fn new(name: &'static str, sectors: u64) -> Self {
        Self {
            name,
            bytes: Spinlock::new(vec![0u8; sectors as usize * SECTOR_SIZE]),
        }
    }

    /// Create a device whose every byte is `pattern` (so reads are checkable).
    pub fn with_pattern(name: &'static str, sectors: u64, pattern: u8) -> Self {
        Self {
            name,
            bytes: Spinlock::new(vec![pattern; sectors as usize * SECTOR_SIZE]),
        }
    }
}

impl BlockDevice for MemBlock {
    fn name(&self) -> &str {
        self.name
    }

    fn read_block(&self, block: u64, buf: &mut [u8]) -> Result<usize, ()> {
        if buf.is_empty() || buf.len() % SECTOR_SIZE != 0 {
            return Err(());
        }
        let bytes = self.bytes.lock();
        let start = block as usize * SECTOR_SIZE;
        let end = start.checked_add(buf.len()).ok_or(())?;
        if end > bytes.len() {
            return Err(());
        }
        buf.copy_from_slice(&bytes[start..end]);
        Ok(buf.len())
    }

    fn write_block(&self, block: u64, buf: &[u8]) -> Result<usize, ()> {
        if buf.is_empty() || buf.len() % SECTOR_SIZE != 0 {
            return Err(());
        }
        let mut bytes = self.bytes.lock();
        let start = block as usize * SECTOR_SIZE;
        let end = start.checked_add(buf.len()).ok_or(())?;
        if end > bytes.len() {
            return Err(());
        }
        bytes[start..end].copy_from_slice(buf);
        Ok(buf.len())
    }

    fn sector_count(&self) -> u64 {
        (self.bytes.lock().len() / SECTOR_SIZE) as u64
    }
}
