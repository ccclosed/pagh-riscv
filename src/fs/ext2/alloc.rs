//! ext2 bitmap primitives: lowest-clear-bit allocation, mark/clear/test, and a
//! population count over the *addressable* span of a bitmap block.
//!
//! ext2's bitmaps are the allocation truth. A set bit means **in use** for both
//! the block and the inode bitmap (the opposite polarity of this kernel's own
//! frame bitmap in `src/memory/pmm.rs`, which is why the rule is stated here
//! once). Bit `k` of a block bitmap stands for block
//! `s_first_data_block + k` inside that block group; bit `k` of the inode
//! bitmap stands for the 1-based inode `k + 1`.
//!
//! The padding bits past the group's declared span (`s_blocks_per_group`, or
//! `s_inodes_per_group` for the last group) are **set** by `format`/`mount`, so
//! a lowest-clear-bit scan can never hand out a block past the filesystem end.
//! These helpers work on a buffer that is exactly one bitmap block long, so they
//! do not need the block size as a parameter.

#![allow(dead_code)]

/// Find and set the lowest clear bit in `bitmap`, returning its index.
///
/// `None` means every addressable bit is already set; the caller reports
/// `FsError::OutOfSpace`. The scan is bounded by `bitmap.len() * 8`, i.e. one
/// filesystem block's worth of bits.
pub fn alloc_bit(bitmap: &mut [u8]) -> Option<u32> {
    for (byte_idx, byte) in bitmap.iter_mut().enumerate() {
        if *byte != 0xFF {
            let bit = (!*byte).trailing_zeros(); // 0..=7; `byte != 0xff` => < 8
            *byte |= 1u8 << bit;
            // `byte_idx * 8 + bit` cannot overflow u32 for a real block size
            // (at most 4096 * 8 bits), but the multiplication is written in
            // `usize` and converted once.
            return u32::try_from(byte_idx * 8 + bit as usize).ok();
        }
    }
    None
}

/// Set bit `idx` (mark used). Returns `true` when it was previously clear.
pub fn set_bit(bitmap: &mut [u8], idx: u32) -> bool {
    let byte = (idx / 8) as usize;
    let bit = (idx % 8) as u8;
    let Some(slot) = bitmap.get_mut(byte) else {
        return false;
    };
    let was_clear = *slot & (1 << bit) == 0;
    *slot |= 1 << bit;
    was_clear
}

/// Clear bit `idx` (mark free). Returns `true` when it was previously set.
pub fn clear_bit(bitmap: &mut [u8], idx: u32) -> bool {
    let byte = (idx / 8) as usize;
    let bit = (idx % 8) as u8;
    let Some(slot) = bitmap.get_mut(byte) else {
        return false;
    };
    let was_set = *slot & (1 << bit) != 0;
    *slot &= !(1 << bit);
    was_set
}

/// Test whether bit `idx` is set (in use). Out-of-range indices are "not set",
/// never a panic: a corrupt on-disk count must not take the machine down.
pub fn test_bit(bitmap: &[u8], idx: u32) -> bool {
    let byte = (idx / 8) as usize;
    let bit = (idx % 8) as u8;
    match bitmap.get(byte) {
        Some(slot) => *slot & (1 << bit) != 0,
        None => false,
    }
}

/// Count the set bits in `[0, span)` — used to derive a group's true free count
/// from its bitmap (the boot-time sanity check and the self-tests).
pub fn count_set_bits(bitmap: &[u8], span: u32) -> u32 {
    let mut count = 0u32;
    for idx in 0..span {
        if test_bit(bitmap, idx) {
            count = count.saturating_add(1);
        }
    }
    count
}
