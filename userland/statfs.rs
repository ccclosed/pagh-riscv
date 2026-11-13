//! The `statfs(2)`/`fstatfs(2)` acceptance fixture: a static RV64 program that
//! asks the kernel for filesystem capacity through the native ABI.
//!
//! The record is pagh's own 96-byte `Statfs` (see `README.md` → native ABI and
//! `src/user/syscall.rs`), not Linux's 120-byte layout: ten little-endian `u64`
//! fields (`magic`, `block_size`, `blocks_total`, `blocks_free`,
//! `blocks_available`, `files_total`, `files_free`, `fsid`, `name_len`,
//! `flags`) followed by a NUL-padded 16-byte `type_name`.
//!
//! On a boot with a disk the fixture checks, byte for byte:
//!
//! * `statfs("/mnt")` reports ext2: magic `0xEF53`, a power-of-two block size,
//!   `blocks_free <= blocks_total`, `blocks_available == blocks_free`,
//!   `files_free <= files_total`, `name_len == 255`, `flags == 0` and
//!   `type_name == "ext2"`;
//! * the numbers track reality: creating a file consumes one inode and at least
//!   one data block (`free` counts go down), and unlinking it returns them
//!   (`files_free` back to the start, the data block freed);
//! * `fstatfs` on a directory fd gives exactly the same record as `statfs` on
//!   its path; on `/proc` it reports `procfs` with zero counts; on a console
//!   descriptor it is `-ENOTSUP` and on a bad fd `-EBADF`;
//! * the error matrix: missing path `-ENOENT`, the synthetic root `-ENOTSUP`,
//!   relative path `-EINVAL`, a NULL record `-EFAULT`; `/proc/version` is a
//!   synthetic file and answers `procfs` with zero counts.
//!
//! A boot without `/mnt` is not a failure of the fixture: it prints
//! `statfs: no /mnt` and exits 0, so `run statfs` is meaningful on the serial
//! console too (the selftest `user-statfs` SKIPs in that configuration).
//!
//! No allocator, no `core::fmt`, no libc: syscall wrappers over `ecall` and
//! fixed stack buffers. Build with `userland/build.sh`.

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicU32, Ordering};

core::arch::global_asm!(
    r#"
    .section .text.entry, "ax", @progbits
    .globl _start
_start:
    call    statfs_main
    li      a7, 93
    ecall
1:  j       1b
"#
);

// ─── syscall wrappers ──────────────────────────────────────────────────────

fn syscall6(
    number: usize,
    a0: usize,
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
    a5: usize,
) -> isize {
    let result: isize;
    // SAFETY: `ecall` is the only door from U-mode into the kernel; the number
    // goes in `a7`, the arguments in `a0..a5` and the result comes back in `a0`.
    // No `nostack` option: the kernel writes the `Statfs` record into a buffer
    // passed by address, and that buffer lives on this frame.
    unsafe {
        core::arch::asm!(
            "ecall",
            in("a7") number,
            inlateout("a0") a0 => result,
            in("a1") a1,
            in("a2") a2,
            in("a3") a3,
            in("a4") a4,
            in("a5") a5,
        )
    };
    result
}

fn syscall4(number: usize, a0: usize, a1: usize, a2: usize, a3: usize) -> isize {
    syscall6(number, a0, a1, a2, a3, 0, 0)
}

fn syscall3(number: usize, a0: usize, a1: usize, a2: usize) -> isize {
    syscall6(number, a0, a1, a2, 0, 0, 0)
}

fn syscall2(number: usize, a0: usize, a1: usize) -> isize {
    syscall6(number, a0, a1, 0, 0, 0, 0)
}

fn syscall1(number: usize, a0: usize) -> isize {
    syscall6(number, a0, 0, 0, 0, 0, 0)
}

// Generic RISC-V numbers for the calls this fixture makes.
const SYS_WRITE: usize = 64;
const SYS_OPENAT: usize = 56;
const SYS_CLOSE: usize = 57;
const SYS_UNLINKAT: usize = 35;
const SYS_STATFS: usize = 43;
const SYS_FSTATFS: usize = 44;

const O_RDONLY: usize = 0;
const O_WRONLY: usize = 1;
const O_CREAT: usize = 0x40;

const ENOENT: isize = -2;
const EBADF: isize = -9;
const EFAULT: isize = -14;
const EINVAL: isize = -22;
const ENOTSUP: isize = -95;

const EXT2_MAGIC: u64 = 0xEF53;
const NAME_MAX: u64 = 255;

const PROBE: &[u8] = b"/mnt/statfs-probe.txt\0";

/// The native `Statfs` record: 10 little-endian `u64`s and a 16-byte type name.
#[repr(C)]
#[derive(Clone, Copy)]
struct Statfs {
    magic: u64,
    block_size: u64,
    blocks_total: u64,
    blocks_free: u64,
    blocks_available: u64,
    files_total: u64,
    files_free: u64,
    fsid: u64,
    name_len: u64,
    flags: u64,
    type_name: [u8; 16],
}

impl Statfs {
    const fn zeroed() -> Self {
        Statfs {
            magic: 0,
            block_size: 0,
            blocks_total: 0,
            blocks_free: 0,
            blocks_available: 0,
            files_total: 0,
            files_free: 0,
            fsid: 0,
            name_len: 0,
            flags: 0,
            type_name: [0; 16],
        }
    }

    fn type_name(&self) -> &[u8] {
        let length = self
            .type_name
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(self.type_name.len());
        &self.type_name[..length]
    }

    fn same_as(&self, other: &Statfs) -> bool {
        self.magic == other.magic
            && self.block_size == other.block_size
            && self.blocks_total == other.blocks_total
            && self.blocks_free == other.blocks_free
            && self.blocks_available == other.blocks_available
            && self.files_total == other.files_total
            && self.files_free == other.files_free
            && self.fsid == other.fsid
            && self.name_len == other.name_len
            && self.flags == other.flags
            && self.type_name == other.type_name
    }
}

/// Exit codes are the verdict: each failing check has its own.
mod code {
    pub const OK: i32 = 0;
    pub const EXT2: i32 = 31;
    pub const FREE_SPACE: i32 = 32;
    pub const FSTATFS: i32 = 33;
    pub const PROC: i32 = 34;
    pub const ERRORS: i32 = 35;
    pub const OUTPUT: i32 = 36;
}

// ─── output ────────────────────────────────────────────────────────────────

fn write_bytes(bytes: &[u8]) -> bool {
    let mut offset = 0usize;
    while offset < bytes.len() {
        let written = syscall3(
            SYS_WRITE,
            1,
            bytes[offset..].as_ptr() as usize,
            bytes.len() - offset,
        );
        if written <= 0 {
            return false;
        }
        offset += written as usize;
    }
    true
}

fn fail(label: &[u8], code: i32) -> i32 {
    let _ = write_bytes(b"statfs: FAIL ");
    let _ = write_bytes(label);
    let _ = write_bytes(b"\n");
    code
}

fn ok(label: &[u8]) -> bool {
    write_bytes(b"statfs: ")
        && write_bytes(label)
        && write_bytes(b" ok\n")
}

// ─── small formatting helpers (no `core::fmt` in this fixture) ─────────────

fn push(out: &mut [u8], len: &mut usize, bytes: &[u8]) {
    for &byte in bytes {
        if *len < out.len() {
            out[*len] = byte;
            *len += 1;
        }
    }
}

fn push_u64(out: &mut [u8], len: &mut usize, mut value: u64) {
    let mut digits = [0u8; 20];
    let mut count = 0usize;
    if value == 0 {
        push(out, len, b"0");
        return;
    }
    while value > 0 && count < digits.len() {
        digits[count] = b'0' + (value % 10) as u8;
        count += 1;
        value /= 10;
    }
    for index in (0..count).rev() {
        push(out, len, &digits[index..index + 1]);
    }
}

/// One human-readable line: `statfs: <path> <type> block=… blocks=<free>/<total>
/// files=<free>/<total> namelen=<n> fsid=<n>`.
fn report(path: &[u8], stat: &Statfs) {
    let mut out = [0u8; 192];
    let mut len = 0usize;
    push(&mut out, &mut len, b"statfs: ");
    push(&mut out, &mut len, path);
    push(&mut out, &mut len, b" ");
    push(&mut out, &mut len, stat.type_name());
    push(&mut out, &mut len, b" block=");
    push_u64(&mut out, &mut len, stat.block_size);
    push(&mut out, &mut len, b" blocks=");
    push_u64(&mut out, &mut len, stat.blocks_free);
    push(&mut out, &mut len, b"/");
    push_u64(&mut out, &mut len, stat.blocks_total);
    push(&mut out, &mut len, b" files=");
    push_u64(&mut out, &mut len, stat.files_free);
    push(&mut out, &mut len, b"/");
    push_u64(&mut out, &mut len, stat.files_total);
    push(&mut out, &mut len, b" namelen=");
    push_u64(&mut out, &mut len, stat.name_len);
    push(&mut out, &mut len, b" fsid=");
    push_u64(&mut out, &mut len, stat.fsid);
    push(&mut out, &mut len, b"\n");
    let _ = write_bytes(&out[..len]);
}

// ─── calls ─────────────────────────────────────────────────────────────────

fn statfs(path: &[u8], buffer: &mut Statfs) -> isize {
    debug_assert!(path.last() == Some(&0));
    syscall2(
        SYS_STATFS,
        path.as_ptr() as usize,
        buffer as *mut Statfs as usize,
    )
}

fn fstatfs(fd: usize, buffer: &mut Statfs) -> isize {
    syscall2(SYS_FSTATFS, fd, buffer as *mut Statfs as usize)
}

fn open(path: &[u8], flags: usize) -> isize {
    debug_assert!(path.last() == Some(&0));
    syscall4(SYS_OPENAT, 0, path.as_ptr() as usize, flags, 0)
}

fn close(fd: usize) {
    let _ = syscall1(SYS_CLOSE, fd);
}

fn unlink(path: &[u8]) -> isize {
    debug_assert!(path.last() == Some(&0));
    syscall3(SYS_UNLINKAT, 0, path.as_ptr() as usize, 0)
}

/// Write `count` bytes of `fill` to `fd`, looping over short writes.
fn write_all(fd: usize, bytes: &[u8]) -> bool {
    let mut offset = 0usize;
    while offset < bytes.len() {
        let written = syscall3(
            SYS_WRITE,
            fd,
            bytes[offset..].as_ptr() as usize,
            bytes.len() - offset,
        );
        if written <= 0 {
            return false;
        }
        offset += written as usize;
    }
    true
}

// ─── checks ────────────────────────────────────────────────────────────────

fn check_ext2(buffer: &mut Statfs) -> Result<(), i32> {
    if statfs(b"/mnt\0", buffer) != 0 {
        return Err(fail(b"statfs /mnt", code::EXT2));
    }
    report(b"/mnt", buffer);
    let block_size = buffer.block_size;
    let power_of_two = block_size.is_power_of_two() && (1024..=4096).contains(&block_size);
    if buffer.magic != EXT2_MAGIC
        || !power_of_two
        || buffer.blocks_total == 0
        || buffer.blocks_free > buffer.blocks_total
        || buffer.blocks_available != buffer.blocks_free
        || buffer.files_total == 0
        || buffer.files_free > buffer.files_total
        || buffer.name_len != NAME_MAX
        || buffer.flags != 0
        || buffer.type_name() != b"ext2"
    {
        return Err(fail(b"/mnt fields", code::EXT2));
    }
    Ok(())
}

/// The counts must follow real allocations, not sit still: one file consumes an
/// inode and at least one data block, and unlinking it gives them back.
fn check_free_space(buffer: &mut Statfs) -> Result<(), i32> {
    // A crashed previous run may have left the probe behind.
    let _ = unlink(PROBE);
    if statfs(b"/mnt\0", buffer) != 0 {
        return Err(fail(b"statfs before", code::FREE_SPACE));
    }
    let before = *buffer;

    let fd = open(PROBE, O_CREAT | O_WRONLY);
    if fd < 0 {
        return Err(fail(b"create probe", code::FREE_SPACE));
    }
    let fd = fd as usize;
    let block = [b'x'; 4096];
    let wrote = write_all(fd, &block);
    close(fd);
    if !wrote {
        return Err(fail(b"write probe", code::FREE_SPACE));
    }

    if statfs(b"/mnt\0", buffer) != 0 {
        return Err(fail(b"statfs after write", code::FREE_SPACE));
    }
    let after_write = *buffer;
    if after_write.files_free + 1 != before.files_free
        || after_write.blocks_free + 1 > before.blocks_free
        || after_write.blocks_free >= before.blocks_free
    {
        return Err(fail(b"allocated counts", code::FREE_SPACE));
    }

    if unlink(PROBE) != 0 {
        return Err(fail(b"unlink probe", code::FREE_SPACE));
    }
    if statfs(b"/mnt\0", buffer) != 0 {
        return Err(fail(b"statfs after unlink", code::FREE_SPACE));
    }
    let after_free = *buffer;
    if after_free.files_free != before.files_free
        || after_free.blocks_free < after_write.blocks_free + 1
        || after_free.blocks_free > before.blocks_free
    {
        return Err(fail(b"freed counts", code::FREE_SPACE));
    }
    Ok(())
}

fn check_fstatfs(buffer: &mut Statfs, scratch: &mut Statfs) -> Result<(), i32> {
    // `fstatfs` on a directory descriptor and `statfs` on its path are the same
    // filesystem: the records must match field for field.
    let fd = open(b"/mnt\0", O_RDONLY);
    if fd < 0 {
        return Err(fail(b"openat /mnt", code::FSTATFS));
    }
    let fd = fd as usize;
    let from_fd = fstatfs(fd, buffer);
    close(fd);
    if from_fd != 0 || statfs(b"/mnt\0", scratch) != 0 || !buffer.same_as(scratch) {
        return Err(fail(b"fstatfs /mnt", code::FSTATFS));
    }

    // A console descriptor is a real fd with no filesystem behind it.
    if fstatfs(1, buffer) != ENOTSUP {
        return Err(fail(b"fstatfs console", code::FSTATFS));
    }
    // And a number that is not a descriptor at all is -EBADF.
    if fstatfs(999, buffer) != EBADF {
        return Err(fail(b"fstatfs bad fd", code::FSTATFS));
    }
    Ok(())
}

fn check_procfs(buffer: &mut Statfs) -> Result<(), i32> {
    if statfs(b"/proc\0", buffer) != 0 {
        return Err(fail(b"statfs /proc", code::PROC));
    }
    report(b"/proc", buffer);
    if buffer.magic != 0
        || buffer.blocks_total != 0
        || buffer.blocks_free != 0
        || buffer.files_total != 0
        || buffer.files_free != 0
        || buffer.name_len != NAME_MAX
        || buffer.flags != 0
        || buffer.type_name() != b"procfs"
    {
        return Err(fail(b"/proc fields", code::PROC));
    }
    // A file inside the synthetic filesystem reports the same filesystem.
    let fd = open(b"/proc/version\0", O_RDONLY);
    if fd < 0 {
        return Err(fail(b"openat /proc/version", code::PROC));
    }
    let fd = fd as usize;
    let result = fstatfs(fd, buffer);
    close(fd);
    if result != 0 || buffer.type_name() != b"procfs" || buffer.blocks_total != 0 {
        return Err(fail(b"fstatfs /proc/version", code::PROC));
    }
    Ok(())
}

fn check_errors(buffer: &mut Statfs) -> Result<(), i32> {
    if statfs(b"/mnt/nonexistent\0", buffer) != ENOENT {
        return Err(fail(b"statfs missing path", code::ERRORS));
    }
    // The synthetic root is not a filesystem: there is nothing to describe.
    if statfs(b"/\0", buffer) != ENOTSUP {
        return Err(fail(b"statfs root", code::ERRORS));
    }
    // The ABI has no cwd: a relative path is -EINVAL, like every other call.
    if statfs(b"mnt\0", buffer) != EINVAL {
        return Err(fail(b"statfs relative path", code::ERRORS));
    }
    // A NULL record pointer is rejected by the user-pointer validator.
    let null_record = syscall2(SYS_STATFS, b"/mnt\0".as_ptr() as usize, 0);
    if null_record != EFAULT {
        return Err(fail(b"statfs null buf", code::ERRORS));
    }
    let null_path = syscall2(
        SYS_STATFS,
        0,
        buffer as *mut Statfs as usize,
    );
    if null_path != EFAULT {
        return Err(fail(b"statfs null path", code::ERRORS));
    }
    Ok(())
}

// ─── entry ─────────────────────────────────────────────────────────────────

/// One keeper for the output-failure flag; a fixture that cannot print is a
/// failure even when every syscall check passed.
static OUTPUT_OK: AtomicU32 = AtomicU32::new(1);

fn progress(label: &[u8]) {
    if !ok(label) {
        OUTPUT_OK.store(0, Ordering::Relaxed);
    }
}

#[no_mangle]
pub extern "C" fn statfs_main() -> i32 {
    let mut buffer = Statfs::zeroed();
    let mut scratch = Statfs::zeroed();

    // No disk is a valid boot configuration: this fixture's job is to describe a
    // filesystem, so it says so and exits cleanly (the selftest skips instead).
    if statfs(b"/mnt\0", &mut buffer) == ENOENT {
        let _ = write_bytes(b"statfs: no /mnt\n");
        return code::OK;
    }

    if let Err(code) = check_ext2(&mut buffer) {
        return code;
    }
    progress(b"ext2");

    if let Err(code) = check_free_space(&mut buffer) {
        return code;
    }
    progress(b"free-space");

    if let Err(code) = check_fstatfs(&mut buffer, &mut scratch) {
        return code;
    }
    progress(b"fstatfs");

    if let Err(code) = check_procfs(&mut buffer) {
        return code;
    }
    progress(b"procfs");

    if let Err(code) = check_errors(&mut buffer) {
        return code;
    }
    progress(b"errors");

    if OUTPUT_OK.load(Ordering::Relaxed) == 0 {
        return code::OUTPUT;
    }
    let _ = write_bytes(b"statfs: done\n");
    code::OK
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    let _ = write_bytes(b"statfs: FAIL panic\n");
    let _ = syscall1(93, code::OUTPUT as usize);
    loop {}
}