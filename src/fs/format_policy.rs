//! When may the kernel format a block device?
//!
//! The x86 tree's answer is ported here because the mistake it prevents is
//! machine-destroying, not stylistic: an early `boot::init_fs` formatted whenever
//! the ext2 *mount* failed, so a partitioned foreign disk (whose byte 1024 is the
//! GPT entry array, not a superblock) was classified "blank" and erased.
//!
//! The rule, in order:
//!
//! 1. a **parsable ext2 superblock** means "this is a filesystem" — never format,
//!    whatever else the device looks like. A filesystem that fails to mount is an
//!    error to report, not permission to erase user data;
//! 2. a device whose probed boot area (the first MiB, or the whole device when it
//!    is smaller) is **entirely zero** is genuinely blank — the only thing this
//!    kernel formats on its own;
//! 3. an ext2 magic whose superblock does not validate, a partition table, or any
//!    other non-zero data is **refused**, and the log names what was found.
//!
//! The x86 tree also supports an operator-written `PAGH-FORMAT` marker in sector
//! 0 as explicit consent to destroy a non-blank device. The RISC-V port has no
//! userland and no second machine to write that marker from, so the marker (and
//! `allow_destructive`) is deliberately absent: on this kernel, non-blank devices
//! are simply never formatted. The enum variant names stay aligned with x86 so
//! the port is recognisable.
//!
//! The classifier is pure (`core` only) and takes bytes; the I/O wrapper
//! ([`probe_device`]) is the only part that touches a [`BlockDevice`]. That split
//! is what lets the host harness assert the decision against real GPT/MBR/ext2
//! boot areas without booting anything.

use crate::drivers::block::BlockDevice;

/// Byte offset of the ext2 superblock inside the first filesystem block.
///
/// The superblock itself starts here; its `s_magic` field is another 0x38 bytes
/// in, and that is the byte pair this classifier must look at. The ported code
/// compared the magic *at* `SUPERBLOCK_OFFSET`, which is where the count fields
/// live — so a real ext2 device was classified as foreign data. The offset is
/// the on-disk struct's own field offset (`structs.rs` asserts it), so the two
/// cannot drift.
pub const SUPERBLOCK_OFFSET: usize = 1024;

/// Offset of `s_magic` inside the superblock (on-disk layout, `structs.rs`).
pub const SUPERBLOCK_MAGIC_OFFSET: usize = 0x38;

/// ext2/ext4 `s_magic` as it appears on disk (little-endian `0xEF53`).
pub const EXT2_MAGIC_LE: [u8; 2] = [0x53, 0xEF];

/// GPT header signature, at LBA 1 (byte 512).
pub const GPT_SIGNATURE: &[u8; 8] = b"EFI PART";

/// MBR boot signature, at the end of LBA 0 (byte 510).
pub const MBR_SIGNATURE: [u8; 2] = [0x55, 0xAA];

/// Minimum probe length that can recognise every layout above.
pub const MIN_PROBE: usize = SUPERBLOCK_OFFSET + SUPERBLOCK_MAGIC_OFFSET + 2;

/// How far into the device the probe looks: 1 MiB.
///
/// Enough for a GPT/MBR partition table, the ext2 superblock, and the boot-sector
/// or superblock layouts of the filesystems a user is likely to attach. The
/// limitation is stated instead of hidden: a device whose first MiB is zero but
/// which carries data later would be classified blank. That is the same window
/// and the same trade-off as the x86 tree.
pub const PROBE_WINDOW: usize = 1024 * 1024;

/// What the probed boot area of a device looks like.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    /// The ext2/ext4 superblock magic is present.
    Ext2,
    /// An MBR or GPT partition table is present.
    PartitionTable,
    /// Every probed byte is zero.
    Blank,
    /// Non-zero data that is neither of the above.
    Foreign,
}

impl Layout {
    /// Short name for the boot log line.
    pub fn name(self) -> &'static str {
        match self {
            Layout::Ext2 => "ext2-like",
            Layout::PartitionTable => "partitioned",
            Layout::Blank => "blank",
            Layout::Foreign => "foreign data",
        }
    }
}

/// Why a device must not be formatted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// A valid ext2 superblock is present: a real filesystem that failed to mount.
    ExistingFilesystem,
    /// The device carries a partition table (MBR or GPT).
    PartitionTable,
    /// The ext2 magic is present but the superblock does not validate.
    Ext2Like,
    /// Non-zero bytes that are neither a partition table nor an ext2 superblock.
    ForeignData,
}

impl Refusal {
    /// One-line explanation for the serial log.
    pub fn reason(self) -> &'static str {
        match self {
            Refusal::ExistingFilesystem => {
                "existing ext2 mount failed; refusing destructive reformat"
            }
            Refusal::PartitionTable => "device carries an MBR/GPT partition table",
            Refusal::Ext2Like => "device looks like ext2 but its superblock does not validate",
            Refusal::ForeignData => "device is neither blank nor ext2 (unknown on-disk data)",
        }
    }
}

/// Incremental boot-area classifier.
///
/// Construct with the first filesystem block and `feed` further chunks while
/// [`Probe::is_decided`] is false. Fail-closed by construction: a probe shorter
/// than [`MIN_PROBE`] is [`Layout::Foreign`] (refused), never `Blank`.
#[derive(Clone, Copy, Debug)]
pub struct Probe {
    layout: Layout,
    /// Still `Blank`-so-far: a later non-zero byte turns it into `Foreign`.
    provisional_blank: bool,
}

impl Probe {
    /// Classify the first chunk of the boot area.
    pub fn new(first_block: &[u8]) -> Self {
        if first_block.len() < MIN_PROBE {
            return Self {
                layout: Layout::Foreign,
                provisional_blank: false,
            };
        }
        let layout = if is_ext2_superblock(first_block) {
            Layout::Ext2
        } else if is_partition_table(first_block) {
            Layout::PartitionTable
        } else if is_zero(first_block) {
            Layout::Blank
        } else {
            Layout::Foreign
        };
        Self {
            layout,
            provisional_blank: layout == Layout::Blank,
        }
    }

    /// A probe that could not be taken (I/O error). Refused, never formatted.
    pub fn unreadable() -> Self {
        Self {
            layout: Layout::Foreign,
            provisional_blank: false,
        }
    }

    /// Feed the next chunk of the boot area.
    pub fn feed(&mut self, chunk: &[u8]) {
        if self.provisional_blank && !is_zero(chunk) {
            self.layout = Layout::Foreign;
            self.provisional_blank = false;
        }
    }

    /// The layout decided so far.
    pub fn layout(&self) -> Layout {
        self.layout
    }

    /// `true` once the layout can no longer change (no further probe needed).
    pub fn is_decided(&self) -> bool {
        !self.provisional_blank
    }
}

/// The one decision the boot path makes: may this device be formatted?
///
/// `has_valid_superblock` must be the result of
/// [`crate::fs::ext2::Ext2Fs::has_valid_superblock`] (a *parsable* superblock,
/// not merely the magic), and `layout` the result of [`probe_device`].
///
/// Precedence: a parsable ext2 superblock refuses unconditionally; a blank
/// device is formatted; everything else is refused by name.
pub fn format_allowed(layout: Layout, has_valid_superblock: bool) -> Result<(), Refusal> {
    if has_valid_superblock {
        // A real filesystem that failed to mount is an error, never permission
        // to erase user data.
        return Err(Refusal::ExistingFilesystem);
    }
    if layout == Layout::Blank {
        return Ok(());
    }
    Err(match layout {
        Layout::PartitionTable => Refusal::PartitionTable,
        Layout::Ext2 => Refusal::Ext2Like,
        // `Blank` returned above; listed for exhaustiveness.
        Layout::Blank | Layout::Foreign => Refusal::ForeignData,
    })
}

/// Probe a device's boot area (up to [`PROBE_WINDOW`] bytes).
///
/// A read failure is [`Layout::Foreign`]: when the device cannot be read, this
/// kernel must not decide that it is blank and wipe it.
pub fn probe_device(dev: &dyn BlockDevice) -> Layout {
    // Chunk = 8 sectors (4 KiB), the smallest unit this probe needs to recognise
    // every layout, and a stack buffer so the probe works before the heap has
    // grown a chunk.
    const CHUNK_SECTORS: u64 = 8;
    let mut buf = [0u8; (CHUNK_SECTORS as usize) * 512];
    let capacity_sectors = dev.sector_count();
    // Capacity 0 means "the device cannot report its size": probe one chunk
    // only, which is enough to recognise a superblock or a partition table but
    // can never conclude "blank".
    let window_sectors = if capacity_sectors == 0 {
        CHUNK_SECTORS
    } else {
        capacity_sectors.min((PROBE_WINDOW / 512) as u64)
    };
    let mut block = 0u64;
    let mut probe: Option<Probe> = None;
    while block < window_sectors {
        let take = core::cmp::min(CHUNK_SECTORS, window_sectors - block) as usize;
        let len = take * 512;
        if dev.read_block(block, &mut buf[..len]).is_err() {
            return Layout::Foreign;
        }
        match &mut probe {
            Some(p) => p.feed(&buf[..len]),
            None => probe = Some(Probe::new(&buf[..len])),
        }
        if let Some(p) = &probe {
            if p.is_decided() {
                break;
            }
        }
        block += take as u64;
    }
    match probe {
        // A device shorter than `MIN_PROBE` (or an empty probe) is refused, never
        // blank — `Probe::new` already fails closed for short input.
        Some(p) => p.layout(),
        None => Layout::Foreign,
    }
}

/// ext2/ext4 superblock magic at [`SUPERBLOCK_OFFSET`].
fn is_ext2_superblock(buf: &[u8]) -> bool {
    let magic = SUPERBLOCK_OFFSET + SUPERBLOCK_MAGIC_OFFSET;
    buf.len() >= MIN_PROBE && buf[magic..magic + 2] == EXT2_MAGIC_LE
}

/// GPT header at LBA 1 or the MBR boot signature at the end of LBA 0.
///
/// A protective MBR (GPT) carries both, so checking either alone is enough to
/// refuse; both are checked so a hybrid or MBR-only layout is caught too.
fn is_partition_table(buf: &[u8]) -> bool {
    let gpt = buf.len() >= 520 && &buf[512..520] == GPT_SIGNATURE;
    let mbr = buf.len() >= 512 && buf[510..512] == MBR_SIGNATURE;
    gpt || mbr
}

/// No non-zero byte anywhere in the slice (empty slices are trivially zero).
fn is_zero(buf: &[u8]) -> bool {
    buf.iter().all(|&b| b == 0)
}
