//! The native-ABI acceptance fixture: one static RV64 program that exercises
//! every syscall in `pagh-api/docs/abi.md` without libpagh.
//!
//! The selftest creates `/mnt/abi-syscalls.txt` (`pagh-abi\n`), an empty
//! `/mnt/abi-created.txt` and the relative symlink `/mnt/abi-link` before it
//! runs this program; a boot without a filesystem gets `-ENOENT` on the first
//! `openat` and exits 77 after every filesystem-independent check has passed, so
//! the serial-only suite still covers `write`/`brk`/`clock_gettime`/`uname`/
//! `getdents64("/")`. Any real failure returns a distinct non-zero code (and
//! prints a diagnostic), which the selftest maps to FAIL.
//!
//! No allocator, no `core::fmt`, no libc: syscall wrappers over `ecall` and
//! fixed stack buffers. Build with `userland/build.sh`.

#![no_std]
#![no_main]

core::arch::global_asm!(
    r#"
    .section .text.entry, "ax", @progbits
    .globl _start
_start:
    call    abi_main
    li      a7, 93
    ecall
1:  j       1b
"#
);

// ─── syscall wrapper ───────────────────────────────────────────────────────

fn syscall4(number: usize, a0: usize, a1: usize, a2: usize, a3: usize) -> isize {
    let result: isize;
    // SAFETY: `ecall` enters the kernel with `a7` as the number and `a0..a3` as
    // arguments; the kernel restores the same `sp` and callee-saved registers.
    unsafe {
        core::arch::asm!(
            "ecall",
            in("a7") number,
            inlateout("a0") a0 => result,
            in("a1") a1,
            in("a2") a2,
            in("a3") a3,
            options(nostack)
        )
    };
    result
}

fn syscall3(number: usize, a0: usize, a1: usize, a2: usize) -> isize {
    syscall4(number, a0, a1, a2, 0)
}

fn syscall2(number: usize, a0: usize, a1: usize) -> isize {
    syscall4(number, a0, a1, 0, 0)
}

fn syscall1(number: usize, a0: usize) -> isize {
    syscall4(number, a0, 0, 0, 0)
}

// ─── numbers and structs (the ABI document, not Linux) ────────────────────

const SYS_READ: usize = 63;
const SYS_WRITE: usize = 64;
const SYS_OPENAT: usize = 56;
const SYS_CLOSE: usize = 57;
const SYS_GETDENTS64: usize = 61;
const SYS_LSEEK: usize = 62;
const SYS_FSTAT: usize = 80;
const SYS_NANOSLEEP: usize = 101;
const SYS_CLOCK_GETTIME: usize = 113;
const SYS_READLINKAT: usize = 78;
const SYS_UNAME: usize = 160;
const SYS_BRK: usize = 214;

const O_RDONLY: usize = 0;
const O_WRONLY: usize = 1;
const O_CREAT: usize = 0x40;
const O_TRUNC: usize = 0x80;
const O_APPEND: usize = 0x100;

const ENOENT: isize = -2;
const EBADF: isize = -9;
const EFAULT: isize = -14;
const ENOTDIR: isize = -20;
const EISDIR: isize = -21;
const EINVAL: isize = -22;
const ENOMEM: isize = -12;
const ENOSYS: isize = -38;

#[repr(C)]
struct Stat {
    mode: u32,
    kind: u32,
    size: u64,
    ino: u64,
}

#[repr(C)]
struct Timespec {
    seconds: i64,
    nanos: i64,
}

#[repr(C)]
struct Uname {
    sysname: [u8; 16],
    nodename: [u8; 16],
    release: [u8; 16],
    version: [u8; 16],
    machine: [u8; 16],
}

const KIND_FILE: u32 = 0;
const KIND_DIR: u32 = 1;

// ─── output ────────────────────────────────────────────────────────────────

fn write_all(bytes: &[u8]) -> bool {
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

fn print(text: &str) {
    let _ = write_all(text.as_bytes());
}

fn print_usize(mut value: usize) {
    let mut digits = [0u8; 20];
    let mut index = digits.len();
    loop {
        index -= 1;
        digits[index] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    let _ = write_all(&digits[index..]);
}

/// Print a failure and leave with `code`.
fn fail(code: i32, what: &str) -> ! {
    print("abi: FAIL ");
    print(what);
    print(" (code ");
    if code < 0 {
        print("-");
        print_usize((-code) as usize);
    } else {
        print_usize(code as usize);
    }
    print(")\n");
    let _ = syscall1(93, code as usize);
    loop {}
}

fn check(condition: bool, code: i32, what: &str) {
    if !condition {
        fail(code, what);
    }
}

fn cstr_eq(pointer: *const u8, expected: &[u8]) -> bool {
    for (index, byte) in expected.iter().enumerate() {
        // SAFETY: the caller only passes NUL-terminated ABI strings from the
        // kernel or from this program's own literals; the read is bounded by
        // `expected`.
        if unsafe { *pointer.add(index) } != *byte {
            return false;
        }
    }
    // SAFETY: one byte past the literal is the NUL of a matching string.
    unsafe { *pointer.add(expected.len()) == 0 }
}

fn buffer_equals(buffer: &[u8], expected: &[u8]) -> bool {
    if buffer.len() != expected.len() {
        return false;
    }
    let mut index = 0;
    while index < buffer.len() {
        if buffer[index] != expected[index] {
            return false;
        }
        index += 1;
    }
    true
}

/// First directory record with `expected` as its name? Also validates that each
/// record is 8-byte aligned and stays inside `length`.
fn directory_has(buffer: &[u8], length: usize, expected: &[u8]) -> bool {
    let mut offset = 0usize;
    while offset + 8 <= length {
        let name_length = buffer[offset + 9] as usize;
        // 12-byte header (ino, kind, name_len, reserved) + name, padded to 8.
        let record = (12 + name_length + 7) & !7;
        if record < 8 || offset + record > length || offset + 12 + name_length > length {
            return false;
        }
        let name = &buffer[offset + 12..offset + 12 + name_length];
        if buffer_equals(name, expected) {
            return true;
        }
        offset += record;
    }
    offset == length
}

// ─── the fixture ───────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn abi_main() -> i32 {
    print("abi: start\n");

    // `getpid`: the shell task id, positive.
    let pid = syscall1(172, 0);
    check(pid > 0, 1, "getpid");

    // ── uname ──
    let mut uname = Uname {
        sysname: [0; 16],
        nodename: [0; 16],
        release: [0; 16],
        version: [0; 16],
        machine: [0; 16],
    };
    check(
        syscall2(SYS_UNAME, &mut uname as *mut Uname as usize, 0) == 0,
        2,
        "uname result",
    );
    check(cstr_eq(uname.sysname.as_ptr(), b"pagh"), 3, "uname.sysname");
    check(
        cstr_eq(uname.nodename.as_ptr(), b"pagh-riscv"),
        4,
        "uname.nodename",
    );
    check(
        cstr_eq(uname.machine.as_ptr(), b"riscv64"),
        5,
        "uname.machine",
    );
    check(
        cstr_eq(uname.release.as_ptr(), b"0.10.0-dev"),
        6,
        "uname.release",
    );

    // ── clock_gettime ──
    let mut before = Timespec {
        seconds: 0,
        nanos: 0,
    };
    check(
        syscall2(
            SYS_CLOCK_GETTIME,
            0,
            &mut before as *mut Timespec as usize,
        ) == 0,
        7,
        "clock_gettime(0)",
    );
    check(
        before.seconds >= 0 && before.nanos >= 0 && before.nanos < 1_000_000_000,
        8,
        "clock_gettime values",
    );
    let mut monotonic = Timespec {
        seconds: 0,
        nanos: 0,
    };
    check(
        syscall2(
            SYS_CLOCK_GETTIME,
            1,
            &mut monotonic as *mut Timespec as usize,
        ) == 0,
        9,
        "clock_gettime(1)",
    );
    check(
        syscall2(SYS_CLOCK_GETTIME, 2, &mut monotonic as *mut Timespec as usize) == EINVAL,
        10,
        "clock id",
    );

    // ── nanosleep: 1 ms rounds up to one 10 ms tick; the clock must not go
    // backwards and must have moved by at least 10 ms. ──
    let short = Timespec {
        seconds: 0,
        nanos: 1_000_000,
    };
    check(
        syscall2(SYS_NANOSLEEP, &short as *const Timespec as usize, 0) == 0,
        11,
        "nanosleep",
    );
    let mut after = Timespec {
        seconds: 0,
        nanos: 0,
    };
    check(
        syscall2(
            SYS_CLOCK_GETTIME,
            0,
            &mut after as *mut Timespec as usize,
        ) == 0,
        12,
        "clock after sleep",
    );
    let before_ms = before.seconds * 1000 + before.nanos / 1_000_000;
    let after_ms = after.seconds * 1000 + after.nanos / 1_000_000;
    check(after_ms >= before_ms, 13, "clock went backwards");
    let bad_sleep = Timespec {
        seconds: 0,
        nanos: 1_000_000_000,
    };
    check(
        syscall2(SYS_NANOSLEEP, &bad_sleep as *const Timespec as usize, 0) == EINVAL,
        14,
        "nanosleep range",
    );

    // ── brk: query, grow, write a pattern, verify, shrink ──
    let base = syscall1(SYS_BRK, 0);
    check(base > 0, 15, "brk(0)");
    check(base as usize % 4096 == 0, 16, "brk alignment");
    let grown = syscall1(SYS_BRK, base as usize + 8192);
    check(grown >= base + 8192, 17, "brk grow");
    // SAFETY: `base` is the break the kernel just returned and the growth to
    // `base + 8192` mapped two fresh pages; these stores are inside them.
    unsafe {
        let first = base as *mut u64;
        *first = 0x1122_3344_5566_7788;
        let second = (base as usize + 4096) as *mut u64;
        *second = 0x99aa_bbcc_ddee_ff00;
        check(*first == 0x1122_3344_5566_7788, 18, "brk pattern page 0");
        check(*second == 0x99aa_bbcc_ddee_ff00, 19, "brk pattern page 1");
    }
    check(
        syscall1(SYS_BRK, base as usize) == base,
        20,
        "brk shrink",
    );
    // Two MiB is past the ABI's 1 MiB heap window.
    check(
        syscall1(SYS_BRK, base as usize + (2 << 20)) == ENOMEM,
        21,
        "brk past window",
    );

    // ── error paths that must not fault the machine ──
    check(
        syscall3(SYS_WRITE, 1, 0x10, 4) == EFAULT,
        22,
        "write bad pointer",
    );
    check(
        syscall4(SYS_OPENAT, 0, 0x10, 0, 0) == EFAULT,
        23,
        "openat bad pointer",
    );
    check(
        syscall1(4096, 0) == ENOSYS,
        24,
        "unknown syscall",
    );
    check(
        syscall3(SYS_WRITE, 99, b"x".as_ptr() as usize, 1) == EBADF,
        25,
        "write bad fd",
    );

    // ── getdents64 on the synthetic root: at least one 8-byte-aligned record,
    // and `/mnt` when a filesystem is mounted. ──
    let root = syscall4(SYS_OPENAT, 0, b"/\0".as_ptr() as usize, O_RDONLY, 0);
    check(root >= 3, 26, "openat /");
    let mut entries = [0u8; 512];
    let encoded = syscall3(
        SYS_GETDENTS64,
        root as usize,
        entries.as_mut_ptr() as usize,
        entries.len(),
    );
    // A serial-only boot has no `/mnt`, so the synthetic root may be empty;
    // when it has entries, they must be well-formed records and must include
    // the mount.
    check(encoded >= 0, 27, "getdents64 /");
    let encoded = encoded as usize;
    if encoded > 0 {
        check(
            encoded % 8 == 0 && directory_has(&entries, encoded, b"mnt"),
            28,
            "getdents64 record /mnt",
        );
    }
    let mut stat = Stat {
        mode: 0,
        kind: 0,
        size: 0,
        ino: 0,
    };
    check(
        syscall2(SYS_FSTAT, root as usize, &mut stat as *mut Stat as usize) == 0,
        29,
        "fstat /",
    );
    check(
        stat.kind == KIND_DIR && stat.mode & 0o777 == 0o755,
        30,
        "fstat / fields",
    );
    check(
        syscall3(
            SYS_READ,
            root as usize,
            entries.as_mut_ptr() as usize,
            4,
        ) == EISDIR,
        73,
        "read on a directory",
    );
    check(syscall1(SYS_CLOSE, root as usize) == 0, 31, "close /");

    // ── the filesystem half: skip when the selftest could not provide /mnt ──
    let path = b"/mnt/abi-syscalls.txt\0";
    let fd = syscall4(SYS_OPENAT, 0, path.as_ptr() as usize, O_RDONLY, 0);
    if fd == ENOENT {
        print("abi: no /mnt, no filesystem half\n");
        let _ = syscall1(93, 77);
        loop {}
    }
    check(fd >= 3, 32, "openat fixture");

    // fstat: a regular file, 0o644, nine bytes, a real inode.
    check(
        syscall2(SYS_FSTAT, fd as usize, &mut stat as *mut Stat as usize) == 0,
        33,
        "fstat fixture",
    );
    check(
        stat.kind == KIND_FILE && stat.mode & 0o777 == 0o644,
        34,
        "fstat fixture mode/kind",
    );
    check(stat.size == 9 && stat.ino != 0, 35, "fstat fixture size/ino");

    // read the whole file, then seek/read the prefix, then END.
    let mut file_buffer = [0u8; 16];
    let read = syscall3(
        SYS_READ,
        fd as usize,
        file_buffer.as_mut_ptr() as usize,
        file_buffer.len(),
    );
    check(read == 9, 36, "read fixture length");
    check(
        buffer_equals(&file_buffer[..9], b"pagh-abi\n"),
        37,
        "read fixture content",
    );
    check(syscall3(SYS_LSEEK, fd as usize, 0, 0) == 0, 38, "lseek SET");
    let read = syscall3(
        SYS_READ,
        fd as usize,
        file_buffer.as_mut_ptr() as usize,
        4,
    );
    check(read == 4, 39, "read after seek");
    check(
        buffer_equals(&file_buffer[..4], b"pagh"),
        40,
        "read after seek content",
    );
    check(
        syscall3(SYS_LSEEK, fd as usize, 0, 2) == 9,
        41,
        "lseek END",
    );
    check(
        syscall3(
            SYS_READ,
            fd as usize,
            file_buffer.as_mut_ptr() as usize,
            4,
        ) == 0,
        42,
        "read at EOF",
    );
    check(
        syscall3(SYS_LSEEK, fd as usize, usize::MAX, 0) == EINVAL,
        43,
        "lseek negative",
    );
    check(
        syscall3(
            SYS_GETDENTS64,
            fd as usize,
            entries.as_mut_ptr() as usize,
            entries.len(),
        ) == ENOTDIR,
        44,
        "getdents64 on a file",
    );
    check(syscall1(SYS_CLOSE, fd as usize) == 0, 45, "close fixture");
    check(syscall1(SYS_CLOSE, fd as usize) == EBADF, 46, "double close");
    check(
        syscall2(SYS_FSTAT, fd as usize, &mut stat as *mut Stat as usize) == EBADF,
        47,
        "fstat closed",
    );

    // ── create, write, append, read back ──
    let created = b"/mnt/abi-created.txt\0";
    let fd = syscall4(
        SYS_OPENAT,
        0,
        created.as_ptr() as usize,
        O_WRONLY | O_CREAT | O_TRUNC,
        0o644,
    );
    check(fd >= 3, 48, "create");
    check(
        syscall3(SYS_WRITE, fd as usize, b"abc".as_ptr() as usize, 3) == 3,
        49,
        "create write",
    );
    check(syscall1(SYS_CLOSE, fd as usize) == 0, 50, "create close");

    let fd = syscall4(
        SYS_OPENAT,
        0,
        created.as_ptr() as usize,
        O_WRONLY | O_APPEND,
        0,
    );
    check(fd >= 3, 51, "reopen append");
    check(
        syscall3(SYS_WRITE, fd as usize, b"de".as_ptr() as usize, 2) == 2,
        52,
        "append write",
    );
    check(syscall1(SYS_CLOSE, fd as usize) == 0, 53, "append close");

    let fd = syscall4(SYS_OPENAT, 0, created.as_ptr() as usize, O_RDONLY, 0);
    check(fd >= 3, 54, "reopen read");
    check(
        syscall2(SYS_FSTAT, fd as usize, &mut stat as *mut Stat as usize) == 0,
        55,
        "fstat created",
    );
    check(stat.size == 5, 56, "created size");
    let read = syscall3(
        SYS_READ,
        fd as usize,
        file_buffer.as_mut_ptr() as usize,
        file_buffer.len(),
    );
    check(read == 5, 57, "created read length");
    check(
        buffer_equals(&file_buffer[..5], b"abcde"),
        58,
        "created content",
    );
    check(syscall1(SYS_CLOSE, fd as usize) == 0, 59, "created close");

    // ── symlink: openat follows it, readlinkat returns the target ──
    let link = b"/mnt/abi-link\0";
    let fd = syscall4(SYS_OPENAT, 0, link.as_ptr() as usize, O_RDONLY, 0);
    check(fd >= 3, 60, "open symlink");
    check(
        syscall2(SYS_FSTAT, fd as usize, &mut stat as *mut Stat as usize) == 0,
        61,
        "fstat symlink target",
    );
    check(stat.size == 9 && stat.kind == KIND_FILE, 62, "symlink target");
    check(syscall1(SYS_CLOSE, fd as usize) == 0, 63, "close symlink");

    let mut target = [0u8; 32];
    let length = syscall4(
        SYS_READLINKAT,
        0,
        link.as_ptr() as usize,
        target.as_mut_ptr() as usize,
        target.len(),
    );
    check(length == 16, 64, "readlinkat length");
    check(
        buffer_equals(&target[..16], b"abi-syscalls.txt"),
        65,
        "readlinkat target",
    );
    check(
        syscall4(
            SYS_READLINKAT,
            0,
            path.as_ptr() as usize,
            target.as_mut_ptr() as usize,
            target.len(),
        ) == EINVAL,
        66,
        "readlinkat on a file",
    );
    check(
        syscall4(SYS_READLINKAT, 0, link.as_ptr() as usize, 0x10, 8) == EFAULT,
        67,
        "readlinkat bad pointer",
    );

    // ── directory iteration over /mnt: the two files and the link ──
    let mnt = b"/mnt\0";
    let fd = syscall4(SYS_OPENAT, 0, mnt.as_ptr() as usize, O_RDONLY, 0);
    check(fd >= 3, 68, "openat /mnt");
    let encoded = syscall3(
        SYS_GETDENTS64,
        fd as usize,
        entries.as_mut_ptr() as usize,
        entries.len(),
    );
    check(encoded > 0, 69, "getdents64 /mnt");
    let encoded = encoded as usize;
    check(
        directory_has(&entries, encoded, b"abi-syscalls.txt")
            && directory_has(&entries, encoded, b"abi-created.txt")
            && directory_has(&entries, encoded, b"abi-link"),
        70,
        "getdents64 /mnt names",
    );
    // Second call: the snapshot is exhausted.
    check(
        syscall3(
            SYS_GETDENTS64,
            fd as usize,
            entries.as_mut_ptr() as usize,
            entries.len(),
        ) == 0,
        71,
        "getdents64 end",
    );
    check(syscall1(SYS_CLOSE, fd as usize) == 0, 72, "close /mnt");

    print("abi: ok\n");
    0
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // A panic in the fixture is itself a failure, but it cannot use `fail`
    // (which prints); park so the selftest times out on the missing exit instead
    // of pretending the program succeeded. The fixture's own checks run before
    // any bounds-dependent code, so this is a backstop, not a path.
    loop {}
}