//! ext2-compatible filesystem: the on-disk format, the writer, and the
//! [`VfsNode`] adapters.
//!
//! This is a port of the x86 tree's ext2 (`Pagh-OS/src/fs/ext2/`) with the pieces
//! M5 does not need removed and three things changed on purpose:
//!
//! * **No write-ahead journal.** The x86 tree batches every mutation into a WAL
//!   transaction. Here a mutation is written straight to the device through
//!   [`Tx`], data blocks first and metadata blocks second, so committed metadata
//!   never points at a block that was not written. A power loss can still leave
//!   an operation half-applied; the x86 journal is explicitly out of M5's scope
//!   (`docs/roadmap.md`), and this module's README says so where a user reads it.
//! * **The block size is dynamic.** Host `mkfs.ext2 -t ext2` picks 1024-byte
//!   blocks on small images and 4096 on large ones, and an ext2 driver that only
//!   speaks one of them is not byte-compatible with the host tools. The geometry
//!   (`block_size`, `inode_size`, `ptrs_per_block`, `s_first_data_block`) is
//!   read from the superblock at mount and threaded through every address
//!   calculation; `format` picks 4096 for devices ≥ 32 KiB and 1024 below that.
//! * **`rename` is a real operation here.** The x86 tree satisfies POSIX rename
//!   with copy-then-unlink at the syscall layer; the shell's `mv` needs move
//!   semantics for directories too, so [`Ext2Fs::rename`] relinks the inode in
//!   one transaction (and refuses a directory move into its own subtree).
//!
//! Everything reachable from a `VfsNode` is total: corrupt on-disk values become
//! [`FsError`] values, never panics, and every 32-bit field read from disk is
//! validated before it is used as an index.
//!
//! The self-tests in [`crate::fs::selftest`] drive this module over the RAM
//! `MemBlock` device. Byte-compatibility with the host tools is checked by
//! mounting a `mkfs.ext2 -t ext2` image and reading a file out of it (the
//! commands are in `src/fs/README.md`), so the on-disk layout is compared with
//! the real thing rather than with a second copy of our own encoder.

#![allow(dead_code)]

pub mod alloc;
pub mod dir;
pub mod inode;
pub mod structs;
/// Pure symlink-layout / hard-link accounting rules.
pub mod symlink;

use ::alloc::collections::{BTreeMap, BTreeSet};
use ::alloc::string::String;
use ::alloc::sync::Arc;
use ::alloc::vec;
use ::alloc::vec::Vec;

use crate::drivers::block::BlockDevice;
use crate::fs::FsError;
use crate::sync::{Guard, Spinlock};
use crate::vfs::{FsStat, VfsError, VfsNode, VfsResult};

use structs::{
    align4, file_type_of_mode, inode_size, read_struct, read_u16, read_u32, write_struct,
    write_u32, Ext2GroupDesc, Ext2Inode, Ext2SuperBlock, EXT2_FIRST_INO, EXT2_MAGIC, EXT2_ROOT_INO,
    FT_DIR, FT_REG_FILE, INODE_SIZE, S_IFDIR, S_IFLNK, S_IFREG,
};

use symlink::{SymlinkLayout, UnlinkAction};

// ─── format layout constants ───────────────────────────────────────────────

/// Byte offset of the superblock inside the first filesystem block (for every
/// block size except 1024, where the superblock *is* block 1 at offset 0).
const SUPERBLOCK_OFFSET: usize = 1024;

/// Inodes 1..=10 are reserved by the format (boot loader, bad blocks, root,
/// ACL index, ...): the bitmap has their bits set and allocation starts at 11.
const RESERVED_INODES: u32 = EXT2_FIRST_INO - 1;

/// Inode density: roughly one inode per this many bytes of capacity, the same
/// heuristic host `mke2fs` uses (`inode_ratio = 16384`).
const BYTES_PER_INODE: u64 = 16 * 1024;

/// Floor on the inode count so even a tiny freshly-formatted filesystem keeps a
/// few usable inodes beyond the reserved set.
const MIN_INODES: u32 = 32;

/// Hard cap on block groups this port manages in RAM.
///
/// Every group costs one 32-byte descriptor in a `Vec`, and this kernel's heap
/// refuses any single allocation above one 64 KiB chunk. 1024 groups is 32 KiB
/// of descriptors, supports 128 GiB with 4096-byte blocks (32768 blocks/group),
/// and turns a corrupt `s_blocks_count` into a clean `BadSuperBlock` instead of
/// an allocation panic.
const MAX_GROUPS: usize = 1024;

// ─── geometry helpers ──────────────────────────────────────────────────────

/// `ceil(a / b)` for `b != 0`.
#[inline]
fn div_ceil(a: u64, b: u64) -> u64 {
    if b == 0 {
        0
    } else {
        (a + b - 1) / b
    }
}

/// Filesystem blocks needed to hold a file of `size` bytes.
#[inline]
fn blocks_for_size(size: u32, bs: usize) -> u64 {
    div_ceil(size as u64, bs as u64)
}

/// The block that holds the (primary) superblock: block 1 for 1024-byte blocks
/// (where block 0 is the boot sector outside the filesystem), block 0 otherwise
/// (the superblock sits at byte 1024 *inside* it).
#[inline]
fn superblock_block(bs: usize) -> u64 {
    if bs == 1024 {
        1
    } else {
        0
    }
}

/// Byte offset of `Ext2SuperBlock` inside [`superblock_block`].
#[inline]
fn superblock_offset(bs: usize) -> usize {
    if bs == 1024 {
        0
    } else {
        SUPERBLOCK_OFFSET
    }
}

/// First block of group `g` (`s_first_data_block + g * s_blocks_per_group`).
#[inline]
fn group_base(sb: &Ext2SuperBlock, g: u32) -> u64 {
    sb.s_first_data_block as u64 + g as u64 * sb.s_blocks_per_group as u64
}

/// Number of blocks the filesystem declares per group (the last group may be
/// shorter).
#[inline]
fn blocks_per_group(sb: &Ext2SuperBlock) -> u64 {
    sb.s_blocks_per_group.max(1) as u64
}

/// Blocks of the group-descriptor table (in group 0, and replicated in every
/// group when `format` writes the image).
#[inline]
fn gd_table_blocks(groups: u64, bs: usize) -> u64 {
    div_ceil(
        groups * core::mem::size_of::<Ext2GroupDesc>() as u64,
        bs as u64,
    )
}

/// Group owning inode `ino` (0-based group index).
#[inline]
fn group_of(ino: u32, sb: &Ext2SuperBlock) -> usize {
    let ipg = sb.s_inodes_per_group.max(1) as u64;
    ((ino.saturating_sub(1)) as u64 / ipg) as usize
}

/// Locate inode `ino` in its group's inode table: `(device block, byte offset)`.
///
/// Rejects garbage inode numbers (`0`, past `s_inodes_count`, a group past the
/// descriptor table) instead of clamping: a garbage directory entry that clamped
/// to another inode's slot is how a corrupt image frees arbitrary blocks.
fn inode_location(
    sb: &Ext2SuperBlock,
    gds: &[Ext2GroupDesc],
    bs: usize,
    ino: u32,
) -> Result<(u64, usize), FsError> {
    if ino == 0 || ino > sb.s_inodes_count {
        return Err(FsError::Corrupt);
    }
    let index = (ino - 1) as u64;
    let ipg = sb.s_inodes_per_group.max(1) as u64;
    let group = (index / ipg) as usize;
    let gd = gds.get(group).ok_or(FsError::Corrupt)?;
    let isz = inode_size(sb).max(1) as u64;
    let ipb = (bs as u64) / isz;
    if ipb == 0 {
        return Err(FsError::Corrupt);
    }
    let within = index % ipg;
    let block = gd.bg_inode_table as u64 + within / ipb;
    let off = ((within % ipb) * isz) as usize;
    if off + isz as usize > bs {
        return Err(FsError::Corrupt);
    }
    Ok((block, off))
}

// ─── Ext2Fs ─────────────────────────────────────────────────────────────────

struct Inner {
    sb: Ext2SuperBlock,
    gds: Vec<Ext2GroupDesc>,
}

/// A mounted ext2 filesystem over a [`BlockDevice`].
pub struct Ext2Fs {
    dev: Arc<dyn BlockDevice>,
    /// Filesystem block size in bytes (1024, 2048 or 4096).
    block_size: usize,
    /// Effective on-disk inode size in bytes (128, or the rev-1 value such as
    /// 256 that host `mkfs.ext2` writes).
    inode_size: usize,
    inner: Spinlock<Inner>,
    /// Serialises mutating transactions: a [`Tx`] snapshots `sb`/`gds` and
    /// publishes them at `commit`, so two concurrent mutations would race their
    /// bitmap/free-count updates (lost updates, double allocation).
    tx_lock: Spinlock<()>,
}

impl Ext2Fs {
    // ── geometry ──

    /// Filesystem block size in bytes.
    pub fn block_size(&self) -> usize {
        self.block_size
    }

    /// 512-byte device sectors per filesystem block.
    pub fn sectors_per_block(&self) -> u32 {
        (self.block_size / 512) as u32
    }

    /// `u32` pointers per indirect block (`block_size / 4`).
    pub fn ptrs_per_block(&self) -> u32 {
        structs::ptrs_per_block(self.block_size)
    }

    /// A snapshot of the superblock (free counts chase the live bitmap, so this
    /// is a copy, not a live reference).
    pub fn superblock(&self) -> Ext2SuperBlock {
        self.inner.lock().sb
    }

    /// Capacity snapshot for the `statfs`-shaped VFS hook.
    pub fn fs_stat(&self) -> FsStat {
        let sb = self.inner.lock().sb;
        FsStat {
            block_size: self.block_size as u64,
            blocks_total: sb.s_blocks_count as u64,
            blocks_free: sb.s_free_blocks_count as u64,
            inodes_total: sb.s_inodes_count as u64,
            inodes_free: sb.s_free_inodes_count as u64,
        }
    }

    // ── low-level device I/O (block_size bytes per filesystem block) ──

    /// Read one filesystem block into a fresh buffer.
    pub fn read_fs_block(&self, block: u64) -> Result<Vec<u8>, FsError> {
        Self::read_block_raw(&*self.dev, self.block_size, block)
    }

    fn read_block_raw(dev: &dyn BlockDevice, bs: usize, block: u64) -> Result<Vec<u8>, FsError> {
        let mut buf = vec![0u8; bs];
        let sectors = (bs / 512) as u64;
        let first = block.checked_mul(sectors).ok_or(FsError::Corrupt)?;
        match dev.read_block(first, &mut buf) {
            Ok(n) if n == bs => Ok(buf),
            _ => Err(FsError::IoError),
        }
    }

    fn write_block_raw(
        dev: &dyn BlockDevice,
        bs: usize,
        block: u64,
        data: &[u8],
    ) -> Result<(), FsError> {
        debug_assert_eq!(data.len(), bs);
        let sectors = (bs / 512) as u64;
        let first = block.checked_mul(sectors).ok_or(FsError::Corrupt)?;
        match dev.write_block(first, data) {
            Ok(n) if n == bs => Ok(()),
            _ => Err(FsError::IoError),
        }
    }

    // ── inode access ──

    /// Read one inode (the first 128 bytes of its table slot; a rev-1 image with
    /// 256-byte inodes keeps its extra bytes untouched by every writer here).
    pub fn read_inode(&self, ino: u32) -> Result<Ext2Inode, FsError> {
        let (block, off) = {
            let inner = self.inner.lock();
            inode_location(&inner.sb, &inner.gds, self.block_size, ino)?
        };
        let buf = self.read_fs_block(block)?;
        // SAFETY: `inode_location` proved `off + 128 <= block_size`, so the slice
        // handed to `read_struct` is at least `size_of::<Ext2Inode>()` long, and
        // an inode is plain integers/bytes (every bit pattern is valid).
        Ok(unsafe { read_struct::<Ext2Inode>(&buf[off..]) })
    }

    // ── mounting ──

    /// True when the device carries a structurally valid ext2 superblock and
    /// group-descriptor table.
    ///
    /// This is the "is there a filesystem here?" question the boot policy asks
    /// before it may format; it is deliberately stricter than "the magic is
    /// present" (a damages superblock must not be erased without being
    /// recognised as one).
    pub fn has_valid_superblock(dev: &dyn BlockDevice) -> bool {
        match Self::read_superblock(dev) {
            Ok(sb) => match sb.block_size() {
                Some(bs) => Self::read_geometry(dev, &sb, bs).is_ok(),
                None => false,
            },
            Err(_) => false,
        }
    }

    /// Read the superblock from its fixed byte offset 1024.
    ///
    /// The read is block-size independent: ext2 puts the superblock at byte 1024
    /// no matter how large a filesystem block is, and 1024 bytes covers the 0x88
    /// bytes this kernel uses even on a device whose block size is 512.
    fn read_superblock(dev: &dyn BlockDevice) -> Result<Ext2SuperBlock, FsError> {
        let mut buf = [0u8; SUPERBLOCK_OFFSET];
        match dev.read_block(2, &mut buf) {
            Ok(SUPERBLOCK_OFFSET) => {}
            _ => return Err(FsError::IoError),
        }
        // SAFETY: `buf` is 1024 bytes and `Ext2SuperBlock` is 0x88 bytes; the
        // struct is plain integers/arrays, so every bit pattern is valid.
        Ok(unsafe { read_struct::<Ext2SuperBlock>(&buf) })
    }

    /// Validate the superblock's geometry and read the full descriptor table
    /// (bounded by [`MAX_GROUPS`] and by the device's reported capacity).
    fn read_geometry(
        dev: &dyn BlockDevice,
        sb: &Ext2SuperBlock,
        bs: usize,
    ) -> Result<Vec<Ext2GroupDesc>, FsError> {
        if sb.s_magic != EXT2_MAGIC {
            return Err(FsError::BadSuperBlock);
        }
        let expected_first = if bs == 1024 { 1 } else { 0 };
        if sb.s_first_data_block != expected_first {
            return Err(FsError::BadSuperBlock);
        }
        if sb.s_inodes_count == 0 || sb.s_blocks_count <= sb.s_first_data_block {
            return Err(FsError::BadSuperBlock);
        }
        let max_bits_per_block = (bs * 8) as u32;
        if sb.s_blocks_per_group == 0
            || sb.s_blocks_per_group > max_bits_per_block
            || sb.s_inodes_per_group == 0
            || sb.s_inodes_per_group > max_bits_per_block
        {
            return Err(FsError::BadSuperBlock);
        }
        if sb.s_rev_level >= 1 {
            let isz = sb.s_inode_size as usize;
            if isz < INODE_SIZE || isz > bs || !isz.is_power_of_two() {
                return Err(FsError::BadSuperBlock);
            }
        }
        let span = (sb.s_blocks_count - sb.s_first_data_block) as u64;
        let groups = div_ceil(span, blocks_per_group(sb));
        if groups == 0 || groups > MAX_GROUPS as u64 {
            return Err(FsError::BadSuperBlock);
        }
        // A filesystem larger than the device can never be read back; reject it
        // while the error still names the representation, not a random I/O.
        let sectors = dev.sector_count();
        if sectors != 0 {
            let device_blocks = sectors / (bs / 512) as u64;
            if sb.s_blocks_count as u64 > device_blocks {
                return Err(FsError::BadSuperBlock);
            }
        }
        let mut gds = Vec::with_capacity(groups as usize);
        let mut cache: Vec<u8> = Vec::new();
        let mut cache_block = u64::MAX;
        let gd_size = core::mem::size_of::<Ext2GroupDesc>() as u64;
        for g in 0..groups {
            let rel = g * gd_size / bs as u64;
            let off = (g * gd_size % bs as u64) as usize;
            let block = superblock_block(bs) + 1 + rel;
            if block != cache_block {
                cache = Self::read_block_raw(dev, bs, block)?;
                cache_block = block;
            }
            // SAFETY: `off + size_of::<Ext2GroupDesc>() <= bs` because `off` is a
            // remainder modulo `bs` and the descriptor is 32 bytes with `bs >=
            // 1024`; plain integer struct, all bit patterns valid.
            let gd: Ext2GroupDesc = unsafe { read_struct::<Ext2GroupDesc>(&cache[off..]) };
            // Nothing may point outside the declared filesystem: a wild
            // descriptor would send the next allocation to an arbitrary block.
            if gd.bg_block_bitmap >= sb.s_blocks_count
                || gd.bg_inode_bitmap >= sb.s_blocks_count
                || gd.bg_inode_table >= sb.s_blocks_count
            {
                return Err(FsError::BadSuperBlock);
            }
            gds.push(gd);
        }
        Ok(gds)
    }

    /// Mount the filesystem and return the root directory node.
    pub fn mount(dev: Arc<dyn BlockDevice>) -> Result<Arc<dyn VfsNode>, FsError> {
        let fs = Self::mount_fs(dev)?;
        Ok(fs.root_node())
    }

    /// Mount the filesystem, returning the live handle.
    ///
    /// Validation only — nothing on the device is written, so a read-only medium
    /// mounts (and can be listed); the first mutation is what fails on a
    /// read-only device.
    pub fn mount_fs(dev: Arc<dyn BlockDevice>) -> Result<Arc<Ext2Fs>, FsError> {
        let sb = Self::read_superblock(&*dev)?;
        let bs = sb.block_size().ok_or(FsError::BadSuperBlock)?;
        let gds = Self::read_geometry(&*dev, &sb, bs)?;
        let inode_size = inode_size(&sb);
        Ok(Arc::new(Ext2Fs {
            dev,
            block_size: bs,
            inode_size,
            inner: Spinlock::new(Inner { sb, gds }),
            tx_lock: Spinlock::new(()),
        }))
    }

    /// The root directory's `VfsNode` (inode 2).
    pub fn root_node(self: &Arc<Self>) -> Arc<dyn VfsNode> {
        let (size, mode) = match self.read_inode(EXT2_ROOT_INO) {
            Ok(i) => (i.i_size as u64, i.i_mode),
            Err(_) => (0, S_IFDIR | 0o755),
        };
        Arc::new(Ext2Dir {
            fs: Arc::clone(self),
            ino: EXT2_ROOT_INO,
            name: String::from("/"),
            cached_size: size,
            mode,
        })
    }

    /// Build a child node by inode/name, dispatching on the inode's `i_mode`.
    ///
    /// The dispatch is explicit and must stay that way: handing a symlink inode
    /// to [`Ext2File`] would let `read` run the block map over `i_block`, where a
    /// *fast* symlink stores its target text — i.e. target bytes interpreted as
    /// block numbers and an arbitrary disk block returned. Anything that is
    /// neither a directory, a regular file nor a symlink (a foreign image's
    /// device node, fifo, ...) becomes an [`Ext2Opaque`] node with no readable
    /// content.
    fn node_for(fs: &Arc<Ext2Fs>, ino: u32, name: &str) -> Result<Arc<dyn VfsNode>, FsError> {
        let inode = fs.read_inode(ino)?;
        let size = inode.i_size as u64;
        let mode = inode.i_mode;
        if inode.is_dir() {
            Ok(Arc::new(Ext2Dir {
                fs: Arc::clone(fs),
                ino,
                name: String::from(name),
                cached_size: size,
                mode,
            }))
        } else if inode.is_symlink() {
            Ok(Arc::new(Ext2Symlink {
                fs: Arc::clone(fs),
                ino,
                name: String::from(name),
                cached_size: size,
                mode,
            }))
        } else if inode.is_reg() {
            Ok(Arc::new(Ext2File {
                fs: Arc::clone(fs),
                ino,
                name: String::from(name),
                cached_size: size,
                mode,
            }))
        } else {
            Ok(Arc::new(Ext2Opaque {
                ino,
                name: String::from(name),
                cached_size: size,
                mode,
            }))
        }
    }

    // ── directory lookup ──

    /// Enumerate a directory's live entries as `(name, inode)` (free slots
    /// excluded; `.`/`..` included).
    pub fn read_dir_entries(&self, dir_ino: u32) -> Result<Vec<(String, u32)>, FsError> {
        let inode = self.read_inode(dir_ino)?;
        if !inode.is_dir() {
            return Err(FsError::NotDirectory);
        }
        let nblocks = blocks_for_size(inode.i_size, self.block_size);
        let mut out = Vec::new();
        for lbn in 0..nblocks {
            if let Some(blk) = inode::block_for_offset(self, &inode, lbn * self.block_size as u64)?
            {
                let buf = self.read_fs_block(blk as u64)?;
                for e in dir::iter_entries(&buf)? {
                    if e.inode != 0 {
                        out.push((e.name, e.inode));
                    }
                }
            }
        }
        Ok(out)
    }

    /// Look up `name` in directory `dir_ino`, returning the child inode number.
    pub fn lookup_entry(&self, dir_ino: u32, name: &str) -> Result<u32, FsError> {
        let inode = self.read_inode(dir_ino)?;
        if !inode.is_dir() {
            return Err(FsError::NotDirectory);
        }
        let nblocks = blocks_for_size(inode.i_size, self.block_size);
        for lbn in 0..nblocks {
            if let Some(blk) = inode::block_for_offset(self, &inode, lbn * self.block_size as u64)?
            {
                let buf = self.read_fs_block(blk as u64)?;
                if let Some((ino, _)) = dir::find(&buf, name)? {
                    return Ok(ino);
                }
            }
        }
        Err(FsError::NotFound)
    }

    /// Is the directory inode `child` empty (no entry other than `.`/`..`)?
    fn dir_is_empty(&self, child: &Ext2Inode) -> Result<bool, FsError> {
        let nblocks = blocks_for_size(child.i_size, self.block_size);
        for lbn in 0..nblocks {
            if let Some(blk) = inode::block_for_offset(self, child, lbn * self.block_size as u64)? {
                let buf = self.read_fs_block(blk as u64)?;
                if dir::live_entry_count(&buf)? != 0 {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    /// Shared pre-flight for a new directory entry: reject unusable names and a
    /// name that already exists in `parent_ino`.
    fn check_new_entry(&self, parent_ino: u32, name: &str) -> Result<(), FsError> {
        if name.is_empty() || name == "." || name == ".." {
            return Err(FsError::InvalidArgument);
        }
        // A directory entry never contains a separator: every caller splits the
        // path first, and an entry created with a '/' in its name could never be
        // looked up again (the path walker splits on '/') — an unreachable,
        // unremovable file. Refuse it at the writer.
        if name.contains('/') {
            return Err(FsError::InvalidArgument);
        }
        if name.len() > 255 {
            return Err(FsError::NameTooLong);
        }
        if self.lookup_entry(parent_ino, name).is_ok() {
            return Err(FsError::AlreadyExists);
        }
        Ok(())
    }

    // ── creation / removal / rename ──

    /// Create a regular file or directory named `name` under `parent_ino`.
    /// Returns the new inode number.
    pub fn create(&self, parent_ino: u32, name: &str, is_dir: bool) -> Result<u32, FsError> {
        self.check_new_entry(parent_ino, name)?;
        let bs = self.block_size;
        let has_filetype = self.inner.lock().sb.has_filetype();
        let mut tx = Tx::new(self);
        let new_ino = tx.alloc_new_inode()?;

        let mut inode = Ext2Inode::zeroed();
        let file_type;
        if is_dir {
            let dblock = tx.alloc_zeroed_block()?;
            {
                let buf = tx.block(dblock as u64)?;
                dir::init_dot_entries(buf, new_ino, parent_ino, has_filetype);
            }
            inode.i_mode = S_IFDIR | 0o755;
            inode.i_links_count = 2; // "." plus the entry in the parent
            inode.i_size = bs as u32;
            inode.i_blocks = tx.sectors_per_block;
            inode.i_block[0] = dblock;
            file_type = FT_DIR;
        } else {
            inode.i_mode = S_IFREG | 0o644;
            inode.i_links_count = 1;
            inode.i_size = 0;
            inode.i_blocks = 0;
            file_type = FT_REG_FILE;
        }
        tx.write_inode(new_ino, &inode)?;

        Self::insert_dirent(&mut tx, parent_ino, name, new_ino, file_type)?;

        if is_dir {
            // The child's ".." is a hard link to the parent: bump the parent's
            // link count and the group's directory counter.
            let mut pinode = tx.read_inode(parent_ino)?;
            pinode.i_links_count = pinode.i_links_count.saturating_add(1);
            tx.write_inode(parent_ino, &pinode)?;
            let g = group_of(new_ino, &tx.sb);
            if let Some(gd) = tx.gds.get_mut(g) {
                gd.bg_used_dirs_count = gd.bg_used_dirs_count.saturating_add(1);
            }
        }

        tx.commit()?;
        Ok(new_ino)
    }

    /// Create a symbolic link named `name` under `parent_ino` whose target is
    /// the raw bytes `target` (stored verbatim, never resolved here).
    ///
    /// The layout comes from [`symlink::plan`]: a target ≤ 59 bytes is stored
    /// inline in `i_block` with no data block at all; longer targets get one
    /// data block whose zero tail terminates the string, exactly like Linux.
    pub fn create_symlink(
        &self,
        parent_ino: u32,
        name: &str,
        target: &[u8],
    ) -> Result<u32, FsError> {
        self.check_new_entry(parent_ino, name)?;
        let plan = match symlink::plan(target) {
            Ok(p) => p,
            Err(symlink::SymlinkError::Empty) => return Err(FsError::InvalidArgument),
            Err(symlink::SymlinkError::TooBig) => return Err(FsError::FileTooBig),
        };
        let mut tx = Tx::new(self);
        let new_ino = tx.alloc_new_inode()?;

        let mut inode = Ext2Inode::zeroed();
        inode.i_mode = S_IFLNK | 0o777; // ext2 symlinks are always 0777
        inode.i_size = plan.size as u32;
        inode.i_links_count = 1;
        match plan.layout {
            SymlinkLayout::Fast => {
                let inline = plan.inline.unwrap_or([0u8; symlink::INLINE_CAPACITY]);
                inode.i_block = symlink::inline_words(&inline);
                inode.i_blocks = 0;
            }
            SymlinkLayout::Slow => {
                let blk = tx.alloc_zeroed_block()?;
                {
                    let b = tx.data_block(blk as u64, true)?;
                    let n = target.len().min(b.len());
                    b[..n].copy_from_slice(&target[..n]);
                }
                inode.i_block[0] = blk;
                inode.i_blocks = tx.sectors_per_block;
            }
        }
        tx.write_inode(new_ino, &inode)?;

        Self::insert_dirent(&mut tx, parent_ino, name, new_ino, structs::FT_SYMLINK)?;
        tx.commit()?;
        Ok(new_ino)
    }

    /// Read the target of symlink `ino`: exactly `i_size` bytes, no trailing NUL.
    pub fn read_symlink(&self, ino: u32) -> Result<Vec<u8>, FsError> {
        let inode = self.read_inode(ino)?;
        if !inode.is_symlink() {
            return Err(FsError::Corrupt);
        }
        if symlink::is_fast(inode.i_blocks, inode.i_size) {
            let inline = symlink::inline_bytes(&inode.i_block);
            let target = symlink::fast_target(&inline, inode.i_size).ok_or(FsError::Corrupt)?;
            return Ok(target.to_vec());
        }
        let size = inode.i_size as usize;
        if size == 0 {
            return Ok(Vec::new());
        }
        if inode::block_for_offset(self, &inode, 0)?.is_none() {
            return Err(FsError::Corrupt);
        }
        let mut out = vec![0u8; size];
        let n = self.read_file(ino, 0, &mut out)?;
        out.truncate(n);
        Ok(out)
    }

    /// Add the hard link `name` under `parent_ino` pointing at `target_ino`.
    ///
    /// Refused for a directory target: ext2 has no directory hard links and a
    /// directory's `i_links_count` counts `.`/`..` rather than names.
    pub fn link(&self, parent_ino: u32, name: &str, target_ino: u32) -> Result<(), FsError> {
        self.check_new_entry(parent_ino, name)?;
        let mut tx = Tx::new(self);
        let mut target = tx.read_inode(target_ino)?;
        if target.is_dir() {
            return Err(FsError::IsDirectory);
        }
        target.i_links_count = symlink::link_bump(target.i_links_count).ok_or(FsError::Corrupt)?;
        tx.write_inode(target_ino, &target)?;
        Self::insert_dirent(
            &mut tx,
            parent_ino,
            name,
            target_ino,
            file_type_of_mode(target.i_mode),
        )?;
        tx.commit()
    }

    /// Remove `name` from `parent_ino`: a file, a symlink, or an empty
    /// directory. Returns `AlreadyExists` for a non-empty directory.
    ///
    /// For a regular inode the link count decides: with several names only
    /// `i_links_count` is decremented, and the blocks + inode are freed only for
    /// the last name — deleting one hard link must not destroy data another name
    /// still reads.
    pub fn unlink(&self, parent_ino: u32, name: &str) -> Result<(), FsError> {
        if name == "." || name == ".." {
            return Err(FsError::InvalidArgument);
        }
        let mut tx = Tx::new(self);
        let (dir_block, child_ino) = match Self::tx_lookup_entry(&mut tx, parent_ino, name)? {
            Some(hit) => hit,
            None => return Err(FsError::NotFound),
        };
        let mut child = tx.read_inode(child_ino)?;

        // Empty-directory check on committed state (the child is unmodified).
        if child.is_dir() && !self.dir_is_empty(&child)? {
            return Err(FsError::AlreadyExists);
        }

        {
            let buf = tx.block(dir_block)?;
            if dir::remove_from_block(buf, name)?.is_none() {
                return Err(FsError::NotFound);
            }
        }

        let released = if child.is_dir() {
            true
        } else {
            match symlink::unlink_action(child.i_links_count) {
                Some(UnlinkAction::DropLink) => {
                    child.i_links_count = child.i_links_count.saturating_sub(1);
                    false
                }
                Some(UnlinkAction::FreeInode) => true,
                // A live directory entry pointing at a free inode is already
                // corrupt; freeing "again" could release a reallocated inode.
                None => return Err(FsError::Corrupt),
            }
        };
        if released {
            tx.release_inode(child_ino, &child)?;
            child.i_links_count = 0;
        }
        tx.write_inode(child_ino, &child)?;

        if child.is_dir() {
            let mut pinode = tx.read_inode(parent_ino)?;
            pinode.i_links_count = pinode.i_links_count.saturating_sub(1);
            tx.write_inode(parent_ino, &pinode)?;
            let g = group_of(child_ino, &tx.sb);
            if let Some(gd) = tx.gds.get_mut(g) {
                gd.bg_used_dirs_count = gd.bg_used_dirs_count.saturating_sub(1);
            }
        }
        tx.commit()
    }

    /// Rename `old_name` in `old_parent` to `new_name` in `new_parent`.
    ///
    /// Semantics follow POSIX rename closely enough for a shell:
    ///
    /// * an existing `new_name` is replaced, but a directory may only replace an
    ///   empty directory and a file may not replace a directory (and vice versa);
    /// * renaming a name onto another hard link of the same inode is a no-op;
    /// * a directory cannot be moved into its own subtree (the parent chain of
    ///   `new_parent` is walked, so `mv a a/b` is refused);
    /// * a moved directory's `..` is rewritten and the two parents' link counts
    ///   are adjusted — `i_links_count` of the moved inode itself never changes.
    ///
    /// All of it is one [`Tx`], so the tree never shows the entry in both places
    /// or in neither.
    pub fn rename(
        &self,
        old_parent: u32,
        old_name: &str,
        new_parent: u32,
        new_name: &str,
    ) -> Result<(), FsError> {
        for name in [old_name, new_name] {
            if name.is_empty() || name == "." || name == ".." || name.contains('/') {
                return Err(FsError::InvalidArgument);
            }
            if name.len() > 255 {
                return Err(FsError::NameTooLong);
            }
        }
        let mut tx = Tx::new(self);
        let mut op = tx.read_inode(old_parent)?;
        if !op.is_dir() {
            return Err(FsError::NotDirectory);
        }
        let Some((old_block, child_ino)) = Self::tx_lookup_entry(&mut tx, old_parent, old_name)?
        else {
            return Err(FsError::NotFound);
        };
        let child = tx.read_inode(child_ino)?;

        if old_parent == new_parent && old_name == new_name {
            return Ok(());
        }

        let mut np = tx.read_inode(new_parent)?;
        if !np.is_dir() {
            return Err(FsError::NotDirectory);
        }

        // Replace an existing target.
        if let Some((new_block, existing_ino)) =
            Self::tx_lookup_entry(&mut tx, new_parent, new_name)?
        {
            if existing_ino == child_ino {
                // POSIX: oldpath and newpath are hard links to the same file —
                // do nothing and succeed.
                return Ok(());
            }
            let mut existing = tx.read_inode(existing_ino)?;
            if existing.is_dir() != child.is_dir() {
                return Err(if existing.is_dir() {
                    FsError::IsDirectory
                } else {
                    FsError::NotDirectory
                });
            }
            if existing.is_dir() {
                if !self.dir_is_empty(&existing)? {
                    return Err(FsError::NotEmpty);
                }
            }
            {
                let buf = tx.block(new_block)?;
                if dir::remove_from_block(buf, new_name)?.is_none() {
                    return Err(FsError::Corrupt);
                }
            }
            tx.release_inode(existing_ino, &existing)?;
            existing.i_links_count = 0;
            tx.write_inode(existing_ino, &existing)?;
            if existing.is_dir() {
                // Removing a child directory drops the ".." link it held.
                np.i_links_count = np.i_links_count.saturating_sub(1);
                tx.write_inode(new_parent, &np)?;
                let g = group_of(existing_ino, &tx.sb);
                if let Some(gd) = tx.gds.get_mut(g) {
                    gd.bg_used_dirs_count = gd.bg_used_dirs_count.saturating_sub(1);
                }
            }
        }

        // A directory may not move into its own subtree.
        if child.is_dir() && new_parent != old_parent {
            let mut cur = new_parent;
            let mut depth = 0u32;
            loop {
                if cur == child_ino {
                    return Err(FsError::InvalidArgument);
                }
                if cur == EXT2_ROOT_INO {
                    break;
                }
                depth = depth.saturating_add(1);
                if depth > 64 {
                    return Err(FsError::Corrupt);
                }
                let Some((_, up)) = Self::tx_lookup_entry(&mut tx, cur, "..")? else {
                    return Err(FsError::Corrupt);
                };
                if up == cur {
                    break;
                }
                cur = up;
            }
        }

        Self::insert_dirent(
            &mut tx,
            new_parent,
            new_name,
            child_ino,
            file_type_of_mode(child.i_mode),
        )?;
        {
            let buf = tx.block(old_block)?;
            if dir::remove_from_block(buf, old_name)?.is_none() {
                return Err(FsError::Corrupt);
            }
        }

        if child.is_dir() && new_parent != old_parent {
            let Some(dot_blk) = inode::block_for_offset(tx.fs, &child, 0)? else {
                return Err(FsError::Corrupt);
            };
            {
                let buf = tx.block(dot_blk as u64)?;
                if !dir::rewrite_dotdot(buf, new_parent)? {
                    return Err(FsError::Corrupt);
                }
            }
            let mut np = tx.read_inode(new_parent)?;
            np.i_links_count = np.i_links_count.saturating_add(1);
            tx.write_inode(new_parent, &np)?;
            op.i_links_count = op.i_links_count.saturating_sub(1);
            tx.write_inode(old_parent, &op)?;
        }
        tx.commit()
    }

    // ── file data ──

    /// Read up to `buf.len()` bytes of file `ino` starting at `offset`, clamped
    /// to `i_size`. A hole (or a corrupt pointer) reads as zeros.
    pub fn read_file(&self, ino: u32, offset: u64, buf: &mut [u8]) -> Result<usize, FsError> {
        let inode = self.read_inode(ino)?;
        let bs = self.block_size as u64;
        let size = inode.i_size as u64;
        if offset >= size || buf.is_empty() {
            return Ok(0);
        }
        let to_read = core::cmp::min(buf.len() as u64, size - offset) as usize;
        let mut done = 0usize;
        let mut pos = offset;
        while done < to_read {
            let within = (pos % bs) as usize;
            let chunk = core::cmp::min(self.block_size - within, to_read - done);
            match inode::block_for_offset(self, &inode, pos)? {
                Some(blk) => {
                    let b = self.read_fs_block(blk as u64)?;
                    buf[done..done + chunk].copy_from_slice(&b[within..within + chunk]);
                }
                None => {
                    for x in &mut buf[done..done + chunk] {
                        *x = 0;
                    }
                }
            }
            done += chunk;
            pos += chunk as u64;
        }
        Ok(to_read)
    }

    /// Write `data` at `offset`, allocating blocks and growing `i_size` as
    /// needed. Returns bytes written.
    ///
    /// Large writes are split into transactions of at most 16 data blocks, so a
    /// single `Tx` never holds more than a chunk-sized working set (this
    /// kernel's heap caps one allocation at 64 KiB).
    pub fn write_file(&self, ino: u32, offset: u64, data: &[u8]) -> Result<usize, FsError> {
        if data.is_empty() {
            return Ok(0);
        }
        // `i_size` is a 32-bit field: a write past 4 GiB would wrap it and alias
        // the file to its low 4 GiB.
        if offset.saturating_add(data.len() as u64) > u32::MAX as u64 {
            return Err(FsError::FileTooBig);
        }
        let bs = self.block_size as u64;
        /// Data blocks one transaction may touch.
        const TX_DATA_BLOCKS: usize = 16;

        let mut written = 0usize;
        let mut pos = offset;
        while written < data.len() {
            let mut tx = Tx::new(self);
            let mut inode = tx.read_inode(ino)?;
            // Only regular files have data blocks: writing a symlink inode would
            // overwrite the inline target with block pointers.
            if !inode.is_reg() {
                return Err(FsError::IsDirectory);
            }
            let mut touched = 0usize;
            while written < data.len() && touched < TX_DATA_BLOCKS {
                let lbn = pos / bs;
                let within = (pos % bs) as usize;
                let blk = tx.map_or_alloc(&mut inode, lbn)?;
                let chunk = core::cmp::min(self.block_size - within, data.len() - written);
                {
                    let full = within == 0 && chunk == self.block_size;
                    let b = tx.data_block(blk as u64, full)?;
                    b[within..within + chunk].copy_from_slice(&data[written..written + chunk]);
                }
                written += chunk;
                pos += chunk as u64;
                touched += 1;
            }
            if pos > inode.i_size as u64 {
                inode.i_size = pos as u32;
            }
            tx.write_inode(ino, &inode)?;
            tx.commit()?;
        }
        Ok(written)
    }

    /// Resize a regular file, zero-filling newly exposed bytes.
    pub fn truncate_file(&self, ino: u32, new_size: u64) -> Result<(), FsError> {
        if new_size > u32::MAX as u64 {
            return Err(FsError::FileTooBig);
        }
        let bs = self.block_size as u64;
        let inode0 = self.read_inode(ino)?;
        if !inode0.is_reg() {
            return Err(FsError::IsDirectory);
        }
        let old_size = inode0.i_size as u64;
        if new_size == old_size {
            return Ok(());
        }
        if new_size > old_size {
            // Grow through the regular write path: the gap must read as zeros,
            // and that is exactly what a hole-free zero fill guarantees.
            const ZERO_CHUNK: usize = 4096;
            let zeros = [0u8; ZERO_CHUNK];
            let mut off = old_size;
            while off < new_size {
                let n = core::cmp::min(ZERO_CHUNK as u64, new_size - off) as usize;
                self.write_file(ino, off, &zeros[..n])?;
                off += n as u64;
            }
            return Ok(());
        }

        // Shrink: free whole blocks past the new end, zero the tail of the block
        // that straddles it, and adjust `i_blocks`.
        let mut tx = Tx::new(self);
        let mut inode = tx.read_inode(ino)?;
        if !inode.is_reg() {
            return Err(FsError::IsDirectory);
        }
        let old_nblocks = blocks_for_size(old_size as u32, self.block_size);
        let new_nblocks = blocks_for_size(new_size as u32, self.block_size);
        let mut freed = 0u32;
        for lbn in new_nblocks..old_nblocks {
            freed = freed.saturating_add(tx.clear_mapping(&mut inode, lbn)?);
        }
        let new_rem = (new_size % bs) as usize;
        if new_rem != 0 && new_nblocks > 0 {
            let lbn = new_nblocks - 1;
            if let Some(blk) = inode::block_for_offset(tx.fs, &inode, lbn * bs)? {
                let b = tx.data_block(blk as u64, false)?;
                for x in b[new_rem..].iter_mut() {
                    *x = 0;
                }
            }
        }
        inode.i_size = new_size as u32;
        inode.i_blocks = inode
            .i_blocks
            .saturating_sub(freed.saturating_mul(tx.sectors_per_block));
        tx.write_inode(ino, &inode)?;
        tx.commit()
    }

    // ── Tx helpers shared by the operations above ──

    /// Look `name` up in `dir_ino` inside the transaction's block cache,
    /// returning `(directory block, child inode)`.
    fn tx_lookup_entry(
        tx: &mut Tx,
        dir_ino: u32,
        name: &str,
    ) -> Result<Option<(u64, u32)>, FsError> {
        let dinode = tx.read_inode(dir_ino)?;
        if !dinode.is_dir() {
            return Ok(None);
        }
        let bs = tx.fs.block_size as u64;
        let nblocks = blocks_for_size(dinode.i_size, tx.fs.block_size);
        for lbn in 0..nblocks {
            let Some(blk) = inode::block_for_offset(tx.fs, &dinode, lbn * bs)? else {
                continue;
            };
            let hit = {
                let buf = tx.block(blk as u64)?;
                dir::find(buf, name)?
            };
            if let Some((ino, _)) = hit {
                return Ok(Some((blk as u64, ino)));
            }
        }
        Ok(None)
    }

    /// Insert `(name -> child_ino)` into `dir_ino`, growing a new directory block
    /// when no existing block has room.
    fn insert_dirent(
        tx: &mut Tx,
        dir_ino: u32,
        name: &str,
        child_ino: u32,
        file_type: u8,
    ) -> Result<(), FsError> {
        if name.is_empty() || name == "." || name == ".." || name.contains('/') {
            return Err(FsError::InvalidArgument);
        }
        if name.len() > 255 {
            return Err(FsError::NameTooLong);
        }
        let bs = tx.fs.block_size as u64;
        let mut dinode = tx.read_inode(dir_ino)?;
        if !dinode.is_dir() {
            return Err(FsError::NotDirectory);
        }
        let nblocks = blocks_for_size(dinode.i_size, tx.fs.block_size);
        let has_filetype = tx.sb.has_filetype();

        // In-transaction duplicate scan: callers probe with `lookup_entry` first,
        // but that probe runs outside the tx lock; two entries with the same name
        // would silently break the link count.
        for lbn in 0..nblocks {
            let Some(blk) = inode::block_for_offset(tx.fs, &dinode, lbn * bs)? else {
                continue;
            };
            let exists = {
                let buf = tx.block(blk as u64)?;
                dir::find(buf, name)?.is_some()
            };
            if exists {
                return Err(FsError::AlreadyExists);
            }
        }

        // Try existing blocks (read-only scan: a hole is skipped, never
        // allocated just to look for a name).
        for lbn in 0..nblocks {
            let Some(blk) = inode::block_for_offset(tx.fs, &dinode, lbn * bs)? else {
                continue;
            };
            let inserted = {
                let buf = tx.block(blk as u64)?;
                dir::insert_into_block(buf, name, child_ino, has_filetype, file_type)?
            };
            if inserted {
                tx.write_inode(dir_ino, &dinode)?;
                return Ok(());
            }
        }

        // Grow a new directory block.
        let blk = tx.map_or_alloc(&mut dinode, nblocks)?;
        {
            let buf = tx.block(blk as u64)?;
            dir::init_empty_block(buf);
            if !dir::insert_into_block(buf, name, child_ino, has_filetype, file_type)? {
                return Err(FsError::Corrupt);
            }
        }
        dinode.i_size = dinode.i_size.saturating_add(tx.fs.block_size as u32);
        tx.write_inode(dir_ino, &dinode)?;
        Ok(())
    }
}

// ─── format ────────────────────────────────────────────────────────────────

impl Ext2Fs {
    /// Format `dev` as a fresh, host-mountable ext2 filesystem.
    ///
    /// The image has: one superblock per block group (features cleared, so a
    /// Linux host mounts it as plain ext2), a 128-byte inode size (the only one
    /// this writer ever creates), `lost+found` in the root, and free bitmaps that
    /// start in the state a freshly written metadata tree implies. `mke2fs`'s
    /// optional features — `resize_inode`, `dir_index`, `filetype`,
    /// `sparse_super` — are deliberately not emitted: none is needed for a
    /// correct mount, and leaving them clear keeps the image a strict subset of
    /// what this kernel understands.
    ///
    /// Block size is chosen from capacity: 4096 for devices of at least 32 KiB
    /// (so a blank 64 MiB QEMU image is a single block group), 1024 below that.
    ///
    /// The caller must have established that the device really is blank
    /// (`crate::fs::format_policy`); this function does not look at the old
    /// contents, only writes over them.
    pub fn format(dev: Arc<dyn BlockDevice>) -> Result<(), FsError> {
        let sectors = dev.sector_count();
        if sectors == 0 {
            // Without a capacity there is no way to size the bitmaps, and
            // guessing would write a filesystem whose last blocks are unmapped.
            return Err(FsError::IoError);
        }
        let bytes = sectors.saturating_mul(512);
        if bytes >= 32 * 1024 {
            Self::format_with(Arc::clone(&dev), 4096).or_else(|e| match e {
                FsError::OutOfSpace => Self::format_with(dev, 1024),
                other => Err(other),
            })
        } else {
            Self::format_with(dev, 1024)
        }
    }

    fn format_with(dev: Arc<dyn BlockDevice>, bs: usize) -> Result<(), FsError> {
        let sectors_per_block = (bs / 512) as u64;
        let capacity_blocks = dev.sector_count() / sectors_per_block;
        let bpg = (bs * 8) as u64;
        if capacity_blocks > (MAX_GROUPS as u64).saturating_mul(bpg) {
            // Beyond what this port's in-RAM descriptor table handles; refusing
            // beats creating an image it cannot mount.
            return Err(FsError::OutOfSpace);
        }
        // The WAL reserve of the x86 formatter is absent here (no journal), so
        // the filesystem may use the whole device.
        let mut total_blocks = capacity_blocks;
        if total_blocks > u32::MAX as u64 {
            total_blocks = u32::MAX as u64;
        }
        if total_blocks < 8 {
            return Err(FsError::OutOfSpace);
        }

        let inodes_per_block = (bs / INODE_SIZE) as u64;
        let gd_size = core::mem::size_of::<Ext2GroupDesc>() as u64;
        let mut groups = div_ceil(total_blocks, bpg);

        // Shrink away a tail group too small to hold its own metadata plus at
        // least one data block, then derive the (uniform) inode geometry.
        let (gdb, ipg, itb, meta) = loop {
            let gdb = div_ceil(groups * gd_size, bs as u64);
            let scaled = total_blocks.saturating_mul(bs as u64) / BYTES_PER_INODE;
            let per_group = (scaled.max(MIN_INODES as u64) / groups.max(1)).max(inodes_per_block);
            let per_group = div_ceil(per_group, inodes_per_block) * inodes_per_block;
            if per_group > bpg {
                return Err(FsError::OutOfSpace);
            }
            let itb = per_group / inodes_per_block;
            let meta = 1 + gdb + 2 + itb; // sb, gd table, 2 bitmaps, inode table
            let tail_span = total_blocks - (groups - 1) * bpg;
            if groups > 1 && tail_span < meta + 3 {
                groups -= 1;
                total_blocks = groups * bpg;
                continue;
            }
            // Group 0 additionally owns the root directory and lost+found.
            if total_blocks <= meta * groups + 2 {
                return Err(FsError::OutOfSpace);
            }
            break (gdb, per_group as u32, itb, meta);
        };

        let total_inodes = groups.saturating_mul(ipg as u64);
        if total_inodes > u32::MAX as u64 {
            return Err(FsError::OutOfSpace);
        }
        let total_blocks = total_blocks as u32;
        let groups = groups as u32;
        let bpg = bpg as u32;
        let gdb = gdb as u32;
        let itb = itb as u32;
        let meta = meta as u32;
        let first = if bs == 1024 { 1u32 } else { 0u32 };

        let mut sb = Ext2SuperBlock {
            s_inodes_count: total_inodes as u32,
            s_blocks_count: total_blocks,
            s_r_blocks_count: 0,
            s_free_blocks_count: 0, // filled after the layout is known
            s_free_inodes_count: 0,
            s_first_data_block: first,
            s_log_block_size: match bs {
                1024 => 0,
                2048 => 1,
                _ => 2,
            },
            s_log_frag_size: match bs {
                1024 => 0,
                2048 => 1,
                _ => 2,
            },
            s_blocks_per_group: bpg,
            s_frags_per_group: bpg,
            s_inodes_per_group: ipg,
            s_mtime: 0,
            s_wtime: 0,
            s_mnt_count: 0,
            // 0xFFFF means "never time to check": this kernel writes no
            // timestamps, and a host `fsck` should not nag about them.
            s_max_mnt_count: 0xFFFF,
            s_magic: EXT2_MAGIC,
            s_state: 1,  // cleanly unmounted
            s_errors: 1, // continue on error
            s_minor_rev_level: 0,
            s_lastcheck: 0,
            s_checkinterval: 0,
            s_creator_os: 0, // "Linux"
            s_rev_level: 1,  // dynamic inode size (the value is still 128)
            s_def_resuid: 0,
            s_def_resgid: 0,
            s_first_ino: EXT2_FIRST_INO,
            s_inode_size: INODE_SIZE as u16,
            s_block_group_nr: 0,
            s_feature_compat: 0,
            s_feature_incompat: 0,
            s_feature_ro_compat: 0,
            s_uuid: [0; 16],
            s_volume_name: [0; 16],
        };
        for (i, b) in b"pagh-riscv".iter().enumerate() {
            if let Some(slot) = sb.s_volume_name.get_mut(i) {
                *slot = *b;
            }
        }

        // Group descriptors: group 0 also owns the root dir and lost+found.
        let mut gds: Vec<Ext2GroupDesc> = Vec::with_capacity(groups as usize);
        let mut free_blocks = 0u32;
        let mut free_inodes = 0u32;
        for g in 0..groups {
            let base = first as u64 + g as u64 * bpg as u64;
            let span = core::cmp::min(bpg as u64, total_blocks as u64 - base) as u32;
            let used = if g == 0 { meta + 2 } else { meta };
            let gd_free_blocks = span.saturating_sub(used);
            let gd_free_inodes = ipg.saturating_sub(if g == 0 { RESERVED_INODES + 1 } else { 0 });
            free_blocks = free_blocks.saturating_add(gd_free_blocks);
            free_inodes = free_inodes.saturating_add(gd_free_inodes);
            gds.push(Ext2GroupDesc {
                bg_block_bitmap: (base + 1 + gdb as u64) as u32,
                bg_inode_bitmap: (base + 2 + gdb as u64) as u32,
                bg_inode_table: (base + 3 + gdb as u64) as u32,
                bg_free_blocks_count: gd_free_blocks.min(u16::MAX as u32) as u16,
                bg_free_inodes_count: gd_free_inodes.min(u16::MAX as u32) as u16,
                bg_used_dirs_count: if g == 0 { 2 } else { 0 },
                bg_pad: 0,
                bg_reserved: [0; 12],
            });
        }
        sb.s_free_blocks_count = free_blocks;
        sb.s_free_inodes_count = free_inodes;

        // The primary superblock image and a group-descriptor table image are
        // written at the start of *every* group (no sparse_super in this image),
        // which is what `mke2fs` does when the feature is off.
        let mut sb_block = vec![0u8; bs];
        // SAFETY: `superblock_offset(bs) + size_of::<Ext2SuperBlock>() <= bs` for
        // all three supported block sizes (1024: 0+136 <= 1024; larger: 1024+136
        // <= 2048/4096); plain integer struct, all bit patterns valid.
        unsafe { write_struct(&mut sb_block[superblock_offset(bs)..], &sb) };

        let gd_blocks = gdb as usize;
        let mut gd_table = vec![0u8; gd_blocks * bs];
        for (g, gd) in gds.iter().enumerate() {
            let off = g * core::mem::size_of::<Ext2GroupDesc>();
            // SAFETY: the table has `gdb * bs` bytes and the descriptors are
            // packed 32 bytes each; `gdb` was computed as ceil(groups*32/bs).
            unsafe { write_struct(&mut gd_table[off..], gd) };
        }

        let zero = vec![0u8; bs];
        for g in 0..groups {
            let base = first as u64 + g as u64 * bpg as u64;
            let span = core::cmp::min(bpg as u64, total_blocks as u64 - base);
            // Superblock copy + descriptor table.
            Self::write_block_raw(&*dev, bs, base, &sb_block)?;
            for i in 0..gd_blocks {
                let src = &gd_table[i * bs..(i + 1) * bs];
                Self::write_block_raw(&*dev, bs, base + 1 + i as u64, src)?;
            }
            // Block bitmap: metadata (+ root/lost+found in group 0) used; all
            // padding bits past the group span set so allocation stops at the
            // filesystem end.
            let mut bbm = vec![0u8; bs];
            let used = if g == 0 { meta + 2 } else { meta };
            for b in 0..used {
                alloc::set_bit(&mut bbm, b);
            }
            for b in span as u32..bpg {
                alloc::set_bit(&mut bbm, b);
            }
            Self::write_block_raw(&*dev, bs, gds[g as usize].bg_block_bitmap as u64, &bbm)?;
            // Inode bitmap: group 0 reserves the classic system inodes plus the
            // lost+found inode; padding bits beyond s_inodes_per_group are set.
            let mut ibm = vec![0u8; bs];
            if g == 0 {
                for i in 0..RESERVED_INODES + 1 {
                    alloc::set_bit(&mut ibm, i);
                }
            }
            for i in ipg..(bs * 8) as u32 {
                alloc::set_bit(&mut ibm, i);
            }
            Self::write_block_raw(&*dev, bs, gds[g as usize].bg_inode_bitmap as u64, &ibm)?;
            // Zero the inode table: a free inode's slot must not look like a
            // live inode to any reader (data blocks are zeroed on allocation
            // instead, which is cheaper and just as correct).
            for b in 0..itb as u64 {
                Self::write_block_raw(&*dev, bs, gds[g as usize].bg_inode_table as u64 + b, &zero)?;
            }
        }

        // Root directory (inode 2) and its entries: `.`, `..`, `lost+found`.
        let root_block = first as u64 + meta as u64;
        let lost_block = root_block + 1;
        let mut root_dir = vec![0u8; bs];
        dir::init_dot_entries(&mut root_dir, EXT2_ROOT_INO, EXT2_ROOT_INO, false);
        if !dir::insert_into_block(&mut root_dir, "lost+found", EXT2_FIRST_INO, false, 0)? {
            return Err(FsError::Corrupt);
        }
        Self::write_block_raw(&*dev, bs, root_block, &root_dir)?;

        let mut lost_dir = vec![0u8; bs];
        dir::init_dot_entries(&mut lost_dir, EXT2_FIRST_INO, EXT2_ROOT_INO, false);
        Self::write_block_raw(&*dev, bs, lost_block, &lost_dir)?;

        let mut root_inode = Ext2Inode::zeroed();
        root_inode.i_mode = S_IFDIR | 0o755;
        root_inode.i_links_count = 3; // ".", "..", "lost+found"
        root_inode.i_size = bs as u32;
        root_inode.i_blocks = sectors_per_block as u32;
        root_inode.i_block[0] = root_block as u32;
        self_write_inode_at(&*dev, bs, &sb, &gds, EXT2_ROOT_INO, &root_inode)?;

        let mut lost_inode = Ext2Inode::zeroed();
        lost_inode.i_mode = S_IFDIR | 0o755;
        lost_inode.i_links_count = 2; // "." and the entry in the root
        lost_inode.i_size = bs as u32;
        lost_inode.i_blocks = sectors_per_block as u32;
        lost_inode.i_block[0] = lost_block as u32;
        self_write_inode_at(&*dev, bs, &sb, &gds, EXT2_FIRST_INO, &lost_inode)?;

        dev.flush().map_err(|_| FsError::IoError)?;
        Ok(())
    }
}

/// Read-modify-write one inode table slot (format helper: the table was just
/// zeroed, so the read only preserves the other slots in the same block).
fn self_write_inode_at(
    dev: &dyn BlockDevice,
    bs: usize,
    sb: &Ext2SuperBlock,
    gds: &[Ext2GroupDesc],
    ino: u32,
    inode: &Ext2Inode,
) -> Result<(), FsError> {
    let (block, off) = inode_location(sb, gds, bs, ino)?;
    let mut buf = Ext2Fs::read_block_raw(dev, bs, block)?;
    // SAFETY: `inode_location` proved `off + size_of::<Ext2Inode>() <= bs` and
    // the buffer is one block long; plain integer struct, all bit patterns valid.
    unsafe { write_struct(&mut buf[off..], inode) };
    Ext2Fs::write_block_raw(dev, bs, block, &buf)
}

// ─── one mutation, batched ─────────────────────────────────────────────────

/// The working set of one mutating operation: a snapshot of the superblock and
/// group descriptors, plus every block the operation has dirtied.
///
/// Without a journal this is not a crash-atomicity mechanism; it is a *batch*.
/// It keeps `sb`/`gds` consistent while several blocks are edited (an allocation
/// updates a bitmap, two free counts and the superblock), serialises mutations
/// (`tx_lock`), and writes data blocks before metadata at [`Tx::commit`] so a
/// committed inode can never point at a data block the device did not see.
struct Tx<'a> {
    fs: &'a Ext2Fs,
    /// Held for the whole `Tx` lifetime: mutations are serialised per-filesystem.
    _guard: Guard<'a, ()>,
    sb: Ext2SuperBlock,
    gds: Vec<Ext2GroupDesc>,
    /// Filesystem block -> its new contents.
    dirty: BTreeMap<u64, Vec<u8>>,
    /// Blocks in `dirty` that are file data (written before metadata).
    data: BTreeSet<u64>,
    /// Cached `fs.block_size`/`fs.sectors_per_block` (the hot fields).
    block_size: usize,
    sectors_per_block: u32,
}

impl<'a> Tx<'a> {
    fn new(fs: &'a Ext2Fs) -> Self {
        let guard = fs.tx_lock.lock();
        let (sb, gds) = {
            let inner = fs.inner.lock();
            (inner.sb, inner.gds.clone())
        };
        Tx {
            fs,
            _guard: guard,
            sb,
            gds,
            dirty: BTreeMap::new(),
            data: BTreeSet::new(),
            block_size: fs.block_size,
            sectors_per_block: fs.sectors_per_block(),
        }
    }

    /// The dirty copy of block `blk`, reading it from the device on first use.
    fn block(&mut self, blk: u64) -> Result<&mut Vec<u8>, FsError> {
        if !self.dirty.contains_key(&blk) {
            let data = self.fs.read_fs_block(blk)?;
            self.dirty.insert(blk, data);
        }
        self.dirty.get_mut(&blk).ok_or(FsError::Corrupt)
    }

    /// Fetch a *file data* block, marking it for the data-first write phase.
    /// `full_overwrite` skips the read when the caller replaces all of it.
    fn data_block(&mut self, blk: u64, full_overwrite: bool) -> Result<&mut Vec<u8>, FsError> {
        if !self.dirty.contains_key(&blk) {
            let data = if full_overwrite {
                vec![0u8; self.block_size]
            } else {
                self.fs.read_fs_block(blk)?
            };
            self.dirty.insert(blk, data);
        }
        self.data.insert(blk);
        self.dirty.get_mut(&blk).ok_or(FsError::Corrupt)
    }

    /// Allocate a zeroed data block from the first group that has one.
    fn alloc_zeroed_block(&mut self) -> Result<u32, FsError> {
        let bpg = blocks_per_group(&self.sb);
        let first = self.sb.s_first_data_block as u64;
        let groups = self.gds.len();
        for g in 0..groups {
            if self.gds[g].bg_free_blocks_count == 0 {
                continue;
            }
            let bbm = self.gds[g].bg_block_bitmap as u64;
            self.block(bbm)?;
            let bit = match self.dirty.get_mut(&bbm) {
                Some(bm) => alloc::alloc_bit(bm),
                None => None,
            };
            let Some(bit) = bit else {
                // The bitmap disagrees with the cached free count: mark the
                // group exhausted and move on instead of trusting the counter.
                self.gds[g].bg_free_blocks_count = 0;
                continue;
            };
            let blk = first + g as u64 * bpg + bit as u64;
            if blk >= self.sb.s_blocks_count as u64 {
                // A padding bit the image forgot to set: undo and treat the
                // group as full.
                if let Some(bm) = self.dirty.get_mut(&bbm) {
                    alloc::clear_bit(bm, bit);
                }
                self.gds[g].bg_free_blocks_count = 0;
                continue;
            }
            self.sb.s_free_blocks_count = self.sb.s_free_blocks_count.saturating_sub(1);
            self.gds[g].bg_free_blocks_count = self.gds[g].bg_free_blocks_count.saturating_sub(1);
            let b = self.block(blk)?;
            for x in b.iter_mut() {
                *x = 0;
            }
            return Ok(blk as u32);
        }
        Err(FsError::OutOfSpace)
    }

    fn free_data_block(&mut self, blk: u32) -> Result<(), FsError> {
        let bpg = blocks_per_group(&self.sb);
        let Some(rel) = (blk as u64).checked_sub(self.sb.s_first_data_block as u64) else {
            // A block below the first data block would wrap around and free a
            // bogus bit (silent corruption on a garbage pointer): ignore it.
            return Ok(());
        };
        let g = (rel / bpg) as usize;
        let bit = (rel % bpg) as u32;
        let Some(gd) = self.gds.get(g).copied() else {
            return Err(FsError::Corrupt);
        };
        let bbm = gd.bg_block_bitmap as u64;
        self.block(bbm)?;
        if let Some(bm) = self.dirty.get_mut(&bbm) {
            if alloc::clear_bit(bm, bit) {
                self.sb.s_free_blocks_count = self.sb.s_free_blocks_count.saturating_add(1);
                if let Some(gd) = self.gds.get_mut(g) {
                    gd.bg_free_blocks_count = gd.bg_free_blocks_count.saturating_add(1);
                }
            }
        }
        Ok(())
    }

    /// Allocate an inode from the first group with a free slot.
    fn alloc_new_inode(&mut self) -> Result<u32, FsError> {
        let ipg = self.sb.s_inodes_per_group.max(1);
        let groups = self.gds.len();
        for g in 0..groups {
            if self.gds[g].bg_free_inodes_count == 0 {
                continue;
            }
            let ibm = self.gds[g].bg_inode_bitmap as u64;
            self.block(ibm)?;
            let bit = match self.dirty.get_mut(&ibm) {
                Some(bm) => alloc::alloc_bit(bm),
                None => None,
            };
            let Some(bit) = bit else {
                self.gds[g].bg_free_inodes_count = 0;
                continue;
            };
            let ino = (g as u64 * ipg as u64 + bit as u64 + 1) as u64;
            if ino > self.sb.s_inodes_count as u64 || ino < EXT2_FIRST_INO as u64 {
                // Past the table, or one of the reserved inodes whose bits a
                // corrupt image left clear: never hand those out.
                if let Some(bm) = self.dirty.get_mut(&ibm) {
                    alloc::clear_bit(bm, bit);
                }
                self.gds[g].bg_free_inodes_count = 0;
                continue;
            }
            self.sb.s_free_inodes_count = self.sb.s_free_inodes_count.saturating_sub(1);
            self.gds[g].bg_free_inodes_count = self.gds[g].bg_free_inodes_count.saturating_sub(1);
            return Ok(ino as u32);
        }
        Err(FsError::OutOfSpace)
    }

    fn free_inode_bit(&mut self, ino: u32) -> Result<(), FsError> {
        if ino == 0 {
            return Ok(());
        }
        let g = group_of(ino, &self.sb);
        let bit = (ino - 1) % self.sb.s_inodes_per_group.max(1);
        let Some(gd) = self.gds.get(g).copied() else {
            return Err(FsError::Corrupt);
        };
        let ibm = gd.bg_inode_bitmap as u64;
        self.block(ibm)?;
        if let Some(bm) = self.dirty.get_mut(&ibm) {
            if alloc::clear_bit(bm, bit) {
                self.sb.s_free_inodes_count = self.sb.s_free_inodes_count.saturating_add(1);
                if let Some(gd) = self.gds.get_mut(g) {
                    gd.bg_free_inodes_count = gd.bg_free_inodes_count.saturating_add(1);
                }
            }
        }
        Ok(())
    }

    fn read_inode(&mut self, ino: u32) -> Result<Ext2Inode, FsError> {
        let (block, off) = inode_location(&self.sb, &self.gds, self.block_size, ino)?;
        self.block(block)?;
        let buf = self.dirty.get(&block).ok_or(FsError::Corrupt)?;
        // SAFETY: `inode_location` proved `off + size_of::<Ext2Inode>() <=
        // block_size` and `buf` is one full block; plain integer struct.
        Ok(unsafe { read_struct::<Ext2Inode>(&buf[off..]) })
    }

    fn write_inode(&mut self, ino: u32, inode: &Ext2Inode) -> Result<(), FsError> {
        let (block, off) = inode_location(&self.sb, &self.gds, self.block_size, ino)?;
        let b = self.block(block)?;
        // SAFETY: as in `read_inode`; the slot is 128 bytes inside the block.
        unsafe { write_struct(&mut b[off..], inode) };
        Ok(())
    }

    /// Clear the mapping for logical block `lbn`, freeing the data block and any
    /// indirect block that becomes empty. Returns the number of freed blocks
    /// (data + indirect) so the caller can adjust `i_blocks`.
    fn clear_mapping(&mut self, inode: &mut Ext2Inode, lbn: u64) -> Result<u32, FsError> {
        let ppb = self.fs.ptrs_per_block() as u64;
        if lbn < 12 {
            let i = lbn as usize;
            if inode.i_block[i] == 0 {
                return Ok(0);
            }
            self.free_data_block(inode.i_block[i])?;
            inode.i_block[i] = 0;
            return Ok(1);
        }
        let l = lbn - 12;
        let (root_slot, path): (usize, Vec<u32>) = if l < ppb {
            (12, vec![l as u32])
        } else if l - ppb < ppb.saturating_mul(ppb) {
            let l2 = l - ppb;
            (13, vec![(l2 / ppb) as u32, (l2 % ppb) as u32])
        } else {
            let t = l - ppb.saturating_mul(ppb);
            if t >= ppb.saturating_mul(ppb).saturating_mul(ppb) {
                return Ok(0); // beyond the triple-indirect range
            }
            (
                14,
                vec![
                    (t / (ppb * ppb)) as u32,
                    ((t / ppb) % ppb) as u32,
                    (t % ppb) as u32,
                ],
            )
        };
        let root = inode.i_block[root_slot];
        if root == 0 {
            return Ok(0);
        }
        let (empty, freed) = self.clear_slot(root, &path, path.len() as u32)?;
        if empty {
            self.free_data_block(root)?;
            inode.i_block[root_slot] = 0;
            return Ok(freed + 1);
        }
        Ok(freed)
    }

    /// Free the leaf (and any emptied child indirect blocks) under `ind`,
    /// zeroing the traversed pointer slots. Returns `(ind became fully empty,
    /// blocks freed below ind)`.
    fn clear_slot(&mut self, ind: u32, path: &[u32], level: u32) -> Result<(bool, u32), FsError> {
        let Some(&slot) = path.first() else {
            return Err(FsError::Corrupt);
        };
        self.block(ind as u64)?;
        let child = {
            let buf = self.dirty.get(&(ind as u64)).ok_or(FsError::Corrupt)?;
            read_u32(buf, slot as usize * 4)
        };
        let mut freed = 0u32;
        if child != 0 {
            let child_empty = if level > 1 {
                let (e, f) = self.clear_slot(child, &path[1..], level - 1)?;
                freed = freed.saturating_add(f);
                e
            } else {
                self.free_data_block(child)?;
                freed = freed.saturating_add(1);
                true
            };
            if child_empty {
                if let Some(b) = self.dirty.get_mut(&(ind as u64)) {
                    write_u32(b, slot as usize * 4, 0);
                }
            }
        }
        let empty = match self.dirty.get(&(ind as u64)) {
            Some(b) => b.chunks_exact(4).all(|c| c == [0u8; 4]),
            None => false,
        };
        Ok((empty, freed))
    }

    /// Map (allocating as needed) logical block `lbn` of `inode` to an ext2
    /// block, walking 12 direct + single/double/triple indirect pointers.
    fn map_or_alloc(&mut self, inode: &mut Ext2Inode, lbn: u64) -> Result<u32, FsError> {
        let ppb = self.fs.ptrs_per_block() as u64;
        if lbn < 12 {
            let i = lbn as usize;
            if inode.i_block[i] == 0 {
                let nb = self.alloc_zeroed_block()?;
                inode.i_block[i] = nb;
                inode.i_blocks = inode.i_blocks.saturating_add(self.sectors_per_block);
            }
            return Ok(inode.i_block[i]);
        }
        let mut l = lbn - 12;
        if l < ppb {
            let root = self.ensure_indirect_root(inode, 12)?;
            return self.map_indirect(inode, root, l as u32);
        }
        l -= ppb;
        if l < ppb.saturating_mul(ppb) {
            let root = self.ensure_indirect_root(inode, 13)?;
            let l1 = self.map_indirect(inode, root, (l / ppb) as u32)?;
            return self.map_indirect(inode, l1, (l % ppb) as u32);
        }
        l -= ppb.saturating_mul(ppb);
        if l < ppb.saturating_mul(ppb).saturating_mul(ppb) {
            let root = self.ensure_indirect_root(inode, 14)?;
            let l2 = self.map_indirect(inode, root, (l / (ppb * ppb)) as u32)?;
            let l1 = self.map_indirect(inode, l2, ((l / ppb) % ppb) as u32)?;
            return self.map_indirect(inode, l1, (l % ppb) as u32);
        }
        Err(FsError::FileTooBig)
    }

    fn ensure_indirect_root(
        &mut self,
        inode: &mut Ext2Inode,
        which: usize,
    ) -> Result<u32, FsError> {
        if inode.i_block[which] == 0 {
            let nb = self.alloc_zeroed_block()?;
            inode.i_block[which] = nb;
            inode.i_blocks = inode.i_blocks.saturating_add(self.sectors_per_block);
        }
        Ok(inode.i_block[which])
    }

    fn map_indirect(
        &mut self,
        inode: &mut Ext2Inode,
        ind_block: u32,
        slot: u32,
    ) -> Result<u32, FsError> {
        self.block(ind_block as u64)?;
        let cur = match self.dirty.get(&(ind_block as u64)) {
            Some(b) => read_u32(b, slot as usize * 4),
            None => 0,
        };
        if cur != 0 {
            return Ok(cur);
        }
        let nb = self.alloc_zeroed_block()?;
        inode.i_blocks = inode.i_blocks.saturating_add(self.sectors_per_block);
        let b = self.block(ind_block as u64)?;
        write_u32(b, slot as usize * 4, nb);
        Ok(nb)
    }

    /// Free every data + indirect block referenced by `inode` (regular files and
    /// directories only; symlinks go through [`Self::free_inode_blocks`]).
    fn free_all_blocks(&mut self, inode: &Ext2Inode) -> Result<(), FsError> {
        for i in 0..12 {
            if inode.i_block[i] != 0 {
                self.free_data_block(inode.i_block[i])?;
            }
        }
        self.free_indirect(inode.i_block[12], 1)?;
        self.free_indirect(inode.i_block[13], 2)?;
        self.free_indirect(inode.i_block[14], 3)?;
        Ok(())
    }

    /// Delete-path counterpart of `free_all_blocks` that honours the inode kind.
    ///
    /// A **fast** symlink keeps its target *text* in `i_block`, so walking it as
    /// a block map would free the blocks whose numbers happen to spell the target
    /// — arbitrary block frees out of a corrupt-but-plausible inode. A **slow**
    /// symlink owns exactly the block in `i_block[0]` and no indirect tree.
    fn free_inode_blocks(&mut self, inode: &Ext2Inode) -> Result<(), FsError> {
        if inode.is_symlink() {
            if symlink::is_fast(inode.i_blocks, inode.i_size) {
                return Ok(());
            }
            if inode.i_block[0] != 0 {
                self.free_data_block(inode.i_block[0])?;
            }
            return Ok(());
        }
        self.free_all_blocks(inode)
    }

    fn free_indirect(&mut self, blk: u32, level: u32) -> Result<(), FsError> {
        if blk == 0 {
            return Ok(());
        }
        let buf = self.block(blk as u64)?.clone();
        let ppb = self.fs.ptrs_per_block() as usize;
        for slot in 0..ppb {
            let p = read_u32(&buf, slot * 4);
            if p != 0 {
                if level > 1 {
                    self.free_indirect(p, level - 1)?;
                } else {
                    self.free_data_block(p)?;
                }
            }
        }
        self.free_data_block(blk)?;
        Ok(())
    }

    /// Free an inode's blocks and its bitmap bit (the caller still writes the
    /// zeroed inode).
    fn release_inode(&mut self, ino: u32, inode: &Ext2Inode) -> Result<(), FsError> {
        self.free_inode_blocks(inode)?;
        self.free_inode_bit(ino)
    }

    /// Write every dirty block, then publish the new superblock/descriptors.
    ///
    /// File data goes first: if the machine dies between the two passes, the
    /// metadata still points at blocks that were written (ordered mode, the same
    /// principle the x86 journal gets from its data-before-commit rule).
    fn commit(mut self) -> Result<(), FsError> {
        let bs = self.block_size;
        // Patch the superblock into its block image.
        {
            let sb = self.sb;
            let b0 = self.block(superblock_block(bs))?;
            let off = superblock_offset(bs);
            // SAFETY: `off + size_of::<Ext2SuperBlock>() <= bs` (see format);
            // plain integer struct, all bit patterns valid.
            unsafe { write_struct(&mut b0[off..], &sb) };
        }
        // Patch every group descriptor into the primary table.
        let gd_size = core::mem::size_of::<Ext2GroupDesc>();
        let gds = self.gds.clone();
        for (g, gd) in gds.iter().enumerate() {
            let rel = g * gd_size / bs;
            let off = g * gd_size % bs;
            let block = superblock_block(bs) + 1 + rel as u64;
            let b = self.block(block)?;
            // SAFETY: as above; a descriptor is 32 bytes and `off` is a
            // remainder modulo the block size (>= 1024), so it fits.
            unsafe { write_struct(&mut b[off..], gd) };
        }
        // Data first.
        for (blk, data) in self.dirty.iter() {
            if self.data.contains(blk) {
                Ext2Fs::write_block_raw(&*self.fs.dev, bs, *blk, data)?;
            }
        }
        // Then metadata (and anything not marked as data).
        for (blk, data) in self.dirty.iter() {
            if !self.data.contains(blk) {
                Ext2Fs::write_block_raw(&*self.fs.dev, bs, *blk, data)?;
            }
        }
        self.fs.dev.flush().map_err(|_| FsError::IoError)?;
        let mut inner = self.fs.inner.lock();
        inner.sb = self.sb;
        inner.gds = self.gds;
        Ok(())
    }
}

// ─── VfsNode adapters ──────────────────────────────────────────────────────

/// Map a filesystem error onto the VFS error the shell sees.
fn fs_to_vfs(e: FsError) -> VfsError {
    match e {
        FsError::NotFound => VfsError::NotFound,
        FsError::AlreadyExists => VfsError::AlreadyExists,
        FsError::NotEmpty => VfsError::NotEmpty,
        FsError::IsDirectory => VfsError::IsADirectory,
        FsError::NotDirectory => VfsError::NotADirectory,
        FsError::NameTooLong => VfsError::NameTooLong,
        FsError::OutOfSpace => VfsError::NoSpace,
        FsError::IoError => VfsError::IoError,
        FsError::Corrupt | FsError::BadSuperBlock => VfsError::Corrupt,
        FsError::FileTooBig | FsError::InvalidArgument => VfsError::InvalidArgument,
    }
}

/// A directory node: inode number + the filesystem handle. Every operation goes
/// through the shared [`Ext2Fs`], so nodes are cheap value objects, not caches.
struct Ext2Dir {
    fs: Arc<Ext2Fs>,
    ino: u32,
    name: String,
    /// Size observed when the node was built: a fallback for `size()` when a
    /// later inode read fails, instead of reporting 0.
    cached_size: u64,
    mode: u16,
}

impl Ext2Dir {
    fn fs_stat_of(&self) -> Option<FsStat> {
        Some(self.fs.fs_stat())
    }
}

impl VfsNode for Ext2Dir {
    fn name(&self) -> &str {
        &self.name
    }
    fn is_directory(&self) -> bool {
        true
    }
    fn fs_stat(&self) -> Option<FsStat> {
        self.fs_stat_of()
    }
    fn fs_ino(&self) -> u64 {
        self.ino as u64
    }
    fn mode(&self) -> u16 {
        self.mode
    }
    fn nlink(&self) -> u64 {
        self.fs
            .read_inode(self.ino)
            .map(|i| i.i_links_count as u64)
            .unwrap_or(1)
    }
    fn size(&self) -> u64 {
        match self.fs.read_inode(self.ino) {
            Ok(i) => i.i_size as u64,
            Err(_) => self.cached_size,
        }
    }
    fn readdir(&self) -> VfsResult<Vec<Arc<dyn VfsNode>>> {
        let entries = self.fs.read_dir_entries(self.ino).map_err(fs_to_vfs)?;
        let mut out = Vec::new();
        for (name, ino) in entries {
            if name == "." || name == ".." {
                continue;
            }
            out.push(Ext2Fs::node_for(&self.fs, ino, &name).map_err(fs_to_vfs)?);
        }
        Ok(out)
    }
    fn lookup(&self, name: &str) -> VfsResult<Arc<dyn VfsNode>> {
        let ino = self.fs.lookup_entry(self.ino, name).map_err(fs_to_vfs)?;
        Ext2Fs::node_for(&self.fs, ino, name).map_err(fs_to_vfs)
    }
    fn create_dir(&self, name: &str) -> VfsResult<Arc<dyn VfsNode>> {
        let ino = self.fs.create(self.ino, name, true).map_err(fs_to_vfs)?;
        Ext2Fs::node_for(&self.fs, ino, name).map_err(fs_to_vfs)
    }
    fn create_file(&self, name: &str) -> VfsResult<Arc<dyn VfsNode>> {
        let ino = self.fs.create(self.ino, name, false).map_err(fs_to_vfs)?;
        Ext2Fs::node_for(&self.fs, ino, name).map_err(fs_to_vfs)
    }
    fn create_symlink(&self, name: &str, target: &[u8]) -> VfsResult<Arc<dyn VfsNode>> {
        let ino = self
            .fs
            .create_symlink(self.ino, name, target)
            .map_err(fs_to_vfs)?;
        Ext2Fs::node_for(&self.fs, ino, name).map_err(fs_to_vfs)
    }
    fn link(&self, name: &str, target: &Arc<dyn VfsNode>) -> VfsResult<Arc<dyn VfsNode>> {
        let ino = target.fs_ino();
        if ino == 0 || ino > u32::MAX as u64 {
            return Err(VfsError::InvalidArgument);
        }
        self.fs
            .link(self.ino, name, ino as u32)
            .map_err(fs_to_vfs)?;
        Ext2Fs::node_for(&self.fs, ino as u32, name).map_err(fs_to_vfs)
    }
    fn remove(&self, name: &str) -> VfsResult<()> {
        self.fs.unlink(self.ino, name).map_err(fs_to_vfs)
    }
    fn rename(
        &self,
        old_name: &str,
        new_parent: &Arc<dyn VfsNode>,
        new_name: &str,
    ) -> VfsResult<()> {
        // The destination must be a directory of *this* filesystem: a rename
        // cannot cross a mount, and copying is not renaming.
        let Some(dest) = new_parent.as_any().downcast_ref::<Ext2Dir>() else {
            return Err(VfsError::NotSupported);
        };
        if !Arc::ptr_eq(&self.fs, &dest.fs) {
            return Err(VfsError::NotSupported);
        }
        self.fs
            .rename(self.ino, old_name, dest.ino, new_name)
            .map_err(fs_to_vfs)
    }
    fn sync(&self) {
        // Writes are synchronous, but a device flush keeps the trait's promise.
        let _ = self.fs.dev.flush();
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

/// A regular file node.
struct Ext2File {
    fs: Arc<Ext2Fs>,
    ino: u32,
    name: String,
    cached_size: u64,
    mode: u16,
}

impl VfsNode for Ext2File {
    fn name(&self) -> &str {
        &self.name
    }
    fn is_directory(&self) -> bool {
        false
    }
    fn fs_stat(&self) -> Option<FsStat> {
        Some(self.fs.fs_stat())
    }
    fn fs_ino(&self) -> u64 {
        self.ino as u64
    }
    fn mode(&self) -> u16 {
        self.mode
    }
    fn nlink(&self) -> u64 {
        self.fs
            .read_inode(self.ino)
            .map(|i| i.i_links_count as u64)
            .unwrap_or(1)
    }
    fn size(&self) -> u64 {
        match self.fs.read_inode(self.ino) {
            Ok(i) => i.i_size as u64,
            Err(_) => self.cached_size,
        }
    }
    fn read(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        self.fs.read_file(self.ino, offset, buf).map_err(fs_to_vfs)
    }
    fn write(&self, offset: u64, buf: &[u8]) -> VfsResult<usize> {
        self.fs.write_file(self.ino, offset, buf).map_err(fs_to_vfs)
    }
    fn truncate(&self, size: u64) -> VfsResult<()> {
        self.fs.truncate_file(self.ino, size).map_err(fs_to_vfs)
    }
    fn sync(&self) {
        let _ = self.fs.dev.flush();
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

/// A symbolic link (ext2 `S_IFLNK`).
///
/// The node carries no readable content: `read`/`write`/`truncate` stay
/// `NotSupported` because for a *fast* link those bytes are the target text
/// living in `i_block`, not data blocks. `size()` is `i_size`, i.e. the target
/// length — what `lstat` reports and `readlink` may return.
struct Ext2Symlink {
    fs: Arc<Ext2Fs>,
    ino: u32,
    name: String,
    cached_size: u64,
    mode: u16,
}

impl VfsNode for Ext2Symlink {
    fn name(&self) -> &str {
        &self.name
    }
    fn is_directory(&self) -> bool {
        false
    }
    fn fs_stat(&self) -> Option<FsStat> {
        Some(self.fs.fs_stat())
    }
    fn fs_ino(&self) -> u64 {
        self.ino as u64
    }
    fn mode(&self) -> u16 {
        self.mode
    }
    fn nlink(&self) -> u64 {
        self.fs
            .read_inode(self.ino)
            .map(|i| i.i_links_count as u64)
            .unwrap_or(1)
    }
    fn size(&self) -> u64 {
        match self.fs.read_inode(self.ino) {
            Ok(i) => i.i_size as u64,
            Err(_) => self.cached_size,
        }
    }
    fn is_symlink(&self) -> bool {
        true
    }
    fn read_link(&self) -> Option<String> {
        self.fs
            .read_symlink(self.ino)
            .ok()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

/// An inode of a kind this writer never creates (a foreign image's device node,
/// fifo, socket, ...): visible and stat-able, never readable as a file, so no
/// unknown `i_mode` can reach the regular-file block map.
struct Ext2Opaque {
    ino: u32,
    name: String,
    cached_size: u64,
    mode: u16,
}

impl VfsNode for Ext2Opaque {
    fn name(&self) -> &str {
        &self.name
    }
    fn is_directory(&self) -> bool {
        false
    }
    fn fs_ino(&self) -> u64 {
        self.ino as u64
    }
    fn mode(&self) -> u16 {
        self.mode
    }
    fn size(&self) -> u64 {
        self.cached_size
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

// `align4` is used by `dir`, not here; the import keeps the geometry helpers in
// one module without a dead-code allow per item.
const _: fn(usize) -> usize = align4;
// `read_struct`/`read_u32`/`read_u16`/`write_u32` are re-used by the sibling
// modules through `structs::` paths; silence "unused import" for the re-exports
// that only some block sizes exercise.
const _: fn(&[u8], usize) -> u16 = read_u16;
const _: fn(&[u8], usize) -> u32 = read_u32;
