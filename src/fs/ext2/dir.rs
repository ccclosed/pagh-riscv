//! ext2 directory entries: iteration over a `rec_len` chain, insertion (split an
//! existing record's slack) and removal (merge a record's length into its
//! predecessor) within a single filesystem block.
//!
//! Invariant maintained over every block: the `rec_len` chain exactly tiles
//! `[0, block.len())` — no gaps, no overlap, no entry crossing the block
//! boundary, and the last entry's `rec_len` reaches the end of the block.
//!
//! The block size is not a constant of this module: every function takes exactly
//! one filesystem block as a slice and derives the geometry from
//! `block.len()`. That is what lets the filesystem mount host `mkfs.ext2`
//! images with 1024-, 2048- and 4096-byte blocks from the same code.
//!
//! `file_type` is written only when the superblock advertises the incompatible
//! FILETYPE feature (`format` clears it, host `mkfs.ext2` sets it); a reader
//! ignores it either way, so an image written by one and grown by the other
//! stays valid.

#![allow(dead_code)]

use alloc::string::String;
use alloc::vec::Vec;

use super::structs::{
    align4, read_u32, write_u16, write_u32, Ext2DirEntryHeader, FT_DIR, FT_UNKNOWN,
};
use crate::fs::FsError;

/// Minimum record length for a name of `name_len` bytes: the 8-byte header plus
/// the name, rounded up to the 4-byte `rec_len` unit.
#[inline]
pub fn min_rec_len(name_len: usize) -> usize {
    align4(8 + name_len)
}

/// A decoded directory entry within a block.
#[derive(Clone)]
pub struct DirEntry {
    /// Byte offset of the entry inside its block.
    pub pos: usize,
    /// Inode number; 0 means a free (unused) record.
    pub inode: u32,
    /// Record length, covering this entry and its trailing slack.
    pub rec_len: u16,
    /// Length of the name in bytes (`0` for a free record).
    pub name_len: u8,
    /// `d_type` byte; meaningful only with the FILETYPE feature.
    pub file_type: u8,
    /// Decoded name; empty for a free record.
    pub name: String,
}

fn read_header(block: &[u8], pos: usize) -> Ext2DirEntryHeader {
    Ext2DirEntryHeader {
        inode: read_u32(block, pos),
        rec_len: super::structs::read_u16(block, pos + 4),
        name_len: block.get(pos + 6).copied().unwrap_or(0),
        file_type: block.get(pos + 7).copied().unwrap_or(0),
    }
}

fn write_header(
    block: &mut [u8],
    pos: usize,
    inode: u32,
    rec_len: u16,
    name_len: u8,
    file_type: u8,
) {
    write_u32(block, pos, inode);
    write_u16(block, pos + 4, rec_len);
    if let Some(slot) = block.get_mut(pos + 6) {
        *slot = name_len;
    }
    if let Some(slot) = block.get_mut(pos + 7) {
        *slot = file_type;
    }
}

/// Walk the entries of one directory block, returning all decoded records
/// (including free ones, whose `inode == 0`).
///
/// `FsError::Corrupt` when the `rec_len` chain does not tile the block exactly:
/// following a corrupt chain is how a filesystem turns a bad sector into a
/// wild pointer, so the walk refuses instead.
pub fn iter_entries(block: &[u8]) -> Result<Vec<DirEntry>, FsError> {
    let bs = block.len();
    if bs < 16 {
        return Err(FsError::Corrupt);
    }
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < bs {
        if pos + 8 > bs {
            return Err(FsError::Corrupt);
        }
        let h = read_header(block, pos);
        let rec_len = h.rec_len as usize;
        // rec_len must be a positive multiple of 4 and stay within the block.
        if rec_len < 8 || rec_len % 4 != 0 || pos + rec_len > bs {
            return Err(FsError::Corrupt);
        }
        let name = if h.inode != 0 {
            let nl = h.name_len as usize;
            if 8 + nl > rec_len {
                return Err(FsError::Corrupt);
            }
            // Lossy: this kernel's paths are UTF-8 `String`s, so a name that is
            // not valid UTF-8 (only possible in a foreign image) is shown with
            // replacement characters; it can still be removed by that name.
            String::from_utf8_lossy(&block[pos + 8..pos + 8 + nl]).into_owned()
        } else {
            String::new()
        };
        out.push(DirEntry {
            pos,
            inode: h.inode,
            rec_len: h.rec_len,
            name_len: h.name_len,
            file_type: h.file_type,
            name,
        });
        pos += rec_len;
    }
    if pos != bs {
        return Err(FsError::Corrupt);
    }
    Ok(out)
}

/// Find an entry by name; returns `(inode, file_type)` on a match.
pub fn find(block: &[u8], name: &str) -> Result<Option<(u32, u8)>, FsError> {
    for e in iter_entries(block)? {
        if e.inode != 0 && e.name == name {
            return Ok(Some((e.inode, e.file_type)));
        }
    }
    Ok(None)
}

/// Initialize a freshly allocated directory block as one free record spanning the
/// whole block.
pub fn init_empty_block(block: &mut [u8]) {
    for b in block.iter_mut() {
        *b = 0;
    }
    let bs = block.len() as u16;
    write_header(block, 0, 0, bs, 0, FT_UNKNOWN);
}

/// Write a `.`/`..` pair into a freshly zeroed directory block (used by `format`
/// and `mkdir`). `self_ino` is this directory, `parent_ino` its parent.
///
/// `.` gets the minimum record (12 bytes for a one-character name) and `..`
/// absorbs the rest of the block, which leaves the block a single insertion away
/// from a valid chain.
pub fn init_dot_entries(block: &mut [u8], self_ino: u32, parent_ino: u32, has_filetype: bool) {
    for b in block.iter_mut() {
        *b = 0;
    }
    let bs = block.len();
    let ft = if has_filetype { FT_DIR } else { FT_UNKNOWN };
    // "." — rec_len = align4(8 + 1) = 12.
    let dot_rec = min_rec_len(1) as u16;
    write_header(block, 0, self_ino, dot_rec, 1, ft);
    if let Some(slot) = block.get_mut(8) {
        *slot = b'.';
    }
    // ".." — rec_len extends to the end of the block.
    let ddot_pos = dot_rec as usize;
    if ddot_pos + 8 <= bs {
        let ddot_rec = (bs - ddot_pos) as u16;
        write_header(block, ddot_pos, parent_ino, ddot_rec, 2, ft);
        if let Some(slot) = block.get_mut(ddot_pos + 8) {
            *slot = b'.';
        }
        if let Some(slot) = block.get_mut(ddot_pos + 9) {
            *slot = b'.';
        }
    }
}

/// Attempt to insert `(name, ino)` into the directory block by splitting the
/// slack of an existing record.
///
/// Returns `Ok(true)` on success and `Ok(false)` when the block has no room (the
/// caller then grows the directory by another block).
pub fn insert_into_block(
    block: &mut [u8],
    name: &str,
    ino: u32,
    has_filetype: bool,
    file_type: u8,
) -> Result<bool, FsError> {
    let name_bytes = name.as_bytes();
    if name_bytes.len() > 255 {
        return Err(FsError::NameTooLong);
    }
    if name_bytes.is_empty() {
        return Err(FsError::Corrupt);
    }
    let needed = min_rec_len(name_bytes.len());
    let ft = if has_filetype { file_type } else { FT_UNKNOWN };

    for e in iter_entries(block)? {
        // A free record's "ideal" size is 0: the whole of it is slack and the new
        // entry simply takes it over. A live record keeps its own minimum.
        let ideal = if e.inode == 0 {
            0
        } else {
            min_rec_len(e.name_len as usize)
        };
        let Some(slack) = (e.rec_len as usize).checked_sub(ideal) else {
            return Err(FsError::Corrupt);
        };
        if slack >= needed {
            let new_pos = e.pos + ideal;
            let new_rec_len = (e.rec_len as usize - ideal) as u16;
            write_header(block, new_pos, ino, new_rec_len, name_bytes.len() as u8, ft);
            if let Some(dst) = block.get_mut(new_pos + 8..new_pos + 8 + name_bytes.len()) {
                dst.copy_from_slice(name_bytes);
            }
            if e.inode != 0 {
                // Shrink the donor record to its ideal size.
                let cur = read_header(block, e.pos);
                write_header(
                    block,
                    e.pos,
                    cur.inode,
                    ideal as u16,
                    cur.name_len,
                    cur.file_type,
                );
            }
            return Ok(true);
        }
    }
    Ok(false)
}

/// Remove the entry named `name` from the block, merging its `rec_len` into the
/// previous record (or freeing it in place when it is the first record).
///
/// Returns the removed entry's inode, or `None` when no such entry exists.
pub fn remove_from_block(block: &mut [u8], name: &str) -> Result<Option<u32>, FsError> {
    let entries = iter_entries(block)?;
    let mut prev: Option<DirEntry> = None;
    for e in &entries {
        if e.inode != 0 && e.name == name {
            let removed = e.inode;
            match &prev {
                Some(p) => {
                    // Merge this record's rec_len into the previous record. The
                    // sum is bounded by the block length, so the u16 cannot wrap.
                    let merged = p.rec_len.saturating_add(e.rec_len);
                    let p = read_header(block, p.pos);
                    write_header(
                        block,
                        prev_pos(&prev, e.pos),
                        p.inode,
                        merged,
                        p.name_len,
                        p.file_type,
                    );
                }
                None => {
                    // First record: keep its rec_len, clear its inode so the chain
                    // still tiles the block.
                    write_header(block, e.pos, 0, e.rec_len, 0, FT_UNKNOWN);
                }
            }
            return Ok(Some(removed));
        }
        prev = Some(e.clone());
    }
    Ok(None)
}

/// The byte offset of the previous record: `prev.pos`, or the current entry's
/// position when there is no previous record (unused branch).
#[inline]
fn prev_pos(prev: &Option<DirEntry>, fallback: usize) -> usize {
    match prev {
        Some(p) => p.pos,
        None => fallback,
    }
}

/// Count the live (non-free, non-dot) entries in a directory block.
pub fn live_entry_count(block: &[u8]) -> Result<usize, FsError> {
    let mut n = 0usize;
    for e in iter_entries(block)? {
        if e.inode != 0 && e.name != "." && e.name != ".." {
            n = n.saturating_add(1);
        }
    }
    Ok(n)
}

/// Point this directory's `..` entry at `parent_ino` (directory rename).
///
/// `false` when the block has no `..` record, which a well-formed directory
/// block always does; the caller reports corruption.
pub fn rewrite_dotdot(block: &mut [u8], parent_ino: u32) -> Result<bool, FsError> {
    let entries = iter_entries(block)?;
    for e in &entries {
        if e.inode != 0 && e.name == ".." {
            write_u32(block, e.pos, parent_ino);
            return Ok(true);
        }
    }
    Ok(false)
}
