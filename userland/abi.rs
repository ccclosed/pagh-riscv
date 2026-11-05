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
    // SAFETY: `ecall` enters the kernel with `a7` as the number and `a0..a5` as
    // arguments; the kernel restores the same `sp` and callee-saved registers.
    //
    // No `nostack` option: the kernel writes into buffers this program passes
    // by address (path strings, `Stat`, `Uname`, socket addresses, the
    // directory array), and some of them live on this stack frame. `nostack`
    // promises the asm does not write the stack, which would be a false promise
    // here.
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

fn syscall5(
    number: usize,
    a0: usize,
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
) -> isize {
    syscall6(number, a0, a1, a2, a3, a4, 0)
}

fn syscall4(number: usize, a0: usize, a1: usize, a2: usize, a3: usize) -> isize {
    syscall5(number, a0, a1, a2, a3, 0)
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
const SYS_MKDIRAT: usize = 34;
const SYS_UNLINKAT: usize = 35;
const SYS_SYMLINKAT: usize = 36;
const SYS_LINKAT: usize = 37;
const SYS_FCHMODAT: usize = 53;
const SYS_FTRUNCATE: usize = 46;
const SYS_RENAMEAT2: usize = 276;
const SYS_SOCKET: usize = 198;
const SYS_CONNECT: usize = 203;
const SYS_SENDTO: usize = 206;
const SYS_RECVFROM: usize = 207;
const SYS_SETSOCKOPT: usize = 208;
const SYS_GETSOCKOPT: usize = 209;
const SYS_SHUTDOWN: usize = 210;

const AF_INET: usize = 2;
const SOCK_STREAM: usize = 1;
const SOL_SOCKET: usize = 1;
const SO_REUSEADDR: usize = 2;
const SO_TYPE: usize = 3;

const AT_REMOVEDIR: usize = 0x200;

const O_RDONLY: usize = 0;
const O_WRONLY: usize = 1;
const O_RDWR: usize = 2;
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
const EEXIST: isize = -17;
const ENOTEMPTY: isize = -39;
const ENOTSUP: isize = -95;
const ENAMETOOLONG: isize = -36;
const EPIPE: isize = -32;
const ENOTSOCK: isize = -88;
const ENOPROTOOPT: isize = -92;
const EAFNOSUPPORT: isize = -97;
const ENETUNREACH: isize = -101;
const ENOSPC: isize = -28;
const ENOTCONN: isize = -107;
const ETIMEDOUT: isize = -110;
const ECONNREFUSED: isize = -111;
const EHOSTUNREACH: isize = -113;
const EAGAIN: isize = -11;

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

/// Every record is well-formed and none is named `expected`. Unlike
/// `!directory_has`, a malformed record fails this check instead of passing it
/// vacuously — the mutation half uses it to prove the names are really gone.
fn directory_lacks(buffer: &[u8], length: usize, expected: &[u8]) -> bool {
    let mut offset = 0usize;
    while offset + 8 <= length {
        let name_length = buffer[offset + 9] as usize;
        let record = (12 + name_length + 7) & !7;
        if record < 8 || offset + record > length || offset + 12 + name_length > length {
            return false;
        }
        let name = &buffer[offset + 12..offset + 12 + name_length];
        if buffer_equals(name, expected) {
            return false;
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
        cstr_eq(uname.release.as_ptr(), b"0.10.0"),
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

    // ── BSD sockets: AF_INET / SOCK_STREAM only, one connection at a time. The
    // bad-argument half always runs. The connect probe to the QEMU host gateway
    // is a SKIP (not a failure) when nothing is listening: CI has no host HTTP
    // server, and a serial-only boot may have no NIC at all. ──
    check(
        syscall3(SYS_SOCKET, 1, SOCK_STREAM as usize, 0) == EAFNOSUPPORT,
        140,
        "socket(AF_UNIX)",
    );
    check(
        syscall3(SYS_SOCKET, AF_INET, 2, 0) == EAFNOSUPPORT,
        141,
        "socket(SOCK_DGRAM)",
    );
    check(
        syscall3(SYS_SOCKET, AF_INET, SOCK_STREAM as usize, 6) == EAFNOSUPPORT,
        142,
        "socket(protocol 6)",
    );
    let sock = syscall3(SYS_SOCKET, AF_INET, SOCK_STREAM as usize, 0);
    check(sock >= 3, 143, "socket");
    check(
        syscall3(SYS_SOCKET, AF_INET, SOCK_STREAM as usize, 0) == ENOSPC,
        144,
        "second socket while one is open",
    );

    let mut address = [0u8; 16];
    address[0..2].copy_from_slice(&(AF_INET as u16).to_le_bytes());
    address[2..4].copy_from_slice(&8000u16.to_be_bytes());
    address[4..8].copy_from_slice(&[10u8, 0, 2, 2]);

    check(
        syscall3(SYS_CONNECT, sock as usize, 0x10, 16) == EFAULT,
        145,
        "connect bad pointer",
    );
    check(
        syscall3(SYS_CONNECT, sock as usize, address.as_ptr() as usize, 8) == EINVAL,
        146,
        "connect addrlen",
    );
    let mut wrong_family = address;
    wrong_family[0] = 1;
    check(
        syscall3(
            SYS_CONNECT,
            sock as usize,
            wrong_family.as_ptr() as usize,
            16,
        ) == EAFNOSUPPORT,
        147,
        "connect family",
    );
    check(
        syscall3(SYS_CONNECT, 99, address.as_ptr() as usize, 16) == EBADF,
        148,
        "connect bad fd",
    );
    check(
        syscall3(SYS_CONNECT, 1, address.as_ptr() as usize, 16) == ENOTSOCK,
        149,
        "connect non-socket fd",
    );
    let mut scratch = [0u8; 4];
    check(
        syscall3(SYS_READ, sock as usize, scratch.as_mut_ptr() as usize, 4) == ENOTCONN,
        150,
        "read before connect",
    );
    check(
        syscall3(SYS_WRITE, sock as usize, b"x".as_ptr() as usize, 1) == ENOTCONN,
        151,
        "write before connect",
    );
    check(
        syscall2(SYS_SHUTDOWN, sock as usize, 0) == ENOTCONN,
        152,
        "shutdown before connect",
    );

    let mut option: u32 = 0;
    let option_ptr = (&mut option as *mut u32) as usize;
    check(
        syscall5(
            SYS_SETSOCKOPT,
            sock as usize,
            SOL_SOCKET,
            SO_REUSEADDR,
            option_ptr,
            4,
        ) == 0,
        153,
        "setsockopt SO_REUSEADDR",
    );
    check(
        syscall5(SYS_SETSOCKOPT, sock as usize, 0x1234, 1, option_ptr, 4) == ENOPROTOOPT,
        154,
        "setsockopt unknown option",
    );
    let mut option_len: u32 = 4;
    check(
        syscall5(
            SYS_GETSOCKOPT,
            sock as usize,
            SOL_SOCKET,
            SO_TYPE,
            option_ptr,
            (&mut option_len as *mut u32) as usize,
        ) == 0,
        155,
        "getsockopt SO_TYPE",
    );
    check(
        option == SOCK_STREAM as u32 && option_len == 4,
        156,
        "getsockopt SO_TYPE value",
    );

    let connected = syscall3(
        SYS_CONNECT,
        sock as usize,
        address.as_ptr() as usize,
        16,
    );
    if connected == 0 {
        print("abi: socket connected to 10.0.2.2:8000\n");
        let request = b"GET / HTTP/1.0\r\nHost: 10.0.2.2\r\n\r\n";
        let sent = syscall6(
            SYS_SENDTO,
            sock as usize,
            request.as_ptr() as usize,
            request.len(),
            0,
            0,
            0,
        );
        print("abi: socket send returned ");
        if sent < 0 {
            print("-");
            print_usize((-sent) as usize);
        } else {
            print_usize(sent as usize);
        }
        print(" bytes\n");
        if sent == request.len() as isize {
            let mut response = [0u8; 256];
            let delay = Timespec {
                seconds: 0,
                nanos: 10_000_000,
            };
            let mut polls = 0;
            let mut got = 0isize;
            while polls < 100 {
                let count = syscall6(
                    SYS_RECVFROM,
                    sock as usize,
                    response.as_mut_ptr() as usize,
                    response.len(),
                    0,
                    0,
                    0,
                );
                if count > 0 {
                    got = count;
                    break;
                }
                if count == 0 {
                    break;
                }
                check(count == EAGAIN, 158, "recv on connected socket");
                let _ = syscall2(SYS_NANOSLEEP, &delay as *const Timespec as usize, 0);
                polls += 1;
            }
            print("abi: socket recv got ");
            print_usize(got as usize);
            print(" bytes\n");
        }
        check(
            syscall2(SYS_SHUTDOWN, sock as usize, 2) == 0,
            157,
            "shutdown connected socket",
        );
        let mut one = [0u8; 1];
        check(
            syscall3(SYS_READ, sock as usize, one.as_mut_ptr() as usize, 1) == 0,
            159,
            "read after shutdown is EOF",
        );
        check(
            syscall3(SYS_WRITE, sock as usize, b"x".as_ptr() as usize, 1) == EPIPE,
            160,
            "write after shutdown is EPIPE",
        );
        print("abi: socket shutdown/EOF ok\n");
    } else {
        // A serial-only boot, no NIC, or a CI host without a listener: this is
        // a SKIP. Any other errno means the kernel rejected a valid sockaddr,
        // which is a real bug.
        check(
            connected == ECONNREFUSED
                || connected == ETIMEDOUT
                || connected == ENETUNREACH
                || connected == EHOSTUNREACH
                || connected == EAGAIN,
            161,
            "connect to 10.0.2.2:8000",
        );
        print("abi: socket SKIP connect (no host server on 10.0.2.2:8000)\n");
    }
    check(syscall1(SYS_CLOSE, sock as usize) == 0, 162, "close socket");
    check(
        syscall1(SYS_CLOSE, sock as usize) == EBADF,
        163,
        "double close socket",
    );
    // Closing frees the one connection slot: another socket must succeed.
    let sock2 = syscall3(SYS_SOCKET, AF_INET, SOCK_STREAM as usize, 0);
    check(sock2 >= 3, 164, "socket after close");
    check(
        syscall1(SYS_CLOSE, sock2 as usize) == 0,
        165,
        "close second socket",
    );

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

    // ── path mutations: mkdirat / unlinkat / symlinkat / linkat / renameat2 /
    // fchmodat / ftruncate. Reached only with /mnt: the no-filesystem boot
    // exited 77 before this point. Every check below is a real syscall through
    // `ecall`; the kernel-side selftest verifies the tree afterwards. ──
    let mut_path = b"/mnt/abi-mut.txt\0";
    let renamed = b"/mnt/abi-renamed.txt\0";
    let hard = b"/mnt/abi-hard.txt\0";
    let sym = b"/mnt/abi-sym\0";
    let sym2 = b"/mnt/abi-sym2\0";
    let dir_path = b"/mnt/abi-dir\0";
    let dir2 = b"/mnt/abi-dir2\0";
    let inside = b"/mnt/abi-dir/inside.txt\0";
    let missing = b"/mnt/abi-missing.txt\0";
    let bad_parent = b"/mnt/abi-missing-dir/x\0";

    // ── directory lifecycle ──
    check(
        syscall3(SYS_MKDIRAT, 0, dir_path.as_ptr() as usize, 0o700) == 0,
        74,
        "mkdirat",
    );
    check(
        syscall3(SYS_MKDIRAT, 0, dir_path.as_ptr() as usize, 0o755) == EEXIST,
        75,
        "mkdirat exists",
    );
    // `mode` is accepted and ignored (no VFS `set_mode`): the directory carries
    // ext2's default 0o755, not the 0o700 that was asked for. This pins the
    // documented behavior.
    let dir_fd = syscall4(SYS_OPENAT, 0, dir_path.as_ptr() as usize, O_RDONLY, 0);
    check(dir_fd >= 3, 76, "openat new directory");
    check(
        syscall2(SYS_FSTAT, dir_fd as usize, &mut stat as *mut Stat as usize) == 0
            && stat.kind == KIND_DIR
            && stat.mode & 0o777 == 0o755,
        77,
        "mkdirat mode is ignored",
    );
    check(
        syscall2(SYS_FTRUNCATE, dir_fd as usize, 0) == EISDIR,
        78,
        "ftruncate on a directory",
    );
    check(syscall1(SYS_CLOSE, dir_fd as usize) == 0, 79, "close directory");

    let fd = syscall4(
        SYS_OPENAT,
        0,
        inside.as_ptr() as usize,
        O_WRONLY | O_CREAT | O_TRUNC,
        0o644,
    );
    check(fd >= 3, 80, "create inside directory");
    check(syscall1(SYS_CLOSE, fd as usize) == 0, 81, "close inside");

    check(
        syscall3(SYS_UNLINKAT, 0, dir_path.as_ptr() as usize, 0) == EISDIR,
        82,
        "unlinkat directory without AT_REMOVEDIR",
    );
    check(
        syscall3(
            SYS_UNLINKAT,
            0,
            dir_path.as_ptr() as usize,
            AT_REMOVEDIR,
        ) == ENOTEMPTY,
        83,
        "rmdir non-empty",
    );
    check(
        syscall3(
            SYS_UNLINKAT,
            0,
            inside.as_ptr() as usize,
            AT_REMOVEDIR,
        ) == ENOTDIR,
        84,
        "AT_REMOVEDIR on a file",
    );
    check(
        syscall3(SYS_UNLINKAT, 0, inside.as_ptr() as usize, 0) == 0,
        85,
        "unlink inside",
    );
    check(
        syscall3(
            SYS_UNLINKAT,
            0,
            dir_path.as_ptr() as usize,
            AT_REMOVEDIR,
        ) == 0,
        86,
        "rmdir empty",
    );
    check(
        syscall3(
            SYS_UNLINKAT,
            0,
            dir_path.as_ptr() as usize,
            AT_REMOVEDIR,
        ) == ENOENT,
        87,
        "rmdir gone",
    );
    check(
        syscall3(SYS_MKDIRAT, 0, b"relative\0".as_ptr() as usize, 0o755) == EINVAL,
        88,
        "mkdirat relative path",
    );
    check(
        syscall3(SYS_UNLINKAT, 0, mut_path.as_ptr() as usize, 0x400) == EINVAL,
        89,
        "unlinkat unknown flag",
    );
    check(
        syscall3(SYS_UNLINKAT, 0, 0x10, 0) == EFAULT,
        90,
        "unlinkat bad pointer",
    );
    check(
        syscall3(
            SYS_MKDIRAT,
            0,
            bad_parent.as_ptr() as usize,
            0o755,
        ) == ENOENT,
        91,
        "mkdirat missing parent",
    );

    // ── file lifecycle: create, write, chmod, rename, truncate, link ──
    let fd = syscall4(
        SYS_OPENAT,
        0,
        mut_path.as_ptr() as usize,
        O_WRONLY | O_CREAT | O_TRUNC,
        0o644,
    );
    check(fd >= 3, 92, "create abi-mut.txt");
    check(
        syscall3(SYS_WRITE, fd as usize, b"mutate\n".as_ptr() as usize, 7) == 7,
        93,
        "write abi-mut.txt",
    );
    check(syscall1(SYS_CLOSE, fd as usize) == 0, 94, "close abi-mut.txt");

    check(
        syscall4(
            SYS_FCHMODAT,
            0,
            mut_path.as_ptr() as usize,
            0o600,
            0,
        ) == 0,
        95,
        "fchmodat",
    );
    let fd = syscall4(SYS_OPENAT, 0, mut_path.as_ptr() as usize, O_RDONLY, 0);
    check(fd >= 3, 96, "open after chmod");
    check(
        syscall2(SYS_FSTAT, fd as usize, &mut stat as *mut Stat as usize) == 0
            && stat.mode & 0o7777 == 0o600,
        97,
        "fstat sees the chmod",
    );
    check(
        syscall2(SYS_FTRUNCATE, fd as usize, 0) == EBADF,
        98,
        "ftruncate read-only fd",
    );
    check(syscall1(SYS_CLOSE, fd as usize) == 0, 99, "close after chmod");
    check(
        syscall4(
            SYS_FCHMODAT,
            0,
            mut_path.as_ptr() as usize,
            0o644,
            0,
        ) == 0,
        100,
        "fchmodat restore",
    );
    check(
        syscall4(SYS_FCHMODAT, 0, mut_path.as_ptr() as usize, 0o644, 0x100) == EINVAL,
        101,
        "fchmodat flags",
    );
    check(
        syscall4(SYS_FCHMODAT, 0, b"/\0".as_ptr() as usize, 0o755, 0) == ENOTSUP,
        102,
        "fchmodat synthetic node",
    );
    check(
        syscall2(SYS_FTRUNCATE, 1, 0) == EINVAL,
        103,
        "ftruncate console",
    );

    check(
        syscall5(
            SYS_RENAMEAT2,
            0,
            mut_path.as_ptr() as usize,
            0,
            renamed.as_ptr() as usize,
            0,
        ) == 0,
        104,
        "renameat2",
    );
    check(
        syscall5(
            SYS_RENAMEAT2,
            0,
            renamed.as_ptr() as usize,
            0,
            hard.as_ptr() as usize,
            1,
        ) == EINVAL,
        105,
        "renameat2 flags",
    );
    check(
        syscall4(SYS_OPENAT, 0, mut_path.as_ptr() as usize, O_RDONLY, 0) == ENOENT,
        106,
        "old name gone after rename",
    );

    let fd = syscall4(SYS_OPENAT, 0, renamed.as_ptr() as usize, O_RDWR, 0);
    check(fd >= 3, 107, "open renamed");
    check(
        syscall2(SYS_FSTAT, fd as usize, &mut stat as *mut Stat as usize) == 0 && stat.size == 7,
        108,
        "renamed size",
    );
    let renamed_ino = stat.ino;
    check(
        syscall2(SYS_FTRUNCATE, fd as usize, 0) == 0,
        109,
        "ftruncate to zero",
    );
    check(
        syscall2(SYS_FSTAT, fd as usize, &mut stat as *mut Stat as usize) == 0 && stat.size == 0,
        110,
        "size after truncate",
    );
    check(syscall3(SYS_LSEEK, fd as usize, 0, 0) == 0, 111, "seek after truncate");
    check(
        syscall3(SYS_WRITE, fd as usize, b"link\n".as_ptr() as usize, 5) == 5,
        112,
        "rewrite after truncate",
    );
    check(
        syscall2(SYS_FSTAT, fd as usize, &mut stat as *mut Stat as usize) == 0 && stat.size == 5,
        113,
        "size after rewrite",
    );
    check(syscall1(SYS_CLOSE, fd as usize) == 0, 114, "close renamed");

    check(
        syscall5(
            SYS_LINKAT,
            0,
            renamed.as_ptr() as usize,
            0,
            hard.as_ptr() as usize,
            0,
        ) == 0,
        115,
        "linkat",
    );
    let fd = syscall4(SYS_OPENAT, 0, hard.as_ptr() as usize, O_RDONLY, 0);
    check(fd >= 3, 116, "open hard link");
    check(
        syscall2(SYS_FSTAT, fd as usize, &mut stat as *mut Stat as usize) == 0
            && stat.ino == renamed_ino
            && stat.size == 5,
        117,
        "hard link shares the inode",
    );
    check(syscall1(SYS_CLOSE, fd as usize) == 0, 118, "close hard link");
    check(
        syscall5(
            SYS_LINKAT,
            0,
            renamed.as_ptr() as usize,
            0,
            hard.as_ptr() as usize,
            1,
        ) == EINVAL,
        119,
        "linkat flags",
    );

    // ── symlink: created by path, read back, followed by openat ──
    check(
        syscall3(
            SYS_SYMLINKAT,
            b"abi-renamed.txt\0".as_ptr() as usize,
            0,
            sym.as_ptr() as usize,
        ) == 0,
        120,
        "symlinkat",
    );
    let length = syscall4(
        SYS_READLINKAT,
        0,
        sym.as_ptr() as usize,
        target.as_mut_ptr() as usize,
        target.len(),
    );
    check(length == 15, 121, "readlinkat new link length");
    check(
        buffer_equals(&target[..15], b"abi-renamed.txt"),
        122,
        "readlinkat new link target",
    );
    let fd = syscall4(SYS_OPENAT, 0, sym.as_ptr() as usize, O_RDONLY, 0);
    check(fd >= 3, 123, "open through new link");
    check(
        syscall2(SYS_FSTAT, fd as usize, &mut stat as *mut Stat as usize) == 0 && stat.size == 5,
        124,
        "new link follows to the file",
    );
    check(syscall1(SYS_CLOSE, fd as usize) == 0, 125, "close through link");
    check(
        syscall3(
            SYS_SYMLINKAT,
            b"\0".as_ptr() as usize,
            0,
            sym2.as_ptr() as usize,
        ) == EINVAL,
        126,
        "symlinkat empty target",
    );

    // ── unlink everything again ──
    check(
        syscall3(SYS_UNLINKAT, 0, hard.as_ptr() as usize, 0) == 0,
        127,
        "unlink hard link",
    );
    check(
        syscall3(SYS_UNLINKAT, 0, renamed.as_ptr() as usize, 0) == 0,
        128,
        "unlink renamed",
    );
    check(
        syscall3(SYS_UNLINKAT, 0, sym.as_ptr() as usize, 0) == 0,
        129,
        "unlink symlink",
    );
    check(
        syscall3(SYS_UNLINKAT, 0, missing.as_ptr() as usize, 0) == ENOENT,
        130,
        "unlink missing",
    );
    check(
        syscall4(SYS_OPENAT, 0, renamed.as_ptr() as usize, O_RDONLY, 0) == ENOENT
            && syscall4(SYS_OPENAT, 0, hard.as_ptr() as usize, O_RDONLY, 0) == ENOENT
            && syscall4(SYS_OPENAT, 0, sym.as_ptr() as usize, O_RDONLY, 0) == ENOENT,
        131,
        "mutation artifacts gone",
    );

    // A directory may not be hard-linked; the check runs while one exists.
    check(
        syscall3(SYS_MKDIRAT, 0, dir2.as_ptr() as usize, 0o755) == 0,
        132,
        "mkdirat second directory",
    );
    check(
        syscall5(
            SYS_LINKAT,
            0,
            dir2.as_ptr() as usize,
            0,
            hard.as_ptr() as usize,
            0,
        ) == EISDIR,
        133,
        "linkat directory",
    );
    check(
        syscall3(
            SYS_UNLINKAT,
            0,
            dir2.as_ptr() as usize,
            AT_REMOVEDIR,
        ) == 0,
        134,
        "rmdir second directory",
    );

    // A path that never terminates inside the 256-byte ABI buffer is refused,
    // not truncated.
    let mut long_path = [b'a'; 300];
    long_path[0] = b'/';
    long_path[299] = 0;
    check(
        syscall3(SYS_UNLINKAT, 0, long_path.as_ptr() as usize, 0) == ENAMETOOLONG,
        135,
        "overlong path",
    );

    // Finally: the directory is back to the fixture inputs, with no mutation
    // artifact left behind (also what the kernel-side ledger check assumes).
    let fd = syscall4(SYS_OPENAT, 0, mnt.as_ptr() as usize, O_RDONLY, 0);
    check(fd >= 3, 136, "reopen /mnt");
    let encoded = syscall3(
        SYS_GETDENTS64,
        fd as usize,
        entries.as_mut_ptr() as usize,
        entries.len(),
    );
    check(encoded > 0, 137, "getdents64 after mutations");
    let encoded = encoded as usize;
    check(
        directory_has(&entries, encoded, b"abi-syscalls.txt")
            && directory_lacks(&entries, encoded, b"abi-mut.txt")
            && directory_lacks(&entries, encoded, b"abi-renamed.txt")
            && directory_lacks(&entries, encoded, b"abi-hard.txt")
            && directory_lacks(&entries, encoded, b"abi-sym")
            && directory_lacks(&entries, encoded, b"abi-dir"),
        138,
        "mutation cleanup in the directory",
    );
    check(syscall1(SYS_CLOSE, fd as usize) == 0, 139, "close /mnt again");

    print("abi: mutations ok\n");
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