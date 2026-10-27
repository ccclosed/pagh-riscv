//! The filesystem's in-QEMU self-tests, run by the shell's `selftest` next to
//! the memory suite.
//!
//! Contract, identical in spirit to `src/memory/selftest.rs`: **non-destructive,
//! deterministic, idempotent**. Every check builds its own RAM `MemBlock`
//! device, formats it, exercises the filesystem and drops it; nothing touches the
//! device `/mnt` is mounted from except the explicit read-only check at the end,
//! which only reads. Two runs in one boot must print the same free-frame ledger,
//! which is why every `Arc` is dropped before the function returns.
//!
//! Why a RAM device instead of the QEMU disk: the M5 filesystem has to be
//! testable before (and without) a real block driver, and a test that can only
//! ever see one disk image cannot create a deliberately corrupt filesystem. The
//! `MemBlock` seam is exactly for this (`src/drivers/block.rs`).
//!
//! Each check prints `SELFTEST ext2-<name> PASS/FAIL`; the summary line is
//! `SELFTEST ext2 summary: n/n passed` (deliberately *not* `SELFTEST SUMMARY:`,
//! which is the memory suite's line and is asserted by tooling).

use alloc::sync::Arc;
use alloc::vec;

use crate::drivers::block::{BlockDevice, MemBlock};
use crate::fs::ext2::structs::{EXT2_MAGIC, EXT2_ROOT_INO};
use crate::fs::ext2::Ext2Fs;
use crate::fs::format_policy::{self, Layout};
use crate::fs::FsError;
use crate::vfs::{self};

/// Sectors of the RAM device every check formats: 120 × 512 B = 60 KiB.
///
/// That is 15 blocks of the 4096-byte block size `format` picks (60 KiB ≥ 32
/// KiB), with 8 free data blocks — enough for every operation below, and small
/// enough that the whole device is one heap chunk (this kernel's heap refuses a
/// single allocation above 64 KiB on purpose).
const RAM_SECTORS: u64 = 120;

/// Filesystem block size a 60 KiB device is formatted with.
const RAM_BLOCK_SIZE: u32 = 4096;

/// A fresh, zeroed RAM device.
fn ram_device() -> Arc<dyn BlockDevice> {
    Arc::new(MemBlock::new("selftest-ram", RAM_SECTORS))
}

/// Format a fresh RAM device and mount it.
fn ram_fs() -> Option<Arc<Ext2Fs>> {
    let dev = ram_device();
    match Ext2Fs::format(Arc::clone(&dev)) {
        Ok(()) => Ext2Fs::mount_fs(dev).ok(),
        Err(_) => None,
    }
}

/// `kprintln!` a failure detail and clear the check's `ok` flag.
macro_rules! check {
    ($ok:ident, $cond:expr, $($arg:tt)*) => {
        if !$cond {
            crate::kprintln!($($arg)*);
            $ok = false;
        }
    };
}

/// Run every filesystem check. Returns the number that passed.
pub fn run() -> usize {
    crate::vfs::init();
    let (before, total) = crate::memory::stats();
    crate::kprintln!("[fs selftest] free frames before: {} of {}", before, total);

    let checks: [(&str, fn() -> bool); 11] = [
        ("format", format_and_mount),
        ("write-read", write_read_roundtrip),
        ("dirs", mkdir_remove),
        ("truncate", truncate_file),
        ("symlink", symlinks),
        ("rename", rename_entries),
        ("remount", remount_persistence),
        ("free-space", free_space_accounting),
        ("corruption", corrupt_input), // no panic on bad on-disk data
        ("policy", format_policy),
        ("on-device", on_device_readonly),
    ];
    let mut passed = 0usize;
    for (name, check) in checks {
        let ok = check();
        if ok {
            passed += 1;
        }
        crate::kprintln!(
            "SELFTEST ext2-{} {}",
            name,
            if ok { "PASS" } else { "FAIL" }
        );
    }

    let (after, _) = crate::memory::stats();
    crate::kprintln!("SELFTEST ext2 summary: {}/{} passed", passed, checks.len());
    crate::kprintln!(
        "[fs selftest] PMM hygiene: free frames {} -> {}",
        before,
        after
    );
    passed
}

/// Format a blank device, then check the on-disk superblock and that the tree
/// (`/`, `lost+found`) is reachable both directly and through a VFS mount.
fn format_and_mount() -> bool {
    let mut ok = true;
    let dev = ram_device();
    check!(
        ok,
        Ext2Fs::format(Arc::clone(&dev)).is_ok(),
        "[fs selftest] format failed"
    );
    let fs = match Ext2Fs::mount_fs(dev) {
        Ok(fs) => fs,
        Err(e) => {
            crate::kprintln!("[fs selftest] mount after format failed: {}", e.as_str());
            return false;
        }
    };
    let sb = fs.superblock();
    check!(
        ok,
        sb.s_magic == EXT2_MAGIC,
        "[fs selftest] superblock magic {:#x}",
        sb.s_magic
    );
    check!(
        ok,
        fs.block_size() as u32 == RAM_BLOCK_SIZE,
        "[fs selftest] formatted block size {}",
        fs.block_size()
    );
    check!(
        ok,
        sb.s_blocks_count == (RAM_SECTORS as u32 * 512 / RAM_BLOCK_SIZE),
        "[fs selftest] block count {}",
        sb.s_blocks_count
    );
    check!(
        ok,
        sb.s_free_blocks_count > 0 && sb.s_free_inodes_count > 0,
        "[fs selftest] free blocks {} / inodes {}",
        sb.s_free_blocks_count,
        sb.s_free_inodes_count
    );

    // The tree: root (inode 2) is a directory and lost+found is in it.
    match fs.read_dir_entries(EXT2_ROOT_INO) {
        Ok(entries) => {
            let has_lost = entries.iter().any(|(name, _)| name == "lost+found");
            check!(
                ok,
                has_lost,
                "[fs selftest] lost+found missing from {:?} entries",
                entries.len()
            );
        }
        Err(e) => {
            crate::kprintln!("[fs selftest] readdir(/) failed: {}", e.as_str());
            ok = false;
        }
    }

    // The VFS layer: mount the same root under a test name and walk to a child.
    let root = fs.root_node();
    if vfs::mount_at("/fstest", root).is_err() {
        crate::kprintln!("[fs selftest] mount_at(/fstest) failed");
        return false;
    }
    let walked = vfs::walk("/fstest/lost+found", true);
    check!(
        ok,
        matches!(&walked, Ok(node) if node.is_directory() && node.name() == "lost+found"),
        "[fs selftest] vfs walk to lost+found failed"
    );
    check!(
        ok,
        vfs::walk("/fstest/..", true).is_ok(),
        "[fs selftest] vfs walk through .. failed"
    );
    let _ = vfs::unmount_at("/fstest");
    ok
}

/// Create, write, read, overwrite-shorter and append: the four things a file
/// must do, checked byte for byte.
fn write_read_roundtrip() -> bool {
    let mut ok = true;
    let Some(fs) = ram_fs() else {
        return false;
    };
    let root = fs.root_node();
    let file = match root.create_file("hello.txt") {
        Ok(f) => f,
        Err(e) => {
            crate::kprintln!("[fs selftest] create failed: {}", e.as_str());
            return false;
        }
    };
    let text = b"hello, ext2\n";
    check!(
        ok,
        file.write(0, text) == Ok(text.len()),
        "[fs selftest] write returned an error"
    );
    check!(
        ok,
        file.size() == text.len() as u64,
        "[fs selftest] size {} after write {}",
        file.size(),
        text.len()
    );
    let mut buf = [0u8; 32];
    let n = file.read(0, &mut buf).unwrap_or(0);
    check!(
        ok,
        n == text.len() && &buf[..n] == text,
        "[fs selftest] read back {} bytes, not the written text",
        n
    );
    check!(
        ok,
        file.read(text.len() as u64, &mut buf) == Ok(0),
        "[fs selftest] read past EOF returned data"
    );

    // Replacement, not overwrite-in-place: the old tail must be gone.
    check!(
        ok,
        vfs::replace_contents(&file, b"short") == Ok(5),
        "[fs selftest] replace_contents failed"
    );
    check!(
        ok,
        file.size() == 5,
        "[fs selftest] size {} after shortening to 5",
        file.size()
    );

    // Append at an explicit offset.
    check!(
        ok,
        file.write(5, b"-tail") == Ok(5),
        "[fs selftest] append failed"
    );
    let mut buf = [0u8; 16];
    let n = file.read(0, &mut buf).unwrap_or(0);
    check!(
        ok,
        &buf[..n] == b"short-tail",
        "[fs selftest] content {:?} after append",
        core::str::from_utf8(&buf[..n]).unwrap_or("?")
    );
    ok
}

/// `mkdir`, a child file, `rm` of the file, `rmdir` of the directory, and the
/// non-empty-directory refusal.
fn mkdir_remove() -> bool {
    let mut ok = true;
    let Some(fs) = ram_fs() else {
        return false;
    };
    let root = fs.root_node();
    let dir = match root.create_dir("sub") {
        Ok(d) => d,
        Err(e) => {
            crate::kprintln!("[fs selftest] mkdir failed: {}", e.as_str());
            return false;
        }
    };
    check!(
        ok,
        dir.is_directory(),
        "[fs selftest] mkdir made a non-directory"
    );
    let file = match dir.create_file("x") {
        Ok(f) => f,
        Err(e) => {
            crate::kprintln!("[fs selftest] create in subdir failed: {}", e.as_str());
            return false;
        }
    };
    check!(
        ok,
        file.write(0, b"data") == Ok(4),
        "[fs selftest] write in subdir failed"
    );
    check!(
        ok,
        root.remove("sub").is_err(),
        "[fs selftest] removed a non-empty directory"
    );
    check!(
        ok,
        dir.remove("x").is_ok(),
        "[fs selftest] rm of file failed"
    );
    check!(
        ok,
        root.remove("sub").is_ok(),
        "[fs selftest] rmdir of empty directory failed"
    );
    check!(
        ok,
        matches!(root.remove("sub"), Err(vfs::VfsError::NotFound)),
        "[fs selftest] removing a removed directory did not report NotFound"
    );
    check!(
        ok,
        matches!(root.lookup("sub"), Err(vfs::VfsError::NotFound)),
        "[fs selftest] removed directory still resolves"
    );
    ok
}

/// `truncate`: shrink to a byte inside a block, grow across a block boundary and
/// verify the zero fill of the gap.
fn truncate_file() -> bool {
    let mut ok = true;
    let Some(fs) = ram_fs() else {
        return false;
    };
    let root = fs.root_node();
    let file = match root.create_file("t.bin") {
        Ok(f) => f,
        Err(_) => return false,
    };
    let mut data = vec![0u8; 9000];
    for (i, b) in data.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    check!(
        ok,
        file.write(0, &data) == Ok(data.len()),
        "[fs selftest] truncate fixture write failed"
    );
    check!(
        ok,
        file.size() == 9000,
        "[fs selftest] size {} != 9000",
        file.size()
    );

    check!(
        ok,
        file.truncate(100).is_ok(),
        "[fs selftest] shrink failed"
    );
    check!(
        ok,
        file.size() == 100,
        "[fs selftest] size {} != 100",
        file.size()
    );
    let mut buf = [0u8; 128];
    let n = file.read(0, &mut buf).unwrap_or(0);
    check!(
        ok,
        n == 100 && buf[..n] == data[..100],
        "[fs selftest] survivor bytes differ after shrink"
    );

    check!(ok, file.truncate(8192).is_ok(), "[fs selftest] grow failed");
    check!(
        ok,
        file.size() == 8192,
        "[fs selftest] size {} != 8192",
        file.size()
    );
    let mut gap = [0xffu8; 64];
    let n = file.read(200, &mut gap).unwrap_or(0);
    check!(
        ok,
        n == 64 && gap.iter().all(|&b| b == 0),
        "[fs selftest] grown gap is not zeroed"
    );
    ok
}

/// Symbolic links: fast (inline target) and slow (target in a data block), plus
/// resolution through the VFS walker.
fn symlinks() -> bool {
    let mut ok = true;
    let Some(fs) = ram_fs() else {
        return false;
    };
    let root = fs.root_node();
    if root.create_file("target.txt").is_err() {
        return false;
    }
    let target = match root.create_symlink("link", b"target.txt") {
        Ok(n) => n,
        Err(e) => {
            crate::kprintln!("[fs selftest] symlink create failed: {}", e.as_str());
            return false;
        }
    };
    check!(
        ok,
        target.is_symlink(),
        "[fs selftest] link is not a symlink"
    );
    check!(
        ok,
        target.read_link().as_deref() == Some("target.txt"),
        "[fs selftest] fast link target {:?}",
        target.read_link()
    );
    check!(
        ok,
        target.size() == "target.txt".len() as u64,
        "[fs selftest] fast link size {}",
        target.size()
    );

    // 100 bytes: one past the 60-byte inline capacity, so the slow layout.
    let long: alloc::string::String = core::iter::repeat('x').take(100).collect();
    let slow = match root.create_symlink("slow-link", long.as_bytes()) {
        Ok(n) => n,
        Err(e) => {
            crate::kprintln!("[fs selftest] slow symlink create failed: {}", e.as_str());
            return false;
        }
    };
    check!(
        ok,
        slow.read_link().as_deref() == Some(long.as_str()),
        "[fs selftest] slow link target differs"
    );
    check!(
        ok,
        matches!(
            target.read(0, &mut [0u8; 4]),
            Err(vfs::VfsError::NotSupported)
        ),
        "[fs selftest] reading a symlink node did not report NotSupported"
    );

    // Through the VFS: `/fstest/link` must resolve to target.txt.
    let root2 = fs.root_node();
    if vfs::mount_at("/fstest", root2).is_err() {
        return false;
    }
    let walked = vfs::walk("/fstest/link", true);
    check!(
        ok,
        matches!(&walked, Ok(node) if node.name() == "target.txt"),
        "[fs selftest] vfs walk did not follow the symlink"
    );
    // ...and without following, it is still the link.
    let unfollowed = vfs::walk("/fstest/link", false);
    check!(
        ok,
        matches!(&unfollowed, Ok(node) if node.is_symlink()),
        "[fs selftest] vfs no-follow walk lost the symlink"
    );
    let _ = vfs::unmount_at("/fstest");
    ok
}

/// `rename`: file to a new name, into a subdirectory, replacing an existing
/// file, a directory rename, and the refusal to move a directory into itself.
fn rename_entries() -> bool {
    let mut ok = true;
    let Some(fs) = ram_fs() else {
        return false;
    };
    let root = fs.root_node();
    let a = match root.create_file("a.txt") {
        Ok(f) => f,
        Err(_) => return false,
    };
    let _ = a.write(0, b"A");

    check!(
        ok,
        root.rename("a.txt", &root, "b.txt").is_ok(),
        "[fs selftest] rename file failed"
    );
    check!(
        ok,
        matches!(root.lookup("a.txt"), Err(vfs::VfsError::NotFound)),
        "[fs selftest] old name still resolves after rename"
    );
    match root.lookup("b.txt") {
        Ok(n) => {
            let mut buf = [0u8; 4];
            let n = n.read(0, &mut buf).unwrap_or(0);
            check!(
                ok,
                &buf[..n] == b"A",
                "[fs selftest] renamed file content {:?}",
                core::str::from_utf8(&buf[..n]).unwrap_or("?")
            );
        }
        Err(_) => {
            crate::kprintln!("[fs selftest] renamed file not found");
            ok = false;
        }
    }

    // Into a subdirectory.
    let d = match root.create_dir("d") {
        Ok(d) => d,
        Err(_) => return false,
    };
    check!(
        ok,
        root.rename("b.txt", &d, "c.txt").is_ok(),
        "[fs selftest] rename into subdir failed"
    );
    check!(
        ok,
        matches!(d.lookup("c.txt"), Ok(_)),
        "[fs selftest] renamed entry missing in subdir"
    );

    // Replace an existing file atomically through rename.
    let _ = root.create_file("one").and_then(|f| f.write(0, b"one"));
    let _ = root.create_file("two").and_then(|f| f.write(0, b"two"));
    check!(
        ok,
        root.rename("one", &root, "two").is_ok(),
        "[fs selftest] rename-over-existing failed"
    );
    match root.lookup("two") {
        Ok(n) => {
            let mut buf = [0u8; 8];
            let n = n.read(0, &mut buf).unwrap_or(0);
            check!(
                ok,
                &buf[..n] == b"one",
                "[fs selftest] rename did not replace"
            );
        }
        Err(_) => ok = false,
    }

    // Directory rename, then the cycle refusal.
    let e = match root.create_dir("e") {
        Ok(e) => e,
        Err(_) => return false,
    };
    check!(
        ok,
        root.rename("d", &root, "d2").is_ok(),
        "[fs selftest] directory rename failed"
    );
    check!(
        ok,
        matches!(root.lookup("d2"), Ok(_)),
        "[fs selftest] renamed directory missing"
    );
    check!(
        ok,
        matches!(
            root.rename("e2", &e, "inside"),
            Err(vfs::VfsError::NotFound)
        ),
        "[fs selftest] renaming a missing entry did not report NotFound"
    );
    // `mv e e/inside` must be refused: a directory cannot move into its subtree.
    match root.rename("e", &e, "inside") {
        Err(vfs::VfsError::InvalidArgument) | Err(vfs::VfsError::NotSupported) => {}
        other => {
            crate::kprintln!(
                "[fs selftest] directory-into-itself rename was not refused: {:?}",
                other.map(|_| ()).map_err(|e| e)
            );
            ok = false;
        }
    }
    check!(
        ok,
        matches!(root.lookup("e"), Ok(_)),
        "[fs selftest] refused rename moved the directory anyway"
    );
    ok
}

/// Write through one mount, drop everything, mount the same device again and read
/// the file back: the proof that a write reached the device, not just a cache.
fn remount_persistence() -> bool {
    let mut ok = true;
    let dev = ram_device();
    if Ext2Fs::format(Arc::clone(&dev)).is_err() {
        return false;
    }
    {
        let fs = match Ext2Fs::mount_fs(Arc::clone(&dev)) {
            Ok(fs) => fs,
            Err(_) => return false,
        };
        let root = fs.root_node();
        let file = match root.create_file("persist.txt") {
            Ok(f) => f,
            Err(_) => return false,
        };
        let payload = b"survives a remount";
        let wrote = file.write(0, payload);
        check!(
            ok,
            wrote == Ok(payload.len()),
            "[fs selftest] persistence fixture write failed: {:?} (wanted {:?})",
            wrote,
            Ok::<usize, crate::vfs::VfsError>(payload.len())
        );
        // `fs`/`root`/`file` drop at the end of this block.
    }
    let fs = match Ext2Fs::mount_fs(dev) {
        Ok(fs) => fs,
        Err(e) => {
            crate::kprintln!("[fs selftest] remount failed: {}", e.as_str());
            return false;
        }
    };
    let root = fs.root_node();
    match root.lookup("persist.txt") {
        Ok(file) => {
            let mut buf = [0u8; 32];
            let n = file.read(0, &mut buf).unwrap_or(0);
            check!(
                ok,
                &buf[..n] == b"survives a remount",
                "[fs selftest] remounted content differs: {:?}",
                core::str::from_utf8(&buf[..n]).unwrap_or("?")
            );
        }
        Err(_) => {
            crate::kprintln!("[fs selftest] file missing after remount");
            ok = false;
        }
    }
    ok
}

/// Free-space and free-inode counters must follow allocations and frees exactly.
fn free_space_accounting() -> bool {
    let mut ok = true;
    let Some(fs) = ram_fs() else {
        return false;
    };
    let root = fs.root_node();
    let sb0 = fs.superblock();
    check!(
        ok,
        fs.fs_stat().blocks_free == sb0.s_free_blocks_count as u64,
        "[fs selftest] fs_stat free blocks disagree with the superblock"
    );

    let file = match root.create_file("space.bin") {
        Ok(f) => f,
        Err(_) => return false,
    };
    let sb1 = fs.superblock();
    check!(
        ok,
        sb1.s_free_inodes_count + 1 == sb0.s_free_inodes_count,
        "[fs selftest] inode count {} -> {} for one file",
        sb0.s_free_inodes_count,
        sb1.s_free_inodes_count
    );
    check!(
        ok,
        file.write(0, b"one block") == Ok(9),
        "[fs selftest] accounting fixture write failed"
    );
    let sb2 = fs.superblock();
    check!(
        ok,
        sb2.s_free_blocks_count + 1 == sb1.s_free_blocks_count,
        "[fs selftest] block count {} -> {} for one data block",
        sb1.s_free_blocks_count,
        sb2.s_free_blocks_count
    );

    check!(
        ok,
        root.remove("space.bin").is_ok(),
        "[fs selftest] unlink failed"
    );
    let sb3 = fs.superblock();
    check!(
        ok,
        sb3.s_free_blocks_count == sb0.s_free_blocks_count
            && sb3.s_free_inodes_count == sb0.s_free_inodes_count,
        "[fs selftest] counters not restored: {} blocks / {} inodes, started {} / {}",
        sb3.s_free_blocks_count,
        sb3.s_free_inodes_count,
        sb0.s_free_blocks_count,
        sb0.s_free_inodes_count
    );
    ok
}

/// Corrupt on-disk data must produce `Err`, never a panic (a panic is a dead
/// machine: `panic = "abort"` plus a parked hart).
fn corrupt_input() -> bool {
    let mut ok = true;

    // 1. A device that is not ext2 at all.
    let junk = Arc::new(MemBlock::with_pattern("junk", RAM_SECTORS, 0xAA)) as Arc<dyn BlockDevice>;
    check!(
        ok,
        Ext2Fs::mount_fs(junk).is_err(),
        "[fs selftest] mounted a device full of 0xAA"
    );

    // Format once; the rest of the checks corrupt that image in place.
    let dev = ram_device();
    if Ext2Fs::format(Arc::clone(&dev)).is_err() {
        return false;
    }

    // 2. A bad superblock magic (byte 1024 + 0x38 = 1080, sector 2 offset 56).
    {
        let mut sector = [0u8; 512];
        if dev.read_block(2, &mut sector).is_ok() {
            sector[56] ^= 0xFF;
            let _ = dev.write_block(2, &sector);
        }
        check!(
            ok,
            Ext2Fs::mount_fs(Arc::clone(&dev)).is_err(),
            "[fs selftest] mounted an image with a broken magic"
        );
        // Repair it so the following checks start from a valid image.
        let mut sector = [0u8; 512];
        if dev.read_block(2, &mut sector).is_ok() {
            sector[56] ^= 0xFF;
            let _ = dev.write_block(2, &sector);
        }
    }

    // 3. An unsupported block size: s_log_block_size = 3 (8192-byte blocks).
    {
        let mut sector = [0u8; 512];
        if dev.read_block(2, &mut sector).is_ok() {
            sector[24] = 3;
            let _ = dev.write_block(2, &sector);
        }
        check!(
            ok,
            Ext2Fs::mount_fs(Arc::clone(&dev)).is_err(),
            "[fs selftest] mounted an image with an 8192-byte block size"
        );
        let mut sector = [0u8; 512];
        if dev.read_block(2, &mut sector).is_ok() {
            sector[24] = 2; // restore s_log_block_size = 2 (4096)
            let _ = dev.write_block(2, &sector);
        }
    }

    // 4. A broken directory `rec_len` chain: the mount succeeds (mount reads
    //    metadata only) but reading the directory must report corruption.
    {
        let fs = match Ext2Fs::mount_fs(Arc::clone(&dev)) {
            Ok(fs) => fs,
            Err(_) => return false,
        };
        let root_blk = match fs.read_inode(EXT2_ROOT_INO) {
            Ok(inode) => inode.i_block[0] as u64,
            Err(_) => return false,
        };
        let sectors = fs.sectors_per_block() as u64;
        let mut block = vec![0u8; fs.block_size()];
        if dev.read_block(root_blk * sectors, &mut block).is_ok() {
            // rec_len = 3: neither a multiple of 4 nor at least 8.
            block[4] = 3;
            block[5] = 0;
            let _ = dev.write_block(root_blk * sectors, &block);
        }
        let err = fs.read_dir_entries(EXT2_ROOT_INO);
        check!(
            ok,
            matches!(err, Err(FsError::Corrupt)),
            "[fs selftest] broken rec_len chain was not reported as Corrupt"
        );
        // The VFS also reports it instead of panicking.
        let root = fs.root_node();
        check!(
            ok,
            matches!(root.readdir(), Err(vfs::VfsError::Corrupt)),
            "[fs selftest] VFS readdir of a corrupt directory did not fail cleanly"
        );
    }
    ok
}

/// The boot format policy: blank is formatted, everything with data is refused,
/// and a valid filesystem is never formatted even after a failed mount.
fn format_policy() -> bool {
    let mut ok = true;
    let blank = ram_device();
    check!(
        ok,
        format_policy::probe_device(&*blank) == Layout::Blank,
        "[fs selftest] a zeroed device was not classified blank"
    );
    check!(
        ok,
        format_policy::format_allowed(Layout::Blank, false).is_ok(),
        "[fs selftest] a blank device was not formattable"
    );

    let junk =
        Arc::new(MemBlock::with_pattern("junk-policy", RAM_SECTORS, 0x5A)) as Arc<dyn BlockDevice>;
    check!(
        ok,
        format_policy::probe_device(&*junk) == Layout::Foreign,
        "[fs selftest] a patterned device was not classified foreign"
    );
    check!(
        ok,
        format_policy::format_allowed(Layout::Foreign, false).is_err(),
        "[fs selftest] a foreign device was formattable"
    );

    // After formatting, the device reads back as ext2 and must be refused.
    let dev = ram_device();
    if Ext2Fs::format(Arc::clone(&dev)).is_err() {
        return false;
    }
    check!(
        ok,
        format_policy::probe_device(&*dev) == Layout::Ext2,
        "[fs selftest] formatted device not classified ext2-like"
    );
    let valid = Ext2Fs::has_valid_superblock(&*dev);
    check!(
        ok,
        valid,
        "[fs selftest] formatted superblock does not validate"
    );
    check!(
        ok,
        format_policy::format_allowed(Layout::Ext2, valid).is_err(),
        "[fs selftest] an existing filesystem was formattable"
    );
    ok
}

/// Read-only checks against the *registered* device (the disk `/mnt` is mounted
/// from, when one is attached): a second mount of the same image, a superblock
/// read and a root listing — and a byte-for-byte proof that none of it wrote.
///
/// Skipped (reported PASS with a reason) when the machine has no block device,
/// which is the normal CI configuration.
fn on_device_readonly() -> bool {
    let Some(dev) = crate::drivers::block::get() else {
        crate::kprintln!("[fs selftest] on-device: no block device (skipped)");
        return true;
    };
    let mut ok = true;
    // 4 KiB of the device's start: the superblock lives at byte 1024, so this
    // covers it for every supported block size.
    let mut before = [0u8; 4096];
    if dev.read_block(0, &mut before).is_err() {
        crate::kprintln!("[fs selftest] on-device: cannot read the device (skipped)");
        return true;
    }
    let fs = match Ext2Fs::mount_fs(Arc::clone(&dev)) {
        Ok(fs) => fs,
        Err(_) => {
            crate::kprintln!(
                "[fs selftest] on-device: {} carries no mountable ext2 (skipped)",
                dev.name()
            );
            return true;
        }
    };
    let sb = fs.superblock();
    check!(
        ok,
        sb.s_magic == EXT2_MAGIC,
        "[fs selftest] on-device superblock magic {:#x}",
        sb.s_magic
    );
    match fs.read_dir_entries(EXT2_ROOT_INO) {
        Ok(entries) => {
            crate::kprintln!(
                "[fs selftest] on-device: {} root has {} entries",
                dev.name(),
                entries.len()
            );
        }
        Err(e) => {
            crate::kprintln!("[fs selftest] on-device readdir failed: {}", e.as_str());
            ok = false;
        }
    }
    let mut after = [0u8; 4096];
    if dev.read_block(0, &mut after).is_ok() {
        check!(
            ok,
            before == after,
            "[fs selftest] the read-only mount changed the device's first 4 KiB"
        );
    }
    ok
}
