//! QEMU's `fw_cfg` device: the transport for the machine's named blobs.
//!
//! The device tree is how a RISC-V machine says *where* its devices are; fw_cfg
//! is how QEMU hands the guest the small named blobs a device tree cannot carry.
//! The one this kernel needs is `etc/ramfb`, the ramfb device's configuration
//! item ([`crate::drivers::ramfb`]) — the port of the x86 kernel's Limine
//! framebuffer response, which on x86 the bootloader filled in for us.
//!
//! Both interfaces are used on purpose:
//!
//! * the **selector/data** registers, a byte at a time, to identify the device
//!   (`QEMU` signature) and to read the file-directory header — a byte read has
//!   no endianness at all, while a wide data-register read is
//!   "string-preserving", i.e. its numeric value depends on the guest's byte
//!   order and on QEMU's convention for assembling it. The bytes are then
//!   assembled in the order each item is *documented* in, which is not the same
//!   order for all of them: the feature bitmap is little-endian, the file
//!   directory big-endian;
//! * the **DMA** interface (QEMU ≥ 2.9; `virt` exposes it — the
//!   `fw-cfg@10100000` node has a 0x18-byte region and a `dma-coherent`
//!   property) to move the directory and the ramfb blob, where a byte loop
//!   would be a second copy of the same logic and much slower.
//!
//! Layout facts that are easy to get wrong, all of them from QEMU's fw_cfg
//! specification:
//!
//! * the MMIO selector register (base + 8, 16-bit) is **big-endian** — unlike
//!   the x86 port's I/O-port interface, which is little-endian;
//! * every field of the DMA structure is big-endian *in memory*, because the
//!   device reads bytes, not a CPU word;
//! * the DMA register (base + 16, 64-bit) holds a **physical** address. This
//!   kernel runs identity-mapped on the firmware's page tables, so a pointer is
//!   a physical address and the code below passes it as-is. The day Sv39 sits
//!   between the kernel and RAM (M2) that line has to become a translation, and
//!   this comment is the warning;
//! * `control` bit 3 means "the selector is in the upper 16 bits", so a transfer
//!   can both select an item and name its operation — and a select resets the
//!   item's data offset to zero, which is why each transfer here carries the
//!   selector instead of relying on whatever the offset was left at.
//!
//! Writes are allowed only to items QEMU marked writable (`etc/ramfb` is one);
//! a rejected write sets the error bit, which [`Error::Transfer`] carries back
//! to the caller. Silently ignoring it would surface much later as a black
//! screen, which is exactly the kind of failure a boot log exists to prevent.

use core::fmt;

use crate::sync::Spinlock;

/// fw_cfg on QEMU's `virt` machine.
///
/// The device tree names the same node (`/fw-cfg@10100000`); the constant is
/// kept because the console's display is probed before the tree is parsed, and
/// a wrong address here fails loudly (the signature check below rejects it)
/// rather than silently — the same trade `drivers::uart::BASE` makes.
pub const BASE: usize = 0x1010_0000;

const DATA: usize = 0x00; // 8 bytes wide, string-preserving
const SELECTOR: usize = 0x08; // 2 bytes, big-endian
const DMA: usize = 0x10; // 8 bytes, big-endian physical address

const SIGNATURE_SELECTOR: u16 = 0x0000; // "QEMU", four bytes
const ID_SELECTOR: u16 = 0x0001; // feature bitmap (bit 1 = DMA interface)
const FILE_DIR_SELECTOR: u16 = 0x0019; // the file directory
const SIGNATURE: u32 = 0x5145_4d55; // 'Q' 'E' 'M' 'U'
const ID_DMA: u32 = 1 << 1;

const CTL_ERROR: u32 = 1 << 0;
const CTL_READ: u32 = 1 << 1;
const CTL_SKIP: u32 = 1 << 2;
const CTL_SELECT: u32 = 1 << 3;
const CTL_WRITE: u32 = 1 << 4;
/// The operations a transfer is "in progress" for: QEMU clears the whole control
/// word when it finishes, so any of these still set means "not done yet".
const CTL_ACTIVE: u32 = CTL_READ | CTL_SKIP | CTL_WRITE;

/// A directory entry: 64 bytes, so a name field 56 bytes wide.
const ENTRY_BYTES: usize = 64;
const NAME_BYTES: usize = 56;
/// Directories bigger than this are refused instead of buffered: the kernel has
/// no allocator (M2) and `virt` reports a handful of entries.
const MAX_ENTRIES: u32 = 64;

/// The DMA structure the device reads and writes in guest RAM.
///
/// `repr(C, align(8))` because the device dereferences the address it was given
/// as a packed 16-byte structure; the alignment is stated rather than assumed so
/// the field at offset 8 is a legal 64-bit access. The *fields* hold big-endian
/// values — [`DmaAccess::store`] and [`DmaAccess::control`] are the only ways in
/// and out, so no caller can hand the device a native-order word by accident.
#[repr(C, align(8))]
struct DmaAccess {
    control: u32,
    length: u32,
    address: u64,
}

impl DmaAccess {
    const fn new() -> Self {
        DmaAccess {
            control: 0,
            length: 0,
            address: 0,
        }
    }
}

/// The one DMA structure.
///
/// It lives in a static (not on the stack) because its address is handed to the
/// device and the device may finish the transfer after the write that started it
/// returns. The spinlock is not there for concurrency — one hart boots, and
/// nothing else calls this driver — it is there so the structure has a single
/// owner and the unsafe accesses below cannot be duplicated by a second caller
/// that arrives later.
static DMA_ACCESS: Spinlock<DmaAccess> = Spinlock::new(DmaAccess::new());

/// Why a fw_cfg operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The named item is not in the file directory.
    NoSuchFile,
    /// The directory is bigger than [`MAX_ENTRIES`] — the kernel refuses to
    /// guess where the entry it wants ends.
    DirectoryTooBig(u32),
    /// The device set the DMA error bit (`control` as it came back).
    Transfer(u32),
    /// The caller's buffer is not the item's size.
    Size {
        /// What the directory says the item holds.
        expected: usize,
        /// What the caller passed.
        actual: usize,
    },
    /// The lookup name is longer than the directory's 56-byte name field.
    NameTooLong,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NoSuchFile => write!(f, "no such fw_cfg item"),
            Error::DirectoryTooBig(count) => {
                write!(
                    f,
                    "fw_cfg file directory has {count} entries (max {MAX_ENTRIES})"
                )
            }
            Error::Transfer(control) => {
                write!(f, "fw_cfg DMA transfer failed (control {control:#x})")
            }
            Error::Size { expected, actual } => {
                write!(f, "fw_cfg item is {expected} bytes, buffer is {actual}")
            }
            Error::NameTooLong => write!(f, "fw_cfg item name is longer than 56 bytes"),
        }
    }
}

/// A probed fw_cfg device.
#[derive(Clone, Copy)]
pub struct FwCfg {
    base: usize,
}

impl FwCfg {
    /// Probe the device at `base`, returning `None` when nothing answers.
    ///
    /// Two questions, in order: does the signature read back `QEMU`, and does
    /// the feature bitmap advertise the DMA interface? Both are answered through
    /// the byte-wide data register, so neither depends on an endianness
    /// convention. A machine without fw_cfg is a machine without a display —
    /// not an error, because the kernel is perfectly usable on the serial line.
    pub fn probe_at(base: usize) -> Option<Self> {
        let device = FwCfg { base };
        device.select(SIGNATURE_SELECTOR);
        let mut signature = [0u8; 4];
        for byte in &mut signature {
            *byte = device.data_byte();
        }
        if u32::from_be_bytes(signature) != SIGNATURE {
            return None;
        }
        device.select(ID_SELECTOR);
        if device.data_u32_le() & ID_DMA == 0 {
            return None;
        }
        Some(device)
    }

    /// Look `name` up in the file directory.
    ///
    /// The directory is a `count` (big-endian, 4 bytes) followed by `count`
    /// 64-byte entries; after the count the data offset is at 4, so the entries
    /// are read by one DMA transfer from where the count left off. The
    /// directory is re-walked on every lookup: it is read twice per boot, and
    /// caching it would need a buffer whose lifetime outlives the call for no
    /// gain.
    pub fn find(&self, name: &str) -> Result<FileEntry, Error> {
        let mut name_bytes = [0u8; NAME_BYTES];
        if name.len() + 1 > NAME_BYTES {
            return Err(Error::NameTooLong);
        }
        name_bytes[..name.len()].copy_from_slice(name.as_bytes());

        self.select(FILE_DIR_SELECTOR);
        let count = self.data_u32_be();
        if count == 0 {
            return Err(Error::NoSuchFile);
        }
        if count > MAX_ENTRIES {
            return Err(Error::DirectoryTooBig(count));
        }

        // The offset is at 4 after the count: exactly where the entries start.
        let mut entries = [0u8; MAX_ENTRIES as usize * ENTRY_BYTES];
        let bytes = count as usize * ENTRY_BYTES;
        self.dma_transfer(CTL_READ, None, entries.as_mut_ptr() as usize, bytes)?;

        for entry in entries[..bytes].chunks_exact(ENTRY_BYTES) {
            let raw = &entry[8..8 + NAME_BYTES];
            // The name is NUL-terminated and the rest of the field is padding:
            // comparing only `name.len()` bytes would let a lookup for a prefix
            // match a longer item ("etc/ram" finding "etc/ramfb").
            let end = raw.iter().position(|byte| *byte == 0).unwrap_or(NAME_BYTES);
            if end != name.len() || raw[..end] != name_bytes[..name.len()] {
                continue;
            }
            let size = u32::from_be_bytes([entry[0], entry[1], entry[2], entry[3]]);
            let selector = u16::from_be_bytes([entry[4], entry[5]]);
            return Ok(FileEntry { selector, size });
        }
        Err(Error::NoSuchFile)
    }

    /// Read the whole of item `name` into `out`, returning its size.
    pub fn read_file(&self, name: &str, out: &mut [u8]) -> Result<usize, Error> {
        let entry = self.find(name)?;
        let size = entry.size as usize;
        if out.len() != size {
            return Err(Error::Size {
                expected: size,
                actual: out.len(),
            });
        }
        self.dma_transfer(
            CTL_READ,
            Some(entry.selector),
            out.as_mut_ptr() as usize,
            size,
        )?;
        Ok(size)
    }

    /// Write the whole of item `name` from `bytes`.
    ///
    /// The length must be the item's size exactly: QEMU drops a write that
    /// would run past the end of the item, and "dropped" is reported as a
    /// transfer error rather than a short write.
    pub fn write_file(&self, name: &str, bytes: &[u8]) -> Result<(), Error> {
        let entry = self.find(name)?;
        if bytes.len() != entry.size as usize {
            return Err(Error::Size {
                expected: entry.size as usize,
                actual: bytes.len(),
            });
        }
        self.dma_transfer(
            CTL_WRITE,
            Some(entry.selector),
            bytes.as_ptr() as usize,
            bytes.len(),
        )
    }

    /// Point the data register at `selector` (and reset its offset to zero).
    fn select(&self, selector: u16) {
        // SAFETY: the selector register is a 16-bit MMIO register at
        // `base + 8` on a device the platform guarantees answers there; the
        // value is big-endian by the fw_cfg specification, not by taste.
        unsafe {
            ((self.base + SELECTOR) as *mut u16).write_volatile(selector.to_be());
        }
    }

    /// One byte from the data register — the endianness-free way to read fw_cfg.
    fn data_byte(&self) -> u8 {
        // SAFETY: one byte from the data register at `base + 0`; a byte access
        // has no endianness and cannot leave the item's bounds.
        unsafe { ((self.base + DATA) as *const u8).read_volatile() }
    }

    /// A 32-bit scalar, four byte reads assembled in the item's own order.
    ///
    /// fw_cfg is not consistent about this and the specification is: the feature
    /// bitmap ([`ID_SELECTOR`]) is a **little-endian** unsigned int, while the
    /// file directory's `count` and each entry's `size` are **big-endian**. A
    /// single "read a u32" helper would be wrong for one of them — and wrong in
    /// the quiet direction: the ID check would just fail, and a misread size
    /// would ask for the wrong number of bytes.
    fn data_u32_le(&self) -> u32 {
        let mut bytes = [0u8; 4];
        for byte in &mut bytes {
            *byte = self.data_byte();
        }
        u32::from_le_bytes(bytes)
    }

    /// See [`Self::data_u32_le`].
    fn data_u32_be(&self) -> u32 {
        let mut bytes = [0u8; 4];
        for byte in &mut bytes {
            *byte = self.data_byte();
        }
        u32::from_be_bytes(bytes)
    }

    /// Run one DMA transfer: `length` bytes at `address`, either from the device
    /// (read) or to it (write).
    ///
    /// `select` names the item when set; `None` continues the item the data
    /// offset is currently on (which is how the directory entries follow their
    /// own count).
    ///
    /// The transfer completes inside the MMIO write that starts it — QEMU's
    /// documentation is explicit that an asynchronous implementation is only a
    /// possibility — but the poll below is written as if it were not: the
    /// completion condition is "the operation bits are gone", never "the write
    /// returned".
    fn dma_transfer(
        &self,
        operation: u32,
        select: Option<u16>,
        address: usize,
        length: usize,
    ) -> Result<(), Error> {
        let mut guard = DMA_ACCESS.lock();
        // A raw pointer into the locked payload: the volatile accesses below are
        // to memory the *device* modifies, which no Rust reference may be alive
        // across (the device is not a Rust alias).
        let dma: *mut DmaAccess = &mut *guard;
        let control = match select {
            Some(selector) => operation | CTL_SELECT | ((selector as u32) << 16),
            None => operation,
        };
        // SAFETY: `dma` points at the lock-protected `DMA_ACCESS`, which is
        // alive for as long as `guard` is held and is not reachable any other
        // way; the fields are written volatile and big-endian because the device
        // reads the bytes, and the value it must observe before it can act (the
        // control word) is written before the register that starts the transfer.
        unsafe {
            core::ptr::addr_of_mut!((*dma).control).write_volatile(control.to_be());
            core::ptr::addr_of_mut!((*dma).length).write_volatile((length as u32).to_be());
            core::ptr::addr_of_mut!((*dma).address).write_volatile((address as u64).to_be());
        }

        // The register holds a physical address; with no paging (see the module
        // comment) that is the pointer itself.
        // SAFETY: a 64-bit MMIO write to the DMA register at `base + 16`; the
        // address it carries is the structure written just above.
        unsafe {
            ((self.base + DMA) as *mut u64).write_volatile((dma as u64).to_be());
        }

        loop {
            // SAFETY: as above — the device clears `control` when it is done,
            // so the read must not be cached in a register (volatile).
            let raw = unsafe { core::ptr::addr_of!((*dma).control).read_volatile() };
            let control = u32::from_be(raw);
            if control & CTL_ERROR != 0 {
                return Err(Error::Transfer(control));
            }
            if control & CTL_ACTIVE == 0 {
                return Ok(());
            }
            core::hint::spin_loop();
        }
    }
}

/// One file-directory entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileEntry {
    /// The selector key that addresses the item.
    pub selector: u16,
    /// The item's size in bytes.
    pub size: u32,
}
