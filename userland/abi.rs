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
//! The thread section (`abi: threads ok`) runs before the filesystem split, so
//! both the disk and the serial-only boot exercise `clone`/`gettid`/futex and
//! thread exit. With `/mnt` the fixture ends with `exit_group(0)` while a thread
//! is blocked in `FUTEX_WAIT`; without it, main exits 77 first and a tail thread
//! with a 30 ms futex deadline must keep the run alive — the run still returns
//! 77, main's code.
//!
//! No allocator, no `core::fmt`, no libc: syscall wrappers over `ecall` and
//! fixed stack buffers. Build with `userland/build.sh`.

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

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

const SYS_POLL: usize = 7;
const SYS_DUP: usize = 23;
const SYS_DUP2: usize = 24;
const SYS_PIPE2: usize = 59;
const SYS_PPOLL: usize = 72;
const SYS_MUNMAP: usize = 215;
const SYS_MMAP: usize = 222;
const SYS_MPROTECT: usize = 226;
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
const SYS_EXIT: usize = 93;
const SYS_EXIT_GROUP: usize = 94;
const SYS_SET_TID_ADDRESS: usize = 96;
const SYS_FUTEX: usize = 98;
const SYS_SCHED_YIELD: usize = 124;
const SYS_GETTID: usize = 178;
const SYS_CLONE: usize = 220;

const CLONE_VM: usize = 0x100;
const CLONE_FS: usize = 0x200;
const CLONE_FILES: usize = 0x400;
const CLONE_SIGHAND: usize = 0x800;
const CLONE_THREAD: usize = 0x10000;
const CLONE_SETTLS: usize = 0x80000;
const THREAD_FLAGS: usize = CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD;

const FUTEX_WAIT: usize = 0;
const FUTEX_WAKE: usize = 1;
const FUTEX_PRIVATE_FLAG: usize = 128;
const FUTEX_WAKE_OP: usize = 5;
const FUTEX_REQUEUE: usize = 3;

/// Tag in the low 16 bits of a thread's `arg`, so the entry can prove it got
/// exactly the value the parent passed.
const THREAD_MAGIC: usize = 0x7a11;

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

const PROT_READ: usize = 1;
const PROT_WRITE: usize = 2;
const PROT_EXEC: usize = 4;
const MAP_SHARED: usize = 1;
const MAP_PRIVATE: usize = 2;
const MAP_ANONYMOUS: usize = 0x20;

const POLLIN: i16 = 0x1;
const POLLOUT: i16 = 0x4;
const POLLHUP: i16 = 0x10;

const ENOENT: isize = -2;
const EBADF: isize = -9;
const EAGAIN: isize = -11;
const EFAULT: isize = -14;
const ENOTDIR: isize = -20;
const EISDIR: isize = -21;
const EINVAL: isize = -22;
const ENOMEM: isize = -12;
const ENOSYS: isize = -38;
const EEXIST: isize = -17;
const ENOTEMPTY: isize = -39;
const EPIPE: isize = -32;
const ENOTSUP: isize = -95;
const ENAMETOOLONG: isize = -36;
const ENOTSOCK: isize = -88;
const ENOPROTOOPT: isize = -92;
const EAFNOSUPPORT: isize = -97;
const ENETUNREACH: isize = -101;
const ENOSPC: isize = -28;
const ENOTCONN: isize = -107;
const ETIMEDOUT: isize = -110;
const ECONNREFUSED: isize = -111;
const EHOSTUNREACH: isize = -113;

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
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
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

/// Fill `words` 64-bit slots at `address` with `value`.
fn fill_words(address: usize, words: usize, value: u64) {
    // SAFETY: the caller passes an address returned by `mmap` and a word count
    // that fits inside that mapping; the stores stay within it.
    unsafe {
        let target = address as *mut u64;
        let mut index = 0usize;
        while index < words {
            *target.add(index) = value;
            index += 1;
        }
    }
}

/// True when every one of `words` slots at `address` is `value`.
fn words_match(address: usize, words: usize, value: u64) -> bool {
    // SAFETY: as `fill_words`; the reads stay inside the mapping the caller
    // created and the program still owns.
    unsafe {
        let target = address as *const u64;
        let mut index = 0usize;
        while index < words {
            if *target.add(index) != value {
                return false;
            }
            index += 1;
        }
    }
    true
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

// ─── threads and futexes ───────────────────────────────────────────────────

// Shared state below is in `.bss` (zeroed by the ELF loader), not on a stack:
// every thread of the run sees the same addresses, which is exactly what the
// CLONE_VM test needs.

static COUNTER: AtomicUsize = AtomicUsize::new(0);
static READY: [AtomicUsize; 2] = [AtomicUsize::new(0), AtomicUsize::new(0)];
static TIDS: [AtomicUsize; 2] = [AtomicUsize::new(0), AtomicUsize::new(0)];
static FINISHED: AtomicUsize = AtomicUsize::new(0);
static GATE_SEEN: [AtomicUsize; 2] = [AtomicUsize::new(0), AtomicUsize::new(0)];
static ARG_BAD: AtomicUsize = AtomicUsize::new(0);
static GATE: AtomicU32 = AtomicU32::new(0);

/// Tail-thread state for the "main exits first" case.
static TAIL_GATE: AtomicU32 = AtomicU32::new(0);

/// `exit_group` state: a thread blocks here forever and must be retired by the
/// group exit.
static GROUP_GATE: AtomicU32 = AtomicU32::new(0);
static GROUP_READY: AtomicUsize = AtomicUsize::new(0);
static GROUP_UNEXPECTED: AtomicUsize = AtomicUsize::new(0);

fn futex_wait_word(address: usize, value: u32, timeout: usize) -> isize {
    syscall6(
        SYS_FUTEX,
        address,
        FUTEX_WAIT | FUTEX_PRIVATE_FLAG,
        value as usize,
        timeout,
        0,
        0,
    )
}

fn futex_wake_word(address: usize, count: usize) -> isize {
    syscall6(
        SYS_FUTEX,
        address,
        FUTEX_WAKE | FUTEX_PRIVATE_FLAG,
        count,
        0,
        0,
        0,
    )
}

fn spawn_thread(flags: usize, stack: usize, entry: usize, arg: usize) -> isize {
    syscall4(SYS_CLONE, flags, stack, entry, arg)
}

/// Yield until `condition` holds; `fail` (which ends the whole fixture) after a
/// bounded number of turns, so a scheduler or futex bug reports a code instead
/// of hanging the boot.
fn wait_until(mut condition: impl FnMut() -> bool, code: i32, what: &str) {
    let mut turns = 0usize;
    while turns < 50_000 {
        if condition() {
            return;
        }
        let _ = syscall1(SYS_SCHED_YIELD, 0);
        turns += 1;
    }
    fail(code, what);
}

/// A child thread. It must be an `extern "C"` function that never returns: the
/// kernel enters it with `a0 = arg` and `sp` at the caller-provided stack.
extern "C" fn thread_entry(arg: usize) -> ! {
    let raw_index = (arg >> 16) & 0xffff;
    let index = if raw_index < 2 { raw_index } else { 0 };
    if raw_index >= 2 || (arg & 0xffff) != THREAD_MAGIC {
        ARG_BAD.fetch_add(1, Ordering::SeqCst);
    }

    // `gettid` must name this thread, not the process and not its sibling.
    let tid = syscall1(SYS_GETTID, 0) as usize;
    TIDS[index].store(tid, Ordering::SeqCst);

    // Shared counter: only correct if CLONE_VM really shares the address space
    // and both threads run.
    COUNTER.fetch_add(1, Ordering::SeqCst);
    READY[index].store(1, Ordering::SeqCst);

    // The rendezvous: wait until the parent sets GATE. A 2 s deadline turns a
    // lost wakeup into a failed check instead of a hung boot.
    let timeout = Timespec {
        seconds: 2,
        nanos: 0,
    };
    let result = futex_wait_word(
        GATE.as_ptr() as usize,
        0,
        &timeout as *const Timespec as usize,
    );
    if result == 0 || result == EAGAIN {
        GATE_SEEN[index].store(1, Ordering::SeqCst);
    } else {
        ARG_BAD.fetch_add(1, Ordering::SeqCst);
    }
    FINISHED.fetch_add(1, Ordering::SeqCst);

    // Thread exit, not process exit: the sibling and main keep running. The
    // distinct codes prove a non-main code is not the run's result.
    let code = if index == 1 { 42 } else { 0 };
    let _ = syscall1(SYS_EXIT, code);
    loop {}
}

/// The thread section. Returns a mapped stack address the caller may reuse for
/// the later one-thread tests (the mapping itself is run-owned).
fn thread_section() -> usize {
    // ── gettid / set_tid_address ──
    let main_tid = syscall1(SYS_GETTID, 0);
    check(main_tid > 0, 170, "gettid main");
    let mut tid_word: u32 = 0;
    check(
        syscall2(SYS_SET_TID_ADDRESS, &mut tid_word as *mut u32 as usize, 0) == main_tid,
        171,
        "set_tid_address",
    );

    // ── clone argument validation (before any mapping exists) ──
    let entry = thread_entry as *const () as usize;
    check(
        spawn_thread(CLONE_VM | CLONE_THREAD, 0, entry, 0) == EINVAL,
        172,
        "clone without stack",
    );
    check(
        spawn_thread(CLONE_VM, 0, entry, 0) == EINVAL,
        173,
        "clone without CLONE_THREAD",
    );
    check(
        spawn_thread(CLONE_VM | CLONE_THREAD | CLONE_SETTLS, 0, entry, 0) == ENOTSUP,
        174,
        "clone CLONE_SETTLS",
    );
    check(
        spawn_thread(CLONE_VM | CLONE_THREAD | 0x40000, 0, entry, 0) == EINVAL,
        175,
        "clone unknown flag",
    );

    // ── a private stack for two threads (one 32 KiB mapping, split in two) ──
    let region = syscall6(
        SYS_MMAP,
        0,
        0x8000,
        PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS,
        usize::MAX,
        0,
    );
    check(region > 0, 176, "mmap thread stacks");
    let stack0 = region as usize + 0x4000;
    let stack1 = region as usize + 0x8000;
    check(stack0 % 16 == 0 && stack1 % 16 == 0, 177, "stack alignment");

    // ── clone validation with real mappings ──
    check(
        spawn_thread(THREAD_FLAGS, stack0, 0, 0) == EINVAL,
        178,
        "clone entry 0",
    );
    check(
        spawn_thread(THREAD_FLAGS, stack0 + 8, entry, 0) == EINVAL,
        179,
        "clone unaligned stack",
    );
    check(
        spawn_thread(THREAD_FLAGS, 0x1000, entry, 0) == EINVAL,
        180,
        "clone unmapped stack",
    );
    check(
        spawn_thread(THREAD_FLAGS, stack0, stack0, 0) == EINVAL,
        181,
        "clone non-executable entry",
    );

    // ── futex argument/op paths before any waiter exists ──
    check(
        futex_wait_word(GATE.as_ptr() as usize, 1, 0) == EAGAIN,
        182,
        "futex wait value mismatch",
    );
    check(
        syscall6(
            SYS_FUTEX,
            GATE.as_ptr() as usize,
            FUTEX_REQUEUE | FUTEX_PRIVATE_FLAG,
            0,
            0,
            0,
            0,
        ) == ENOSYS,
        183,
        "futex requeue is ENOSYS",
    );
    check(
        syscall6(
            SYS_FUTEX,
            GATE.as_ptr() as usize,
            FUTEX_WAKE_OP | FUTEX_PRIVATE_FLAG,
            0,
            0,
            0,
            0,
        ) == ENOSYS,
        184,
        "futex wake_op is ENOSYS",
    );
    check(
        futex_wake_word(GATE.as_ptr() as usize, 2) == 0,
        185,
        "futex wake nobody",
    );

    // A timed wait on a word that still holds the expected value must block and
    // then time out; 10 ms is one tick. This is the deterministic proof that
    // FUTEX_WAIT reaches the scheduler instead of spinning or returning EAGAIN.
    let timeout = Timespec {
        seconds: 0,
        nanos: 10_000_000,
    };
    check(
        futex_wait_word(
            GATE.as_ptr() as usize,
            0,
            &timeout as *const Timespec as usize,
        ) == ETIMEDOUT,
        186,
        "futex wait timeout",
    );

    // ── two native threads ──
    let t0 = spawn_thread(THREAD_FLAGS, stack0, entry, (0 << 16) | THREAD_MAGIC);
    check(t0 > 0, 187, "clone thread 0");
    let t1 = spawn_thread(THREAD_FLAGS, stack1, entry, (1 << 16) | THREAD_MAGIC);
    check(t1 > 0, 188, "clone thread 1");
    check(
        t0 != t1 && t0 != main_tid && t1 != main_tid,
        189,
        "thread ids distinct",
    );

    wait_until(
        || COUNTER.load(Ordering::SeqCst) == 2,
        190,
        "threads increment shared counter",
    );
    check(
        TIDS[0].load(Ordering::SeqCst) == t0 as usize
            && TIDS[1].load(Ordering::SeqCst) == t1 as usize,
        191,
        "gettid in child matches clone tid",
    );

    // Both children are at (or about to reach) their futex wait. A few turns
    // let them block; the gate is set before the wake, so a child that has not
    // blocked yet sees the new value and returns EAGAIN — no lost wakeup either
    // way, which is the invariant the userspace ordering provides.
    for _ in 0..8 {
        let _ = syscall1(SYS_SCHED_YIELD, 0);
    }
    GATE.store(1, Ordering::SeqCst);
    let woken = futex_wake_word(GATE.as_ptr() as usize, 2);
    // Eight voluntary turns after both children announced READY are two full
    // rounds of the four runnable tasks (shell, main, two children), so both
    // children are parked in FUTEX_WAIT before the gate is set; the wake must
    // therefore report both. If this ever races, the event is a real scheduler
    // bug, not a timing assumption to paper over.
    check(woken == 2, 192, "futex wake count");

    wait_until(
        || FINISHED.load(Ordering::SeqCst) == 2,
        193,
        "threads finish after wake",
    );
    check(
        GATE_SEEN[0].load(Ordering::SeqCst) == 1 && GATE_SEEN[1].load(Ordering::SeqCst) == 1,
        194,
        "futex rendezvous gate seen",
    );
    check(
        ARG_BAD.load(Ordering::SeqCst) == 0,
        195,
        "thread arguments/gettid",
    );
    check(
        futex_wake_word(GATE.as_ptr() as usize, 2) == 0,
        196,
        "futex wake after exit",
    );

    print("abi: threads ok\n");
    stack1
}

/// The tail thread for the "main exits first" case: it blocks on a timeout,
/// prints when it returns, and exits with a code *different* from main's. The
/// run must stay alive until it is gone and must still return main's code.
extern "C" fn tail_entry(arg: usize) -> ! {
    let _ = arg;
    let timeout = Timespec {
        seconds: 0,
        nanos: 30_000_000,
    };
    let result = futex_wait_word(
        TAIL_GATE.as_ptr() as usize,
        0,
        &timeout as *const Timespec as usize,
    );
    if result == ETIMEDOUT {
        print("abi: tail thread timed out, main code wins\n");
    } else {
        print("abi: tail thread FAIL\n");
    }
    let _ = syscall1(SYS_EXIT, 99);
    loop {}
}

/// Spawn the tail thread that outlives main. Returns whether it started.
fn spawn_tail(stack: usize) -> bool {
    spawn_thread(THREAD_FLAGS, stack, tail_entry as *const () as usize, 0) > 0
}

/// The `exit_group` thread: announces itself, then blocks on a word nobody will
/// ever set. `exit_group` must retire it without another wakeup.
extern "C" fn group_entry(_arg: usize) -> ! {
    GROUP_READY.store(1, Ordering::SeqCst);
    let _ = futex_wait_word(GROUP_GATE.as_ptr() as usize, 0, 0);
    // A blocked thread must never get here: the only exit from the wait is the
    // group kill, and a killed task does not resume.
    GROUP_UNEXPECTED.store(1, Ordering::SeqCst);
    let _ = syscall1(SYS_EXIT, 1);
    loop {}
}

/// End the fixture with `exit_group(0)` while one thread is blocked: the run
/// returns 0 (the caller's code), the survivor is retired, and its kernel stack
/// and mappings are the runner's to release.
fn exit_group_case(stack: usize) {
    let tid = spawn_thread(THREAD_FLAGS, stack, group_entry as *const () as usize, 0);
    check(tid > 0, 197, "clone exit_group thread");
    wait_until(
        || GROUP_READY.load(Ordering::SeqCst) == 1,
        198,
        "exit_group thread ready",
    );
    // Let it reach the blocking wait; exit_group must handle Ready and Blocked
    // alike, so this is a best-effort staging, not a correctness requirement.
    for _ in 0..8 {
        let _ = syscall1(SYS_SCHED_YIELD, 0);
    }
    check(
        GROUP_UNEXPECTED.load(Ordering::SeqCst) == 0,
        199,
        "group thread stayed blocked",
    );
    print("abi: exit_group ok\n");
    let _ = syscall1(SYS_EXIT_GROUP, 0);
    loop {}
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

    // ── mmap / mprotect / munmap: anonymous private memory above brk ──
    const PAGE: usize = 4096;
    let mapped = syscall6(
        SYS_MMAP,
        0,
        4 * PAGE,
        PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS,
        usize::MAX,
        0,
    );
    check(mapped > 0, 140, "mmap anon");
    let mapped = mapped as usize;
    check(
        mapped >= 0x3000_0000 && mapped < 0x3f00_0000 && mapped % PAGE == 0,
        141,
        "mmap address in the window",
    );
    // SAFETY: `mapped` is a live 16 KiB anonymous mapping just created; the
    // first word is inside it.
    unsafe {
        check(
            *(mapped as *const u64) == 0,
            142,
            "mmap pages are zeroed",
        );
    }
    fill_words(mapped, 4 * PAGE / 8, 0x1122_3344_5566_7788);
    check(
        words_match(mapped, 4 * PAGE / 8, 0x1122_3344_5566_7788),
        143,
        "mmap write pattern",
    );
    // Leave a printable marker in the mapping so the read-back through the
    // now read-only page is visible in the serial log instead of eight binary
    // bytes.
    let marker = b"mprotect\n";
    for (index, byte) in marker.iter().enumerate() {
        // SAFETY: `mapped` is the live 16 KiB mapping; `marker` is nine bytes.
        unsafe { *((mapped + index) as *mut u8) = *byte };
    }
    check(
        syscall3(SYS_MPROTECT, mapped, 4 * PAGE, PROT_READ) == 0,
        144,
        "mprotect read-only",
    );
    // A kernel read through the now read-only page must still work: this is the
    // user-pointer validation path in `write` (and proves the PTE kept R).
    check(
        syscall3(SYS_WRITE, 1, mapped, marker.len()) == marker.len() as isize,
        145,
        "write reads the read-only mapping",
    );
    check(
        syscall3(
            SYS_MPROTECT,
            mapped,
            4 * PAGE,
            PROT_READ | PROT_WRITE,
        ) == 0,
        146,
        "mprotect read-write",
    );
    fill_words(mapped, 1, 0xdead_beef_0bad_f00d);
    check(
        words_match(mapped, 1, 0xdead_beef_0bad_f00d),
        147,
        "write after mprotect rw",
    );
    check(
        syscall2(SYS_MUNMAP, mapped, 4 * PAGE) == 0,
        148,
        "munmap",
    );
    check(
        syscall2(SYS_MUNMAP, mapped, 4 * PAGE) == EINVAL,
        149,
        "munmap twice",
    );
    check(
        syscall2(SYS_MUNMAP, mapped + 1, 4 * PAGE) == EINVAL,
        150,
        "munmap unaligned",
    );
    check(
        syscall3(SYS_MPROTECT, mapped, 4 * PAGE, PROT_READ) == EINVAL,
        151,
        "mprotect after munmap",
    );

    // A hint is honoured when it is page-aligned and free, and ignored when the
    // requested range is occupied: the second mapping must land somewhere else.
    let hint = 0x3100_0000usize;
    let first = syscall6(
        SYS_MMAP,
        hint,
        PAGE,
        PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS,
        usize::MAX,
        0,
    );
    check(first as usize == hint, 152, "mmap honours a free hint");
    let second = syscall6(
        SYS_MMAP,
        hint,
        PAGE,
        PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS,
        usize::MAX,
        0,
    );
    check(second > 0, 153, "mmap falls back from an occupied hint");
    let second = second as usize;
    check(
        second != hint && second % PAGE == 0,
        154,
        "mmap fallback is a different page",
    );
    fill_words(hint, 1, 0xaaaa_bbbb_cccc_dddd);
    fill_words(second, 1, 0x0123_4567_89ab_cdef);
    check(
        words_match(hint, 1, 0xaaaa_bbbb_cccc_dddd)
            && words_match(second, 1, 0x0123_4567_89ab_cdef),
        155,
        "two mappings are independent",
    );
    check(syscall2(SYS_MUNMAP, hint, PAGE) == 0, 156, "munmap hint");
    check(
        syscall2(SYS_MUNMAP, second, PAGE) == 0,
        157,
        "munmap fallback",
    );

    // Argument validation: zero length, shared/file-backed, W^X and a real fd
    // are all refused without touching the frame pool.
    check(
        syscall6(
            SYS_MMAP,
            0,
            0,
            PROT_READ,
            MAP_PRIVATE | MAP_ANONYMOUS,
            usize::MAX,
            0,
        ) == EINVAL,
        158,
        "mmap zero length",
    );
    check(
        syscall6(
            SYS_MMAP,
            0,
            PAGE,
            PROT_READ | PROT_WRITE,
            MAP_SHARED | MAP_ANONYMOUS,
            usize::MAX,
            0,
        ) == ENOTSUP,
        159,
        "mmap shared",
    );
    check(
        syscall6(
            SYS_MMAP,
            0,
            PAGE,
            PROT_WRITE | PROT_EXEC,
            MAP_PRIVATE | MAP_ANONYMOUS,
            usize::MAX,
            0,
        ) == EINVAL,
        160,
        "mmap W+X",
    );
    check(
        syscall6(
            SYS_MMAP,
            0,
            PAGE,
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANONYMOUS,
            4,
            0,
        ) == EINVAL,
        161,
        "mmap file-backed fd",
    );

    // ── pipe2, read/write, EOF, poll, dup/dup2 ──
    let mut pipe_fds = [0i32; 2];
    check(
        syscall2(SYS_PIPE2, pipe_fds.as_mut_ptr() as usize, 0) == 0,
        162,
        "pipe2",
    );
    let read_fd = pipe_fds[0];
    let write_fd = pipe_fds[1];
    check(
        read_fd >= 3 && write_fd >= 3 && read_fd != write_fd,
        163,
        "pipe2 descriptors",
    );

    let mut pipe_buffer = [0u8; 8];
    check(
        syscall3(
            SYS_READ,
            read_fd as usize,
            pipe_buffer.as_mut_ptr() as usize,
            pipe_buffer.len(),
        ) == EAGAIN,
        164,
        "read empty pipe is EAGAIN",
    );
    let mut poll_read = PollFd {
        fd: read_fd,
        events: POLLIN,
        revents: 0,
    };
    check(
        syscall3(
            SYS_POLL,
            &mut poll_read as *mut PollFd as usize,
            1,
            0,
        ) == 0
            && poll_read.revents == 0,
        165,
        "poll empty pipe",
    );
    let mut poll_write = PollFd {
        fd: write_fd,
        events: POLLOUT,
        revents: 0,
    };
    check(
        syscall3(
            SYS_POLL,
            &mut poll_write as *mut PollFd as usize,
            1,
            0,
        ) == 1
            && poll_write.revents & POLLOUT != 0,
        166,
        "poll write-ready pipe",
    );
    // A short timeout on a pipe with no data must come back 0, not spin forever.
    let mut poll_timeout = PollFd {
        fd: read_fd,
        events: POLLIN,
        revents: 0,
    };
    check(
        syscall3(
            SYS_POLL,
            &mut poll_timeout as *mut PollFd as usize,
            1,
            20,
        ) == 0,
        167,
        "poll timeout",
    );
    check(
        syscall3(
            SYS_WRITE,
            write_fd as usize,
            b"hello".as_ptr() as usize,
            5,
        ) == 5,
        168,
        "pipe write",
    );
    check(
        syscall3(
            SYS_POLL,
            &mut poll_read as *mut PollFd as usize,
            1,
            0,
        ) == 1
            && poll_read.revents & POLLIN != 0,
        169,
        "poll read-ready pipe",
    );
    check(
        syscall3(
            SYS_READ,
            read_fd as usize,
            pipe_buffer.as_mut_ptr() as usize,
            pipe_buffer.len(),
        ) == 5,
        170,
        "pipe read length",
    );
    check(
        buffer_equals(&pipe_buffer[..5], b"hello"),
        171,
        "pipe read content",
    );
    check(
        syscall3(
            SYS_READ,
            read_fd as usize,
            pipe_buffer.as_mut_ptr() as usize,
            pipe_buffer.len(),
        ) == EAGAIN,
        172,
        "pipe empty again",
    );
    check(syscall1(SYS_CLOSE, write_fd as usize) == 0, 173, "close pipe writer");
    let mut poll_hup = PollFd {
        fd: read_fd,
        events: POLLIN,
        revents: 0,
    };
    check(
        syscall3(
            SYS_POLL,
            &mut poll_hup as *mut PollFd as usize,
            1,
            0,
        ) == 1
            && poll_hup.revents & POLLHUP != 0,
        174,
        "poll reports HUP after the writer closes",
    );
    check(
        syscall3(
            SYS_READ,
            read_fd as usize,
            pipe_buffer.as_mut_ptr() as usize,
            pipe_buffer.len(),
        ) == 0,
        175,
        "pipe EOF after the writer closes",
    );
    check(syscall1(SYS_CLOSE, read_fd as usize) == 0, 176, "close pipe reader");

    // ppoll with a zero `Timespec` is poll's timeout 0 and sees a writable pipe.
    check(
        syscall2(SYS_PIPE2, pipe_fds.as_mut_ptr() as usize, 0) == 0,
        177,
        "pipe2 for ppoll",
    );
    let zero_timeout = Timespec {
        seconds: 0,
        nanos: 0,
    };
    let mut poll_ppoll = PollFd {
        fd: pipe_fds[1],
        events: POLLOUT,
        revents: 0,
    };
    check(
        syscall4(
            SYS_PPOLL,
            &mut poll_ppoll as *mut PollFd as usize,
            1,
            &zero_timeout as *const Timespec as usize,
            0,
        ) == 1
            && poll_ppoll.revents & POLLOUT != 0,
        178,
        "ppoll",
    );

    // dup: both descriptors share the 4 KiB ring; EOF waits for the last writer.
    let duped = syscall1(SYS_DUP, pipe_fds[1] as usize);
    check(duped >= 3 && duped != pipe_fds[1] as isize, 179, "dup");
    check(
        syscall3(SYS_WRITE, duped as usize, b"dup".as_ptr() as usize, 3) == 3,
        180,
        "write through dup",
    );
    check(
        syscall1(SYS_CLOSE, pipe_fds[1] as usize) == 0,
        181,
        "close original writer",
    );
    check(
        syscall3(SYS_WRITE, duped as usize, b"!".as_ptr() as usize, 1) == 1,
        182,
        "dup keeps the write end alive",
    );
    check(
        syscall3(
            SYS_READ,
            pipe_fds[0] as usize,
            pipe_buffer.as_mut_ptr() as usize,
            pipe_buffer.len(),
        ) == 4,
        183,
        "read both messages",
    );
    check(
        buffer_equals(&pipe_buffer[..4], b"dup!"),
        184,
        "dup shared the ring",
    );
    check(syscall1(SYS_CLOSE, duped as usize) == 0, 185, "close dup");
    check(
        syscall3(
            SYS_READ,
            pipe_fds[0] as usize,
            pipe_buffer.as_mut_ptr() as usize,
            pipe_buffer.len(),
        ) == 0,
        186,
        "EOF after the last writer",
    );
    check(
        syscall1(SYS_CLOSE, pipe_fds[0] as usize) == 0,
        187,
        "close pipe reader after dup",
    );

    // dup2 onto an explicit free slot; `oldfd == newfd` is the dup2 no-op.
    check(
        syscall2(SYS_PIPE2, pipe_fds.as_mut_ptr() as usize, 0) == 0,
        188,
        "pipe2 for dup2",
    );
    check(
        syscall3(
            SYS_DUP2,
            pipe_fds[0] as usize,
            pipe_fds[0] as usize,
            0,
        ) == pipe_fds[0] as isize,
        189,
        "dup2 same descriptor",
    );
    check(
        syscall3(SYS_DUP2, pipe_fds[1] as usize, 5, 0) == 5,
        190,
        "dup2 onto slot 5",
    );
    check(
        syscall3(SYS_WRITE, 5, b"xy".as_ptr() as usize, 2) == 2,
        191,
        "write through dup2",
    );
    check(
        syscall3(
            SYS_READ,
            pipe_fds[0] as usize,
            pipe_buffer.as_mut_ptr() as usize,
            pipe_buffer.len(),
        ) == 2
            && buffer_equals(&pipe_buffer[..2], b"xy"),
        192,
        "read through the dup2 pipe",
    );
    check(
        syscall1(SYS_CLOSE, pipe_fds[1] as usize) == 0,
        193,
        "close dup2 source",
    );
    check(syscall1(SYS_CLOSE, 5) == 0, 194, "close dup2 target");
    check(
        syscall3(
            SYS_READ,
            pipe_fds[0] as usize,
            pipe_buffer.as_mut_ptr() as usize,
            pipe_buffer.len(),
        ) == 0,
        195,
        "dup2 EOF after both writers close",
    );
    check(
        syscall1(SYS_CLOSE, pipe_fds[0] as usize) == 0,
        196,
        "close dup2 reader",
    );
    check(
        syscall3(SYS_DUP2, 999, 5, 0) == EBADF,
        197,
        "dup2 bad source",
    );

    // No reader: poll reports POLLERR and write is EPIPE, then closing the
    // writer frees the ring (the kernel-side selftest checks `live_pipes`).
    check(
        syscall2(SYS_PIPE2, pipe_fds.as_mut_ptr() as usize, 0) == 0,
        198,
        "pipe2 for EPIPE",
    );
    check(
        syscall1(SYS_CLOSE, pipe_fds[0] as usize) == 0,
        199,
        "close read end",
    );
    let mut poll_err = PollFd {
        fd: pipe_fds[1],
        events: POLLOUT,
        revents: 0,
    };
    check(
        syscall3(
            SYS_POLL,
            &mut poll_err as *mut PollFd as usize,
            1,
            0,
        ) == 1
            && poll_err.revents & 0x8 != 0,
        200,
        "poll POLLERR without a reader",
    );
    check(
        syscall3(
            SYS_WRITE,
            pipe_fds[1] as usize,
            b"x".as_ptr() as usize,
            1,
        ) == EPIPE,
        201,
        "write without a reader is EPIPE",
    );
    check(
        syscall1(SYS_CLOSE, pipe_fds[1] as usize) == 0,
        202,
        "close EPIPE writer",
    );

    let mut bad_pipe = [0i32; 2];
    check(
        syscall2(SYS_PIPE2, 0x10, 0) == EFAULT,
        203,
        "pipe2 bad pointer",
    );
    check(
        syscall2(SYS_PIPE2, bad_pipe.as_mut_ptr() as usize, 0x80000) == EINVAL,
        204,
        "pipe2 unknown flag",
    );
    let mut poll_bad = PollFd {
        fd: 99,
        events: POLLIN,
        revents: 0,
    };
    check(
        syscall3(
            SYS_POLL,
            &mut poll_bad as *mut PollFd as usize,
            1,
            0,
        ) == 1
            && poll_bad.revents & 0x20 != 0,
        205,
        "poll reports POLLNVAL",
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

    // ── native threads: clone/gettid/futex/thread exit (runs with or without /mnt) ──
    let thread_stack = thread_section();

    // ── the filesystem half: skip when the selftest could not provide /mnt ──
    let path = b"/mnt/abi-syscalls.txt\0";
    let fd = syscall4(SYS_OPENAT, 0, path.as_ptr() as usize, O_RDONLY, 0);
    if fd == ENOENT {
        print("abi: no /mnt, no filesystem half\n");
        // Main exits first; this thread is still blocked in FUTEX_WAIT with a
        // 30 ms deadline. The run must stay alive until it has exited and must
        // return main's 77, not the tail's 99.
        check(spawn_tail(thread_stack), 200, "spawn tail thread");
        let _ = syscall1(SYS_EXIT, 77);
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
    exit_group_case(thread_stack);
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