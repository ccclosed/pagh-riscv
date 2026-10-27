//! ext2 on-disk `#[repr(C)]` structures and the byte<->struct helpers.
//!
//! Every structure keeps the canonical ext2 field names and offsets, so an
//! image written here is a plain ext2 image to `e2fsprogs` and to Linux, and an
//! image written by host `mkfs.ext2 -t ext2` is readable here. The kernel runs
//! on RV64, which is little-endian like the ext2 format, so the in-memory
//! `#[repr(C)]` layout *is* the on-disk layout; the `const` size/offset asserts
//! below make a field reorder a compile error instead of a corrupt image.
//!
//! Unlike the x86 tree (which compiles the block size in as `BS = 4096`), the
//! port supports the three block sizes ext2 actually uses — 1024, 2048 and
//! 4096 bytes — because host `mkfs.ext2 -t ext2` picks 1024 by default on small
//! images and 4096 on large ones. Nothing here may assume a fixed block size:
//! address math takes the mounted superblock's geometry from `Ext2Fs`
//! ([`super::Ext2Fs::block_size`]), and the directory/bitmap helpers derive it
//! from the slice they are handed (a block buffer is always exactly one
//! filesystem block long).

#![allow(dead_code)]

/// Smallest ext2 block size (bytes).
pub const MIN_BLOCK_SIZE: usize = 1024;

/// Largest block size this port accepts (bytes). ext2 can encode up to 64 KiB
/// blocks; every image a `mkfs.ext2` on this host produces is 1024/2048/4096,
/// and a smaller accepted set is one less way to get the address math wrong.
pub const MAX_BLOCK_SIZE: usize = 4096;

/// Classic ext2 inode size for `s_rev_level == 0` (bytes).
pub const INODE_SIZE: usize = 128;

/// ext2 magic, stored at superblock offset 0x38.
pub const EXT2_MAGIC: u16 = 0xEF53;

/// Root directory inode number.
pub const EXT2_ROOT_INO: u32 = 2;

/// First non-reserved inode (inodes 1..=10 are reserved by the format).
pub const EXT2_FIRST_INO: u32 = 11;

/// Inode mode bits: type mask and the three types this writer creates.
pub const S_IFMT: u16 = 0xF000;
pub const S_IFREG: u16 = 0x8000;
pub const S_IFDIR: u16 = 0x4000;
pub const S_IFLNK: u16 = 0xA000;

/// `s_feature_compat` bits this code knows about (none is required to mount).
pub const FEATURE_COMPAT_DIR_INDEX: u32 = 0x20;

/// `s_feature_incompat` bits.
pub const FEATURE_INCOMPAT_FILETYPE: u32 = 0x2;

/// `s_feature_ro_compat` bits.
pub const FEATURE_RO_COMPAT_SPARSE_SUPER: u32 = 0x1;

/// Directory entry `file_type` values (`s_feature_incompat & FILETYPE`).
pub const FT_UNKNOWN: u8 = 0;
pub const FT_REG_FILE: u8 = 1;
pub const FT_DIR: u8 = 2;
pub const FT_SYMLINK: u8 = 7;

/// File type byte for an inode `i_mode` word.
pub fn file_type_of_mode(mode: u16) -> u8 {
    match mode & S_IFMT {
        S_IFREG => FT_REG_FILE,
        S_IFDIR => FT_DIR,
        S_IFLNK => FT_SYMLINK,
        _ => FT_UNKNOWN,
    }
}

/// Effective on-disk inode size for a mounted superblock.
///
/// Rev-0 images always carry 128-byte inodes; a rev-1 image declares the size in
/// `s_inode_size` (e.g. 256, which is what host `mkfs.ext2 -t ext2` writes).
/// Address math must honour it or every inode-table access lands on the wrong
/// slot. A bogus value is *not* trusted: callers validate the superblock at
/// mount ([`super::Ext2Fs::mount`]) and this function clamps defensively.
pub fn inode_size(sb: &Ext2SuperBlock) -> usize {
    let v = sb.s_inode_size as usize;
    // `!v.is_power_of_two()` covers 0 too; the explicit lower/upper bounds keep
    // a power-of-two-but-absurd value (e.g. 8192 on a 4096-byte-block fs) out.
    if sb.s_rev_level == 0 || v < INODE_SIZE || v > MAX_BLOCK_SIZE || !v.is_power_of_two() {
        return INODE_SIZE;
    }
    v
}

/// `ptrs_per_block` for a filesystem block size (`bs / 4`).
pub fn ptrs_per_block(bs: usize) -> u32 {
    (bs / 4) as u32
}

/// `align4(n)` rounds `n` up to a multiple of 4 (dirent `rec_len` unit).
#[inline]
pub fn align4(n: usize) -> usize {
    (n + 3) & !3
}

// ─── ext2 superblock (lives at byte offset 1024 of the filesystem) ──────────

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Ext2SuperBlock {
    pub s_inodes_count: u32,      // 0x00
    pub s_blocks_count: u32,      // 0x04
    pub s_r_blocks_count: u32,    // 0x08
    pub s_free_blocks_count: u32, // 0x0C
    pub s_free_inodes_count: u32, // 0x10
    pub s_first_data_block: u32,  // 0x14
    pub s_log_block_size: u32,    // 0x18
    pub s_log_frag_size: u32,     // 0x1C
    pub s_blocks_per_group: u32,  // 0x20
    pub s_frags_per_group: u32,   // 0x24
    pub s_inodes_per_group: u32,  // 0x28
    pub s_mtime: u32,             // 0x2C
    pub s_wtime: u32,             // 0x30
    pub s_mnt_count: u16,         // 0x34
    pub s_max_mnt_count: u16,     // 0x36
    pub s_magic: u16,             // 0x38 = 0xEF53
    pub s_state: u16,             // 0x3A
    pub s_errors: u16,            // 0x3C
    pub s_minor_rev_level: u16,   // 0x3E
    pub s_lastcheck: u32,         // 0x40
    pub s_checkinterval: u32,     // 0x44
    pub s_creator_os: u32,        // 0x48
    pub s_rev_level: u32,         // 0x4C
    pub s_def_resuid: u16,        // 0x50
    pub s_def_resgid: u16,        // 0x52
    pub s_first_ino: u32,         // 0x54
    pub s_inode_size: u16,        // 0x58
    pub s_block_group_nr: u16,    // 0x5A
    pub s_feature_compat: u32,    // 0x5C
    pub s_feature_incompat: u32,  // 0x60
    pub s_feature_ro_compat: u32, // 0x64
    pub s_uuid: [u8; 16],         // 0x68
    pub s_volume_name: [u8; 16],  // 0x78
}

const _: () = assert!(core::mem::size_of::<Ext2SuperBlock>() == 0x88);
// Field-offset locks for the fields whose address matters to the host tools.
const _: () = assert!(core::mem::offset_of!(Ext2SuperBlock, s_log_block_size) == 0x18);
const _: () = assert!(core::mem::offset_of!(Ext2SuperBlock, s_magic) == 0x38);
const _: () = assert!(core::mem::offset_of!(Ext2SuperBlock, s_inode_size) == 0x58);
const _: () = assert!(core::mem::offset_of!(Ext2SuperBlock, s_feature_incompat) == 0x60);

impl Ext2SuperBlock {
    /// The block size in bytes implied by `s_log_block_size`, or `None` when the
    /// field is outside the range this port supports (1024/2048/4096).
    pub fn block_size(&self) -> Option<usize> {
        match self.s_log_block_size {
            0 => Some(1024),
            1 => Some(2048),
            2 => Some(4096),
            _ => None,
        }
    }

    /// True when directory entries carry the `file_type` byte.
    pub fn has_filetype(&self) -> bool {
        self.s_feature_incompat & FEATURE_INCOMPAT_FILETYPE != 0
    }

    /// True when backup superblocks live only in groups 0, 1 and powers of
    /// 3/5/7. Only the primary copy is ever read or written here, so this is
    /// informational; it is used by `format` to decide that writing one copy per
    /// group is correct for the image this kernel creates (features cleared).
    pub fn has_sparse_super(&self) -> bool {
        self.s_feature_ro_compat & FEATURE_RO_COMPAT_SPARSE_SUPER != 0
    }
}

// ─── ext2 block group descriptor (32 bytes) ─────────────────────────────────

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Ext2GroupDesc {
    pub bg_block_bitmap: u32,
    pub bg_inode_bitmap: u32,
    pub bg_inode_table: u32,
    pub bg_free_blocks_count: u16,
    pub bg_free_inodes_count: u16,
    pub bg_used_dirs_count: u16,
    pub bg_pad: u16,
    pub bg_reserved: [u8; 12],
}

const _: () = assert!(core::mem::size_of::<Ext2GroupDesc>() == 32);

impl Ext2GroupDesc {
    pub const fn zeroed() -> Self {
        Ext2GroupDesc {
            bg_block_bitmap: 0,
            bg_inode_bitmap: 0,
            bg_inode_table: 0,
            bg_free_blocks_count: 0,
            bg_free_inodes_count: 0,
            bg_used_dirs_count: 0,
            bg_pad: 0,
            bg_reserved: [0; 12],
        }
    }
}

// ─── ext2 inode (128 bytes, the rev-0/1 fixed part) ─────────────────────────

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Ext2Inode {
    pub i_mode: u16,
    pub i_uid: u16,
    pub i_size: u32,
    pub i_atime: u32,
    pub i_ctime: u32,
    pub i_mtime: u32,
    pub i_dtime: u32,
    pub i_gid: u16,
    pub i_links_count: u16,
    pub i_blocks: u32, // count of 512-byte sectors
    pub i_flags: u32,
    pub i_osd1: u32,
    pub i_block: [u32; 15], // 0..=11 direct, 12 single, 13 double, 14 triple
    pub i_generation: u32,
    pub i_file_acl: u32,
    pub i_dir_acl: u32,
    pub i_faddr: u32,
    pub i_osd2: [u8; 12],
}

const _: () = assert!(core::mem::size_of::<Ext2Inode>() == 128);
const _: () = assert!(core::mem::offset_of!(Ext2Inode, i_block) == 40);

impl Ext2Inode {
    pub const fn zeroed() -> Self {
        Ext2Inode {
            i_mode: 0,
            i_uid: 0,
            i_size: 0,
            i_atime: 0,
            i_ctime: 0,
            i_mtime: 0,
            i_dtime: 0,
            i_gid: 0,
            i_links_count: 0,
            i_blocks: 0,
            i_flags: 0,
            i_osd1: 0,
            i_block: [0; 15],
            i_generation: 0,
            i_file_acl: 0,
            i_dir_acl: 0,
            i_faddr: 0,
            i_osd2: [0; 12],
        }
    }

    pub fn is_dir(&self) -> bool {
        self.i_mode & S_IFMT == S_IFDIR
    }

    pub fn is_reg(&self) -> bool {
        self.i_mode & S_IFMT == S_IFREG
    }

    /// A symbolic link: the target lives inline in `i_block` (fast,
    /// `i_blocks == 0`) or in one data block (slow). Layout math is in
    /// [`super::symlink`].
    pub fn is_symlink(&self) -> bool {
        self.i_mode & S_IFMT == S_IFLNK
    }
}

// ─── ext2 directory entry header (8 bytes; the name follows) ────────────────

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Ext2DirEntryHeader {
    pub inode: u32,
    pub rec_len: u16,
    pub name_len: u8,
    pub file_type: u8,
}

const _: () = assert!(core::mem::size_of::<Ext2DirEntryHeader>() == 8);

// ─── byte <-> struct helpers ────────────────────────────────────────────────

/// Read a `Copy` POD struct out of a byte buffer with an unaligned load.
///
/// # Safety
/// `bytes.len()` must be at least `size_of::<T>()`, and the bytes must be a
/// valid bit pattern for `T` — always true for the integer/array POD structs in
/// this module (no references, enums or padding constraints beyond plain bytes).
pub unsafe fn read_struct<T: Copy>(bytes: &[u8]) -> T {
    debug_assert!(bytes.len() >= core::mem::size_of::<T>());
    // SAFETY: the caller guarantees the length and that the bit pattern is
    // valid; `read_unaligned` makes no alignment assumption about `bytes`.
    unsafe { core::ptr::read_unaligned(bytes.as_ptr() as *const T) }
}

/// Write a `Copy` POD struct into a byte buffer with an unaligned store.
///
/// # Safety
/// `bytes.len()` must be at least `size_of::<T>()`.
pub unsafe fn write_struct<T: Copy>(bytes: &mut [u8], value: &T) {
    debug_assert!(bytes.len() >= core::mem::size_of::<T>());
    // SAFETY: as above; `write_unaligned` makes no alignment assumption.
    unsafe { core::ptr::write_unaligned(bytes.as_mut_ptr() as *mut T, *value) }
}

/// Read a little-endian `u16` at `off`, or 0 when `off + 2` is out of bounds.
///
/// The out-of-bounds answer is 0 rather than a panic on purpose: every caller in
/// this filesystem indexes a buffer whose length it has just validated against
/// the block size, but a corrupt on-disk field must never be able to kill the
/// machine, and a "0" is the format's own "absent" value.
pub fn read_u16(bytes: &[u8], off: usize) -> u16 {
    match (bytes.get(off), bytes.get(off + 1)) {
        (Some(a), Some(b)) => u16::from_le_bytes([*a, *b]),
        _ => 0,
    }
}

/// Read a little-endian `u32` at `off`, or 0 when out of bounds (see
/// [`read_u16`]).
pub fn read_u32(bytes: &[u8], off: usize) -> u32 {
    match bytes.get(off..off + 4) {
        Some(b) => u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        None => 0,
    }
}

/// Write a little-endian `u16` at `off`; out-of-range writes are dropped (the
/// callers write into a block-sized buffer they own).
pub fn write_u16(bytes: &mut [u8], off: usize, v: u16) {
    if let Some(b) = bytes.get_mut(off..off + 2) {
        b.copy_from_slice(&v.to_le_bytes());
    }
}

/// Write a little-endian `u32` at `off`; out-of-range writes are dropped.
pub fn write_u32(bytes: &mut [u8], off: usize, v: u32) {
    if let Some(b) = bytes.get_mut(off..off + 4) {
        b.copy_from_slice(&v.to_le_bytes());
    }
}
