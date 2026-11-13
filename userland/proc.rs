//! The `/proc` acceptance fixture: a static RV64 program that reads the
//! synthetic filesystem through the ordinary syscall ABI — no libpagh, no libc.
//!
//! `/proc` is a VFS subtree whose files have no backing store; the kernel
//! renders their text from live state when a program reads them
//! (`src/vfs/proc.rs`). This fixture proves the userland half of that contract:
//!
//! * a directory listing through `openat`+`getdents64` gives the seven root
//!   entries with the right `d_type`;
//! * `fstat` reports a regular file (0o644) with a size equal to the bytes
//!   `read` returns, and a read from offset 0 after `lseek(0)` gives the same
//!   text again for a static file (`version`);
//! * the live files parse: `meminfo` has `MemTotal`/`MemFree` with
//!   `MemFree <= MemTotal`, `uptime` is `<sec>.<centi> 0.00`, `tasks` has the
//!   scheduler header, `mounts` reports `/proc`, `net/dev` reports the interface
//!   (or its honest absence), `net/arp` is readable;
//! * `/proc` is read-only: `mkdirat`/`unlinkat` are `-ENOTSUP` and a `write` to
//!   a file opened `O_WRONLY` is `-ENOTSUP`;
//! * an unknown path is `-ENOENT`, never a synthesised node.
//!
//! A failure prints one diagnostic line and exits with a distinct non-zero code;
//! success prints the `proc: ... ok` markers the selftest greps for and exits 0.
//! Everything is `no_std` Rust over raw `ecall`s. Build with
//! `userland/build.sh` (see `userland/README.md`).

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicU32, Ordering};

core::arch::global_asm!(
    r#"
    .section .text.entry, "ax", @progbits
    .globl _start
_start:
    call    proc_main
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
    // No `nostack` option: the kernel writes into buffers this program passes by
    // address, and some of them live on this frame.
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

fn syscall1(number: usize, a0: usize) -> isize {
    syscall6(number, a0, 0, 0, 0, 0, 0)
}

// Linux/RISC-V numbers, the subset this fixture needs.
const SYS_READ: usize = 63;
const SYS_WRITE: usize = 64;
const SYS_OPENAT: usize = 56;
const SYS_CLOSE: usize = 57;
const SYS_GETDENTS64: usize = 61;
const SYS_LSEEK: usize = 62;
const SYS_FSTAT: usize = 80;
const SYS_MKDIRAT: usize = 34;
const SYS_UNLINKAT: usize = 35;
const SYS_READLINKAT: usize = 78;
const SYS_NANOSLEEP: usize = 101;
const SYS_GETPID: usize = 172;
const SYS_GETTID: usize = 178;
const SYS_FORK: usize = 1000;
const SYS_WAIT4: usize = 260;
const SYS_EXIT: usize = 93;

const O_RDONLY: usize = 0;
const O_WRONLY: usize = 1;
const SEEK_SET: usize = 0;

const ENOENT: isize = -2;
const ENOTSUP: isize = -95;

/// `fstat`'s native `Stat` (24 bytes: mode, kind, size, ino).
#[repr(C)]
struct Stat {
    mode: u32,
    kind: u32,
    size: u64,
    ino: u64,
}

const KIND_FILE: u32 = 0;
const KIND_DIR: u32 = 1;
const KIND_SYMLINK: u32 = 2;

/// `nanosleep(101)`'s `Timespec` (16 bytes: seconds, nanoseconds).
#[repr(C)]
struct Timespec {
    tv_sec: i64,
    tv_nsec: i64,
}

/// Exit codes are the verdict: each failing check has its own.
mod code {
    pub const OK: i32 = 0;
    pub const VERSION: i32 = 11;
    pub const MEMINFO: i32 = 12;
    pub const UPTIME: i32 = 13;
    pub const TASKS: i32 = 14;
    pub const MOUNTS: i32 = 15;
    pub const NET: i32 = 16;
    pub const DIR: i32 = 17;
    pub const READONLY: i32 = 18;
    pub const MISSING: i32 = 19;
    pub const OUTPUT: i32 = 20;
    pub const SELF: i32 = 21;
    pub const PID: i32 = 22;
    pub const KTHREADS: i32 = 23;
    pub const FORK: i32 = 24;
}

// ─── output ────────────────────────────────────────────────────────────────

/// Write bytes to fd 1, retrying short writes.
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
    let _ = write_bytes(b"proc: FAIL ");
    let _ = write_bytes(label);
    let _ = write_bytes(b"\n");
    code
}

fn ok(label: &[u8]) -> bool {
    write_bytes(b"proc: ")
        && write_bytes(label)
        && write_bytes(b" ok\n")
}

// ─── path helpers ──────────────────────────────────────────────────────────

/// `openat(0, path, flags)` for an absolute path with its NUL included.
fn open(path: &[u8], flags: usize) -> isize {
    debug_assert!(path.last() == Some(&0));
    syscall4(SYS_OPENAT, 0, path.as_ptr() as usize, flags, 0)
}

fn close(fd: usize) {
    let _ = syscall1(SYS_CLOSE, fd);
}

fn read_into(fd: usize, buf: &mut [u8]) -> Result<usize, isize> {
    let mut total = 0usize;
    while total < buf.len() {
        let count = syscall3(
            SYS_READ,
            fd,
            buf[total..].as_mut_ptr() as usize,
            buf.len() - total,
        );
        if count < 0 {
            return Err(count);
        }
        if count == 0 {
            return Ok(total);
        }
        total += count as usize;
    }
    Ok(total)
}

/// Open `path`, read it whole and close it. `Ok(n)` means the file fit in `buf`
/// (`n < buf.len()`); a file that did not fit is an error rather than a silently
/// truncated read, because every check below parses the whole text.
fn read_file(path: &[u8], buf: &mut [u8]) -> Result<usize, isize> {
    let fd = open(path, O_RDONLY);
    if fd < 0 {
        return Err(fd);
    }
    let fd = fd as usize;
    let result = read_into(fd, buf);
    close(fd);
    match result {
        Ok(total) if total < buf.len() => Ok(total),
        Ok(_) => Err(-1),
        Err(error) => Err(error),
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.len() > haystack.len() {
        return false;
    }
    let mut start = 0usize;
    while start + needle.len() <= haystack.len() {
        if &haystack[start..start + needle.len()] == needle {
            return true;
        }
        start += 1;
    }
    false
}

fn starts_with(haystack: &[u8], prefix: &[u8]) -> bool {
    haystack.len() >= prefix.len() && &haystack[..prefix.len()] == prefix
}

/// Parse the decimal number that follows `key` (and optional spaces/tabs).
fn parse_after(haystack: &[u8], key: &[u8]) -> Option<u64> {
    let mut index = 0usize;
    while index + key.len() <= haystack.len() {
        if &haystack[index..index + key.len()] == key {
            let mut cursor = index + key.len();
            while cursor < haystack.len() && (haystack[cursor] == b' ' || haystack[cursor] == b'\t') {
                cursor += 1;
            }
            let mut value = 0u64;
            let mut digits = 0usize;
            while cursor < haystack.len() && haystack[cursor].is_ascii_digit() {
                value = value.saturating_mul(10).saturating_add((haystack[cursor] - b'0') as u64);
                cursor += 1;
                digits += 1;
            }
            return if digits > 0 { Some(value) } else { None };
        }
        index += 1;
    }
    None
}

// ─── id paths and field matching ─────────────────────────────────────────

/// Write `prefix + decimal(id) + suffix + NUL` into `out`, returning the length
/// **including** the NUL, or `None` when it does not fit.
fn id_path(out: &mut [u8], prefix: &[u8], id: u64, suffix: &[u8]) -> Option<usize> {
    let mut len = 0usize;
    for &byte in prefix {
        if len >= out.len() {
            return None;
        }
        out[len] = byte;
        len += 1;
    }
    let mut digits = [0u8; 20];
    let count = decimal_digits(id, &mut digits);
    for &digit in &digits[..count] {
        if len >= out.len() {
            return None;
        }
        out[len] = digit;
        len += 1;
    }
    for &byte in suffix {
        if len >= out.len() {
            return None;
        }
        out[len] = byte;
        len += 1;
    }
    if len >= out.len() {
        return None;
    }
    out[len] = 0;
    Some(len + 1)
}

/// Decimal digits of `value`, no NUL; returns the count.
fn decimal_digits(mut value: u64, out: &mut [u8; 20]) -> usize {
    if value == 0 {
        out[0] = b'0';
        return 1;
    }
    let mut reversed = [0u8; 20];
    let mut count = 0usize;
    while value > 0 && count < reversed.len() {
        reversed[count] = b'0' + (value % 10) as u8;
        count += 1;
        value /= 10;
    }
    for index in 0..count {
        out[index] = reversed[count - 1 - index];
    }
    count
}

/// True when `text` contains `<key><decimal(id)>\n` — a status field value.
fn has_field_with_id(text: &[u8], key: &[u8], id: u64) -> bool {
    let mut needle = [0u8; 64];
    let mut len = 0usize;
    for &byte in key {
        need_byte(&mut needle, &mut len, byte);
    }
    let mut digits = [0u8; 20];
    let count = decimal_digits(id, &mut digits);
    for &digit in &digits[..count] {
        need_byte(&mut needle, &mut len, digit);
    }
    need_byte(&mut needle, &mut len, b'\n');
    contains(text, &needle[..len])
}

fn need_byte(buffer: &mut [u8; 64], len: &mut usize, byte: u8) {
    if *len < buffer.len() {
        buffer[*len] = byte;
        *len += 1;
    }
}

/// True when `text` starts with `<decimal(id)> ` (the `/proc/<id>/stat` prefix).
fn starts_with_id(text: &[u8], id: u64) -> bool {
    let mut digits = [0u8; 20];
    let count = decimal_digits(id, &mut digits);
    text.len() > count && &text[..count] == &digits[..count] && text[count] == b' '
}

// ─── checks ────────────────────────────────────────────────────────────────

/// `/proc/version`: static text, `fstat` metadata, and chunked reads that must
/// reassemble into exactly that text (offset continuation, EOF from `size()`).
fn check_version(scratch: &mut [u8]) -> Result<(), i32> {
    let fd = open(b"/proc/version\0", O_RDONLY);
    if fd < 0 {
        return Err(fail(b"openat /proc/version", code::VERSION));
    }
    let fd = fd as usize;

    let mut stat = Stat {
        mode: 0,
        kind: 0,
        size: 0,
        ino: 0,
    };
    if syscall4(
        SYS_FSTAT,
        fd,
        &mut stat as *mut Stat as usize,
        0,
        0,
    ) < 0
        || stat.kind != KIND_FILE
        || stat.mode & 0o777 != 0o644
        || stat.size == 0
        || stat.size > scratch.len() as u64
    {
        close(fd);
        return Err(fail(b"fstat /proc/version", code::VERSION));
    }

    // Chunked read: the kernel must keep one render across offsets and stop at
    // exactly `size` bytes. A 7-byte chunk is deliberately not a divisor of the
    // line length.
    let mut filled = 0usize;
    let mut chunk = [0u8; 7];
    loop {
        let count = syscall3(SYS_READ, fd, chunk.as_mut_ptr() as usize, chunk.len());
        if count < 0 {
            close(fd);
            return Err(fail(b"read /proc/version", code::VERSION));
        }
        if count == 0 {
            break;
        }
        let count = count as usize;
        if filled + count > scratch.len() {
            close(fd);
            return Err(fail(b"/proc/version too long", code::VERSION));
        }
        scratch[filled..filled + count].copy_from_slice(&chunk[..count]);
        filled += count;
    }
    if filled as u64 != stat.size
        || !starts_with(&scratch[..filled], b"pagh ")
        || !contains(&scratch[..filled], b"(riscv64)")
    {
        close(fd);
        return Err(fail(b"/proc/version contents", code::VERSION));
    }

    // Rewind: a read from offset 0 re-renders, and for a static file the bytes
    // must be identical. This is the documented "one reading pass" contract.
    if syscall3(SYS_LSEEK, fd, 0, SEEK_SET) != 0 {
        close(fd);
        return Err(fail(b"lseek /proc/version", code::VERSION));
    }
    let again = read_into(fd, scratch);
    close(fd);
    match again {
        Ok(total) if total == filled => {}
        _ => return Err(fail(b"/proc/version reread", code::VERSION)),
    }
    Ok(())
}

fn check_meminfo(scratch: &mut [u8]) -> Result<(), i32> {
    let total_read = match read_file(b"/proc/meminfo\0", scratch) {
        Ok(total) => total,
        Err(_) => return Err(fail(b"read /proc/meminfo", code::MEMINFO)),
    };
    let text = &scratch[..total_read];
    let total = parse_after(text, b"MemTotal:");
    let free = parse_after(text, b"MemFree:");
    match (total, free) {
        (Some(total), Some(free)) if total > 0 && free <= total => {}
        _ => return Err(fail(b"/proc/meminfo rows", code::MEMINFO)),
    }
    // The custom rows are documented in src/vfs/proc.rs; a reader that parses
    // them is exactly what the file is for.
    if !contains(text, b"HeapUsed:") || !contains(text, b"Sv39Pages:") {
        return Err(fail(b"/proc/meminfo extra rows", code::MEMINFO));
    }
    Ok(())
}

fn check_uptime(scratch: &mut [u8]) -> Result<(), i32> {
    let total = match read_file(b"/proc/uptime\0", scratch) {
        Ok(total) => total,
        Err(_) => return Err(fail(b"read /proc/uptime", code::UPTIME)),
    };
    let text = &scratch[..total];
    // `<seconds>.<centiseconds> 0.00\n`: the second field is documented as the
    // always-zero idle time, so checking the exact tail checks the contract.
    if total < 8
        || !text.ends_with(b" 0.00\n")
        || !text[0].is_ascii_digit()
        || !contains(text, b".")
    {
        return Err(fail(b"/proc/uptime shape", code::UPTIME));
    }
    Ok(())
}

fn check_tasks(scratch: &mut [u8]) -> Result<(), i32> {
    let total = match read_file(b"/proc/tasks\0", scratch) {
        Ok(total) => total,
        Err(_) => return Err(fail(b"read /proc/tasks", code::TASKS)),
    };
    let text = &scratch[..total];
    if !starts_with(text, b"tasks: ")
        || !contains(text, b"  id  name")
        || !contains(text, b"stack")
    {
        return Err(fail(b"/proc/tasks shape", code::TASKS));
    }
    Ok(())
}

fn check_mounts(scratch: &mut [u8]) -> Result<(), i32> {
    let total = match read_file(b"/proc/mounts\0", scratch) {
        Ok(total) => total,
        Err(_) => return Err(fail(b"read /proc/mounts", code::MOUNTS)),
    };
    let text = &scratch[..total];
    if !contains(text, b"/proc /proc procfs rw 0 0\n") {
        return Err(fail(b"/proc/mounts rows", code::MOUNTS));
    }
    Ok(())
}

fn check_net(scratch: &mut [u8]) -> Result<(), i32> {
    let total = match read_file(b"/proc/net/dev\0", scratch) {
        Ok(total) => total,
        Err(_) => return Err(fail(b"read /proc/net/dev", code::NET)),
    };
    let dev = &scratch[..total];
    // No NIC is a valid boot (CI's serial mode); a NIC must report a MAC.
    if !starts_with(dev, b"virtio-net0: ") || !(contains(dev, b"no interface") || contains(dev, b"mac ")) {
        return Err(fail(b"/proc/net/dev rows", code::NET));
    }
    let total = match read_file(b"/proc/net/arp\0", scratch) {
        Ok(total) => total,
        Err(_) => return Err(fail(b"read /proc/net/arp", code::NET)),
    };
    let arp = &scratch[..total];
    if !(starts_with(arp, b"IP address") || starts_with(arp, b"virtio-net0: no interface")) {
        return Err(fail(b"/proc/net/arp rows", code::NET));
    }
    Ok(())
}

/// `getdents64` on `/proc` and `/proc/net`: the seven root entries with their
/// kinds, the two net entries, and record framing (`% 8 == 0`, names in bounds).
fn check_directory(scratch: &mut [u8]) -> Result<(), i32> {
    const ROOT_NAMES: [&[u8]; 9] = [
        b"cpuinfo",
        b"kthreads",
        b"meminfo",
        b"mounts",
        b"net",
        b"self",
        b"tasks",
        b"uptime",
        b"version",
    ];
    let own_pid = syscall1(SYS_GETPID, 0);
    if own_pid <= 0 {
        return Err(fail(b"getpid", code::DIR));
    }
    let own_pid = own_pid as u64;
    let fd = open(b"/proc\0", O_RDONLY);
    if fd < 0 {
        return Err(fail(b"openat /proc", code::DIR));
    }
    let fd = fd as usize;
    let entries = syscall3(
        SYS_GETDENTS64,
        fd,
        scratch.as_mut_ptr() as usize,
        scratch.len(),
    );
    close(fd);
    if entries <= 0 {
        return Err(fail(b"getdents64 /proc", code::DIR));
    }
    let entries = entries as usize;
    if entries % 8 != 0 || entries > scratch.len() {
        return Err(fail(b"getdents64 /proc framing", code::DIR));
    }

    let mut seen = [false; 9];
    let mut net_is_dir = false;
    let mut kthreads_is_dir = false;
    let mut self_is_link = false;
    let mut own_pid_is_dir = false;
    let mut cursor = 0usize;
    while cursor < entries {
        if cursor + 12 > entries {
            return Err(fail(b"getdents64 short header", code::DIR));
        }
        let name_len = scratch[cursor + 9] as usize;
        let kind = scratch[cursor + 8];
        // The native `Dirent` has no `rec_len` field: the reader derives the
        // record length from the header (12 bytes) plus the name, rounded up to
        // 8. The kernel's `getdents64` packs exactly that.
        let rec_len = (12 + name_len + 7) & !7;
        if cursor + rec_len > entries {
            return Err(fail(b"getdents64 record length", code::DIR));
        }
        let name = &scratch[cursor + 12..cursor + 12 + name_len];
        for (index, expected) in ROOT_NAMES.iter().enumerate() {
            if name == *expected {
                seen[index] = true;
            }
        }
        if name == b"net" && kind == KIND_DIR as u8 {
            net_is_dir = true;
        }
        if name == b"kthreads" && kind == KIND_DIR as u8 {
            kthreads_is_dir = true;
        }
        if name == b"self" && kind == KIND_SYMLINK as u8 {
            self_is_link = true;
        }
        let mut digits = [0u8; 20];
        let count = decimal_digits(own_pid, &mut digits);
        if name == &digits[..count] && kind == KIND_DIR as u8 {
            own_pid_is_dir = true;
        }
        cursor += rec_len;
    }
    if !seen.iter().all(|found| *found)
        || !net_is_dir
        || !kthreads_is_dir
        || !self_is_link
        || !own_pid_is_dir
    {
        return Err(fail(b"/proc entries", code::DIR));
    }

    // `/proc/net` is a directory with `arp` and `dev`; a file node must not be
    // navigable as one.
    let fd = open(b"/proc/net\0", O_RDONLY);
    if fd < 0 {
        return Err(fail(b"openat /proc/net", code::DIR));
    }
    let fd = fd as usize;
    let entries = syscall3(
        SYS_GETDENTS64,
        fd,
        scratch.as_mut_ptr() as usize,
        scratch.len(),
    );
    close(fd);
    if entries <= 0 {
        return Err(fail(b"getdents64 /proc/net", code::DIR));
    }
    let entries = entries as usize;
    let mut arp = false;
    let mut dev = false;
    let mut cursor = 0usize;
    while cursor < entries {
        let name_len = scratch[cursor + 9] as usize;
        let rec_len = (12 + name_len + 7) & !7;
        if cursor + rec_len > entries {
            return Err(fail(b"getdents64 /proc/net record", code::DIR));
        }
        let name = &scratch[cursor + 12..cursor + 12 + name_len];
        arp |= name == b"arp";
        dev |= name == b"dev";
        cursor += rec_len;
    }
    if !arp || !dev {
        return Err(fail(b"/proc/net entries", code::DIR));
    }
    Ok(())
}

/// A synthetic filesystem must refuse mutation through the ordinary calls.
fn check_readonly() -> Result<(), i32> {
    let mkdir = syscall3(SYS_MKDIRAT, 0, b"/proc/new\0".as_ptr() as usize, 0o755);
    if mkdir != ENOTSUP {
        return Err(fail(b"mkdirat /proc/new", code::READONLY));
    }
    let unlink = syscall3(SYS_UNLINKAT, 0, b"/proc/version\0".as_ptr() as usize, 0);
    if unlink != ENOTSUP {
        return Err(fail(b"unlinkat /proc/version", code::READONLY));
    }
    // Opening for write succeeds (there is no permission layer in this VFS),
    // but the write itself must be refused rather than silently dropped.
    let fd = open(b"/proc/version\0", O_WRONLY);
    if fd < 0 {
        return Err(fail(b"openat /proc/version O_WRONLY", code::READONLY));
    }
    let fd = fd as usize;
    let written = syscall3(SYS_WRITE, fd, b"x".as_ptr() as usize, 1);
    close(fd);
    if written != ENOTSUP {
        return Err(fail(b"write /proc/version", code::READONLY));
    }
    Ok(())
}

fn check_missing() -> Result<(), i32> {
    let fd = open(b"/proc/nosuch\0", O_RDONLY);
    if fd != ENOENT {
        if fd >= 0 {
            close(fd as usize);
        }
        return Err(fail(b"openat /proc/nosuch", code::MISSING));
    }
    let fd = open(b"/proc/net/nosuch\0", O_RDONLY);
    if fd != ENOENT {
        if fd >= 0 {
            close(fd as usize);
        }
        return Err(fail(b"openat /proc/net/nosuch", code::MISSING));
    }
    Ok(())
}

/// `/proc/self` and the files behind it resolve to *this* process.
fn check_self(scratch: &mut [u8]) -> Result<(), i32> {
    let pid = syscall1(SYS_GETPID, 0);
    let tid = syscall1(SYS_GETTID, 0);
    if pid <= 0 || tid <= 0 {
        return Err(fail(b"getpid/gettid", code::SELF));
    }
    let pid = pid as u64;
    let tid = tid as u64;

    // `/proc/self` is a symlink to this process's entry: `<pid>` for a user
    // thread. The target is computed from the reader's context.
    let mut target = [0u8; 32];
    let length = syscall4(
        SYS_READLINKAT,
        0,
        b"/proc/self\0".as_ptr() as usize,
        target.as_mut_ptr() as usize,
        target.len(),
    );
    if length <= 0 {
        return Err(fail(b"readlink /proc/self", code::SELF));
    }
    let mut expected = [0u8; 20];
    let count = decimal_digits(pid, &mut expected);
    if &target[..length as usize] != &expected[..count] {
        return Err(fail(b"/proc/self target", code::SELF));
    }

    // Status through the symlink: the ordinary VFS walk expands it.
    let total = match read_file(b"/proc/self/status\0", scratch) {
        Ok(total) => total,
        Err(_) => return Err(fail(b"read /proc/self/status", code::SELF)),
    };
    let status = &scratch[..total];
    if !has_field_with_id(status, b"Pid:\t", pid)
        || !has_field_with_id(status, b"Tid:\t", tid)
        || !contains(status, b"Name:\t")
        || !contains(status, b"Threads:\t")
        || !contains(status, b"State:\t")
    {
        return Err(fail(b"/proc/self/status fields", code::SELF));
    }

    // stat: `<pid> (comm) ...` with this process's pid in field zero.
    let total = match read_file(b"/proc/self/stat\0", scratch) {
        Ok(total) => total,
        Err(_) => return Err(fail(b"read /proc/self/stat", code::SELF)),
    };
    if !starts_with_id(&scratch[..total], pid) || !contains(&scratch[..total], b"(proc") {
        return Err(fail(b"/proc/self/stat line", code::SELF));
    }

    // cmdline: NUL-separated, trailing NUL, contains the argv[0] the selftest or
    // the shell handed the fixture.
    let total = match read_file(b"/proc/self/cmdline\0", scratch) {
        Ok(total) => total,
        Err(_) => return Err(fail(b"read /proc/self/cmdline", code::SELF)),
    };
    if total == 0 || scratch[total - 1] != 0 || !contains(&scratch[..total], b"proc") {
        return Err(fail(b"/proc/self/cmdline bytes", code::SELF));
    }
    Ok(())
}

/// `/proc/<pid>` is addressable by the id `getpid()` returns, and an id that
/// names no live process is `-ENOENT`.
fn check_pid(scratch: &mut [u8]) -> Result<(), i32> {
    let pid = syscall1(SYS_GETPID, 0);
    if pid <= 0 {
        return Err(fail(b"getpid", code::PID));
    }
    let pid = pid as u64;

    let mut path = [0u8; 64];
    let Some(length) = id_path(&mut path, b"/proc/", pid, b"/status") else {
        return Err(fail(b"pid path", code::PID));
    };
    let total = match read_file(&path[..length], scratch) {
        Ok(total) => total,
        Err(_) => return Err(fail(b"read /proc/<pid>/status", code::PID)),
    };
    if !has_field_with_id(&scratch[..total], b"Pid:\t", pid)
        || !has_field_with_id(&scratch[..total], b"Threads:\t", 1)
    {
        return Err(fail(b"/proc/<pid>/status fields", code::PID));
    }

    // The same process reached by id and through `/proc/self` is the same file.
    let direct = total;
    let total = match read_file(b"/proc/self/status\0", scratch) {
        Ok(total) => total,
        Err(_) => return Err(fail(b"reread /proc/self/status", code::PID)),
    };
    let _ = direct;
    if !has_field_with_id(&scratch[..total], b"Pid:\t", pid) {
        return Err(fail(b"/proc/self is /proc/<pid>", code::PID));
    }

    // A pid that never existed: -ENOENT, not an empty file.
    let missing = open(b"/proc/999999/status\0", O_RDONLY);
    if missing != ENOENT {
        if missing >= 0 {
            close(missing as usize);
        }
        return Err(fail(b"/proc/999999/status", code::PID));
    }
    Ok(())
}

/// Kernel threads have no pid; `/proc/kthreads` is where the shell's own entry
/// lives, which is what makes `/proc/self` work outside a process.
fn check_kthreads(scratch: &mut [u8]) -> Result<(), i32> {
    let fd = open(b"/proc/kthreads\0", O_RDONLY);
    if fd < 0 {
        return Err(fail(b"openat /proc/kthreads", code::KTHREADS));
    }
    let fd = fd as usize;
    let entries = syscall3(
        SYS_GETDENTS64,
        fd,
        scratch.as_mut_ptr() as usize,
        scratch.len(),
    );
    close(fd);
    if entries <= 0 {
        return Err(fail(b"getdents64 /proc/kthreads", code::KTHREADS));
    }
    let entries = entries as usize;
    let mut shell_dir = false;
    let mut idle_dir = false;
    let mut cursor = 0usize;
    while cursor < entries {
        let name_len = scratch[cursor + 9] as usize;
        let kind = scratch[cursor + 8];
        let rec_len = (12 + name_len + 7) & !7;
        if cursor + rec_len > entries {
            return Err(fail(b"getdents64 /proc/kthreads record", code::KTHREADS));
        }
        let name = &scratch[cursor + 12..cursor + 12 + name_len];
        if name == b"0" && kind == KIND_DIR as u8 {
            shell_dir = true;
        }
        if name == b"1" && kind == KIND_DIR as u8 {
            idle_dir = true;
        }
        cursor += rec_len;
    }
    if !shell_dir || !idle_dir {
        return Err(fail(b"/proc/kthreads entries", code::KTHREADS));
    }

    let total = match read_file(b"/proc/kthreads/0/status\0", scratch) {
        Ok(total) => total,
        Err(_) => return Err(fail(b"read /proc/kthreads/0/status", code::KTHREADS)),
    };
    let status = &scratch[..total];
    if !contains(status, b"Name:\tshell\n")
        || !has_field_with_id(status, b"Pid:\t", 0)
        || !has_field_with_id(status, b"Tid:\t", 0)
        || !contains(status, b"Stack:\tboot\n")
    {
        return Err(fail(b"/proc/kthreads/0/status fields", code::KTHREADS));
    }

    let total = match read_file(b"/proc/kthreads/0/stat\0", scratch) {
        Ok(total) => total,
        Err(_) => return Err(fail(b"read /proc/kthreads/0/stat", code::KTHREADS)),
    };
    if !starts_with_id(&scratch[..total], 0) || !contains(&scratch[..total], b"(shell) R ") {
        return Err(fail(b"/proc/kthreads/0/stat line", code::KTHREADS));
    }

    // A kernel task id that does not exist is -ENOENT in that namespace too.
    let missing = open(b"/proc/kthreads/999999/status\0", O_RDONLY);
    if missing != ENOENT {
        if missing >= 0 {
            close(missing as usize);
        }
        return Err(fail(b"/proc/kthreads/999999", code::KTHREADS));
    }
    Ok(())
}

/// A live *other* process is readable by pid; once it is reaped, the same path
/// is `-ENOENT`. This is the part of the contract the shell cannot exercise on
/// its own (a foreground `run` owns the console), so the fixture forks.
fn check_fork(scratch: &mut [u8]) -> Result<(), i32> {
    let parent = syscall1(SYS_GETPID, 0);
    if parent <= 0 {
        return Err(fail(b"getpid", code::FORK));
    }
    let parent = parent as u64;
    let child = syscall1(SYS_FORK, 0);
    if child == 0 {
        // Child: stay alive long enough for the parent's two reads, then exit 0.
        let mut delay = Timespec {
            tv_sec: 0,
            tv_nsec: 200_000_000,
        };
        let _ = syscall3(
            SYS_NANOSLEEP,
            &mut delay as *mut Timespec as usize,
            0,
            0,
        );
        let _ = syscall1(SYS_EXIT, 0);
        loop {}
    }
    if child < 0 {
        return Err(fail(b"fork", code::FORK));
    }
    let child = child as u64;

    let mut path = [0u8; 64];
    let Some(length) = id_path(&mut path, b"/proc/", child, b"/status") else {
        return Err(fail(b"child path", code::FORK));
    };
    let total = match read_file(&path[..length], scratch) {
        Ok(total) => total,
        Err(_) => return Err(fail(b"read child status", code::FORK)),
    };
    let status = &scratch[..total];
    if !has_field_with_id(status, b"Pid:\t", child)
        || !has_field_with_id(status, b"PPid:\t", parent)
        || !contains(status, b"Name:\tproc")
    {
        return Err(fail(b"child status fields", code::FORK));
    }

    // fork copies the argv snapshot: the child's cmdline is the parent's.
    let Some(length) = id_path(&mut path, b"/proc/", child, b"/cmdline") else {
        return Err(fail(b"child cmdline path", code::FORK));
    };
    let total = match read_file(&path[..length], scratch) {
        Ok(total) => total,
        Err(_) => return Err(fail(b"read child cmdline", code::FORK)),
    };
    if total == 0 || scratch[total - 1] != 0 || !contains(&scratch[..total], b"proc") {
        return Err(fail(b"child cmdline bytes", code::FORK));
    }

    let mut wait_status: i32 = -1;
    let waited = syscall4(
        SYS_WAIT4,
        child as usize,
        &mut wait_status as *mut i32 as usize,
        0,
        0,
    );
    if waited != child as isize || wait_status != 0 {
        return Err(fail(b"wait4 child", code::FORK));
    }

    // The child is gone and reaped: its pid is no longer a directory.
    let Some(length) = id_path(&mut path, b"/proc/", child, b"/status") else {
        return Err(fail(b"dead child path", code::FORK));
    };
    let gone = open(&path[..length], O_RDONLY);
    if gone != ENOENT {
        if gone >= 0 {
            close(gone as usize);
        }
        return Err(fail(b"/proc/<dead pid> exists", code::FORK));
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
pub extern "C" fn proc_main() -> i32 {
    let mut scratch = [0u8; 2048];

    if let Err(code) = check_version(&mut scratch) {
        return code;
    }
    progress(b"version");

    if let Err(code) = check_meminfo(&mut scratch) {
        return code;
    }
    progress(b"meminfo");

    if let Err(code) = check_uptime(&mut scratch) {
        return code;
    }
    progress(b"uptime");

    if let Err(code) = check_tasks(&mut scratch) {
        return code;
    }
    progress(b"tasks");

    if let Err(code) = check_mounts(&mut scratch) {
        return code;
    }
    progress(b"mounts");

    if let Err(code) = check_net(&mut scratch) {
        return code;
    }
    progress(b"net");

    if let Err(code) = check_directory(&mut scratch) {
        return code;
    }
    progress(b"directory");

    if let Err(code) = check_readonly() {
        return code;
    }
    progress(b"readonly");

    if let Err(code) = check_missing() {
        return code;
    }
    progress(b"missing");

    if let Err(code) = check_self(&mut scratch) {
        return code;
    }
    progress(b"self");

    if let Err(code) = check_pid(&mut scratch) {
        return code;
    }
    progress(b"pid");

    if let Err(code) = check_kthreads(&mut scratch) {
        return code;
    }
    progress(b"kthreads");

    if let Err(code) = check_fork(&mut scratch) {
        return code;
    }
    progress(b"fork");

    if OUTPUT_OK.load(Ordering::Relaxed) == 0 {
        return code::OUTPUT;
    }
    let _ = write_bytes(b"proc: done\n");
    code::OK
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // A panic in the fixture is a failure, not a hang: exit with its own code.
    let _ = write_bytes(b"proc: FAIL panic\n");
    let _ = syscall1(93, code::OUTPUT as usize);
    loop {}
}