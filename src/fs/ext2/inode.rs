//! ext2 inode block map (read path): resolve a file byte offset to the ext2
//! block that holds it, walking the classic 12 direct + single/double/triple
//! indirect pointers.
//!
//! The write/allocate-past-EOF half of the map lives in `ext2::Tx::map_or_alloc`
//! (it allocates blocks and updates the bitmaps inside one transaction). Keeping
//! the read walk separate is what lets `read` take no write lock at all.
//!
//! `ptrs_per_block` is `block_size / 4`, i.e. 256/512/1024 for the three block
//! sizes this port supports, so it comes from the mounted filesystem
//! ([`super::Ext2Fs::block_size`]) rather than a constant.

#![allow(dead_code)]

use super::structs::{read_u32, Ext2Inode};
use super::Ext2Fs;
use crate::fs::FsError;

/// Return `Some(p)` when `p != 0`, else `None` (an unallocated hole).
#[inline]
fn nonzero(p: u32) -> Option<u32> {
    if p == 0 {
        None
    } else {
        Some(p)
    }
}

/// Read pointer slot `index` from indirect block `block`.
fn read_ptr(fs: &Ext2Fs, block: u32, index: u32) -> Result<u32, FsError> {
    if block == 0 {
        return Ok(0);
    }
    let buf = fs.read_fs_block(block as u64)?;
    Ok(read_u32(&buf, index as usize * 4))
}

/// Resolve the ext2 block holding file byte offset `off`, or `None` when that
/// logical block has not been allocated (a hole or past the last block).
///
/// `Err` only on an underlying device error: a corrupt pointer chain is reported
/// as "no block" (reading a hole gives zeros), because `read` must stay total —
/// a file with a broken map reads short, it does not kill the machine.
pub fn block_for_offset(fs: &Ext2Fs, inode: &Ext2Inode, off: u64) -> Result<Option<u32>, FsError> {
    let bs = fs.block_size() as u64;
    let ppb = fs.ptrs_per_block() as u64;
    let mut idx = off / bs;

    // 12 direct pointers.
    let direct = core::cmp::min(inode.i_block.len() as u64 - 3, 12);
    if idx < direct {
        return Ok(nonzero(inode.i_block[idx as usize]));
    }
    idx -= direct;

    // Single indirect.
    if idx < ppb {
        let l1 = inode.i_block[12];
        if l1 == 0 {
            return Ok(None);
        }
        let ptr = read_ptr(fs, l1, idx as u32)?;
        return Ok(nonzero(ptr));
    }
    idx -= ppb;

    // Double indirect.
    if idx < ppb.saturating_mul(ppb) {
        let l2 = inode.i_block[13];
        if l2 == 0 {
            return Ok(None);
        }
        let l1 = read_ptr(fs, l2, (idx / ppb) as u32)?;
        if l1 == 0 {
            return Ok(None);
        }
        let ptr = read_ptr(fs, l1, (idx % ppb) as u32)?;
        return Ok(nonzero(ptr));
    }
    idx = idx.saturating_sub(ppb.saturating_mul(ppb));

    // Triple indirect.
    let ppb2 = ppb.saturating_mul(ppb);
    if idx < ppb2.saturating_mul(ppb) {
        let l3 = inode.i_block[14];
        if l3 == 0 {
            return Ok(None);
        }
        let l2 = read_ptr(fs, l3, (idx / ppb2) as u32)?;
        if l2 == 0 {
            return Ok(None);
        }
        let l1 = read_ptr(fs, l2, ((idx / ppb) % ppb) as u32)?;
        if l1 == 0 {
            return Ok(None);
        }
        let ptr = read_ptr(fs, l1, (idx % ppb) as u32)?;
        return Ok(nonzero(ptr));
    }

    // Beyond the triple-indirect range: not representable (and a 32-bit `i_size`
    // never reaches it on the block sizes supported here).
    Ok(None)
}
