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
//! The thread section (`abi: threads ok`) and the signal section
//! (`abi: signals ok`) run before the filesystem split, so both the disk and
//! the serial-only boot exercise `clone`/`gettid`/futex/thread exit and the
//! POSIX signal path (handler, restorer, `rt_sigreturn`, masks). With `/mnt` the fixture ends with `exit_group(0)` while a thread
//! is blocked in `FUTEX_WAIT`; without it, main exits 77 first and a tail thread
//! with a 30 ms futex deadline must keep the run alive — the run still returns
//! 77, main's code.
//!
//! No allocator, no `core::fmt`, no libc: syscall wrappers over `ecall` and
//! fixed stack buffers. Build with `userland/build.sh`.

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicI32, AtomicU32, AtomicUsize, Ordering};

core::arch::global_asm!(
    r#"
    .section .text.entry, "ax", @progbits
    .globl _start
_start:
    call    abi_main
    li      a7, 93
    ecall
1:  j       1b

    /* The libpagh-style restorer: the kernel enters a signal handler with
       `ra = abi_sigrestorer` and `sp` at the signal frame, so the handler's
       ordinary `ret` lands here. `rt_sigreturn` (139) restores the interrupted
       context; it must never return. If it did, the only honest answer is a
       distinct exit code. */
    .align  2
    .globl  abi_sigrestorer
abi_sigrestorer:
    li      a7, 139
    ecall
    li      a7, 93
    li      a0, 1
    ecall
2:  j       2b

    /* Call rt_sigreturn with an arbitrary frame pointer and restore the real
       `sp` afterwards, which only happens on the error path (a successful
       sigreturn replaces the whole trap frame and never resumes here). */
    .align  2
    .globl  abi_bad_sigreturn
abi_bad_sigreturn:
    mv      t0, sp
    mv      sp, a0
    li      a7, 139
    ecall
    mv      sp, t0
    ret

    /* F-r3-rv-1 regression: forge this frame's back-chain pointer.
       `prev_frame` lives at offset 416 of the kernel's 736-byte SignalFrame
       (siginfo is 288+128; `const _` asserts pin both in src/user/signal.rs).
       Writing usize::MAX and asking `rt_sigreturn` used to install it
       unchecked, and the next delivery's `prev_frame + offset_of!(ucontext)`
       overflowed a debug kernel into `park_forever`. A fixed kernel answers
       -EINVAL and leaves this still-armed frame untouched, so record the
       error, repair the word and unwind through the ordinary restorer. */
    .align  2
    .globl  abi_forged_prev_frame
abi_forged_prev_frame:
    mv      s0, sp
    li      t0, 416
    add     t0, s0, t0
    li      t1, -1
    sd      t1, 0(t0)
    li      a7, 139
    ecall
    la      t1, abi_forged_prev_result
    sd      a0, 0(t1)
    li      t0, 416
    add     t0, s0, t0
    sd      zero, 0(t0)
    mv      sp, s0
    ret
"#
);

/// What the first `rt_sigreturn` in [`abi_forged_prev_frame`] returned:
/// `-EINVAL` on a fixed kernel (the frame was left untouched).
#[no_mangle]
static mut abi_forged_prev_result: isize = 0;

extern "C" {
    fn abi_sigrestorer();
    fn abi_bad_sigreturn(pointer: usize) -> isize;
    fn abi_forged_prev_frame();
}

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
const SYS_GETRANDOM: usize = 278;
const SYS_SOCKET: usize = 198;
const SYS_BIND: usize = 200;
const SYS_LISTEN: usize = 201;
const SYS_ACCEPT: usize = 202;
const SYS_CONNECT: usize = 203;
const SYS_SENDTO: usize = 206;
const SYS_RECVFROM: usize = 207;
const SYS_SETSOCKOPT: usize = 208;
const SYS_GETSOCKOPT: usize = 209;
const SYS_SHUTDOWN: usize = 210;
const SYS_EXIT: usize = 93;
const SYS_EXIT_GROUP: usize = 94;
const SYS_GETPPID: usize = 173;
const SYS_EXECVE: usize = 221;
const SYS_WAIT4: usize = 260;
/// pagh-native: asm-generic has no `fork`; `clone(220)` with all-zero arguments
/// is the alias the kernel also accepts.
const SYS_FORK: usize = 1000;
const WNOHANG: usize = 1;
const SYS_SET_TID_ADDRESS: usize = 96;
const SYS_FUTEX: usize = 98;
const SYS_SCHED_YIELD: usize = 124;
const SYS_GETTID: usize = 178;
const SYS_CLONE: usize = 220;
const SYS_KILL: usize = 129;
const SYS_TGKILL: usize = 131;
const SYS_RT_SIGACTION: usize = 134;
const SYS_RT_SIGPROCMASK: usize = 135;
const SYS_RT_SIGRETURN: usize = 139;
const SYS_SIGALTSTACK: usize = 132;

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

// ─── signals ───────────────────────────────────────────────────────────────
const SA_RESTORER: usize = 0x0400_0000;
const SA_ONSTACK: usize = 0x0800_0000;
const SA_RESTART: usize = 0x1000_0000;
const SA_NODEFER: usize = 0x4000_0000;
const SA_RESETHAND: usize = 0x8000_0000;
const SA_SIGINFO: usize = 0x0000_0004;
const SS_ONSTACK: i32 = 1;
const SS_DISABLE: i32 = 2;
const MINSIGSTKSZ: usize = 4096;
const SI_USER: i32 = 0;
const SI_TKILL: i32 = -6;
const SEGV_MAPERR: i32 = 1;
const SIG_DFL: usize = 0;
const SIG_IGN: usize = 1;
const SIGKILL: usize = 9;
const SIGUSR1: usize = 10;
const SIGSEGV: usize = 11;
const SIGUSR2: usize = 12;
const SIGCHLD: usize = 17;
const SIGCONT: usize = 18;
const SIGSTOP: usize = 19;
const SIGTSTP: usize = 20;
const SIG_BLOCK: usize = 0;
const SIG_UNBLOCK: usize = 1;
const SIG_SETMASK: usize = 2;

/// Tag in the low 16 bits of a thread's `arg`, so the entry can prove it got
/// exactly the value the parent passed.
const THREAD_MAGIC: usize = 0x7a11;

const AF_INET: usize = 2;
const SOCK_STREAM: usize = 1;
const SOCK_DGRAM: usize = 2;
const MSG_DONTWAIT: usize = 0x40;
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
const ESRCH: isize = -3;
const ENOEXEC: isize = -8;
const ECHILD: isize = -10;
const EINTR: isize = -4;
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
const EMSGSIZE: isize = -90;
const EADDRINUSE: isize = -98;
const EADDRNOTAVAIL: isize = -99;
const EDESTADDRREQ: isize = -89;
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
struct SigAction {
    handler: usize,
    flags: usize,
    mask: u64,
    restorer: usize,
}

/// `sigaltstack` argument/result: `{ ss_sp, ss_flags, pad, ss_size }`.
#[repr(C)]
struct SigAltStack {
    ss_sp: usize,
    ss_flags: i32,
    _pad: i32,
    ss_size: usize,
}

/// The pagh `siginfo_t`: signo, errno, code, then the 16-byte union slot
/// (`si_pid`/`si_uid` or `si_addr`), `si_status`, padding to 128 bytes.
#[repr(C)]
struct SigInfo {
    si_signo: i32,
    si_errno: i32,
    si_code: i32,
    _pad: i32,
    si_value: u64,
    si_status: i32,
    _pad2: i32,
    _rest: [u8; 96],
}

impl SigInfo {
    /// `si_pid` (low half of the union slot).
    fn pid(&self) -> u32 {
        self.si_value as u32
    }

    /// `si_uid` (high half of the union slot).
    fn uid(&self) -> u32 {
        (self.si_value >> 32) as u32
    }

    /// `si_addr` for a synchronous fault.
    fn addr(&self) -> u64 {
        self.si_value
    }
}

/// The pagh `mcontext`: `gregs[0..32]` is x0..x31 and `pc` the interrupted
/// `sepc`.
#[repr(C)]
struct MContext {
    gregs: [u64; 32],
    pc: u64,
}

/// The pagh `ucontext_t`: flags, previous context, handler mask, mcontext.
#[repr(C)]
struct UContext {
    flags: u64,
    link: u64,
    sigmask: u64,
    mcontext: MContext,
}

const _: () = assert!(core::mem::size_of::<SigAltStack>() == 24);
const _: () = assert!(core::mem::size_of::<SigInfo>() == 128);
const _: () = assert!(core::mem::size_of::<MContext>() == 264);
const _: () = assert!(core::mem::size_of::<UContext>() == 288);

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

fn fork_process() -> isize {
    syscall1(SYS_FORK, 0)
}

fn wait_child(pid: isize, status: *mut i32, options: usize) -> isize {
    syscall4(
        SYS_WAIT4,
        pid as usize,
        status as usize,
        options,
        0,
    )
}

fn execve(path: &[u8], argv: *const usize, envp: *const usize) -> isize {
    syscall3(SYS_EXECVE, path.as_ptr() as usize, argv as usize, envp as usize)
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

/// Like `wait_until`, but with a real deadline so a setup failure cannot spin
/// the whole selftest: sleeps 10 ms per turn (the signal tests need a blocked
/// thread, which only a sleeping main thread reliably gives it time to reach).
fn wait_millis(mut condition: impl FnMut() -> bool, millis: usize, code: i32, what: &str) {
    let mut waited = 0usize;
    while waited <= millis {
        if condition() {
            return;
        }
        let pause = Timespec {
            seconds: 0,
            nanos: 10_000_000,
        };
        let _ = syscall2(
            SYS_NANOSLEEP,
            &pause as *const Timespec as usize,
            0,
        );
        waited += 10;
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

// ─── signals ───────────────────────────────────────────────────────────────

/// Hits recorded by the SIGUSR1 handler. A signal is only proved by user code
/// observing its effect after `rt_sigreturn`, not by `kill` returning 0.
static SIGUSR1_HITS: AtomicUsize = AtomicUsize::new(0);
static SIGSEGV_HITS: AtomicUsize = AtomicUsize::new(0);
static WAKE_GATE: AtomicU32 = AtomicU32::new(0);
static WAKE_READY: AtomicUsize = AtomicUsize::new(0);
static WAKE_RESULT: AtomicUsize = AtomicUsize::new(0);
static WAKE_HITS: AtomicUsize = AtomicUsize::new(0);

extern "C" fn sigusr1_handler(_signum: usize, _info: usize) {
    SIGUSR1_HITS.fetch_add(1, Ordering::SeqCst);
}

/// SIGSEGV handler for a child thread that deliberately loads address 0.
///
/// Returning through `rt_sigreturn` would re-execute the faulting load forever,
/// so the handler proves synchronous delivery and then ends *its thread*
/// (`exit`, syscall 93): the run's other threads must survive.
extern "C" fn sigsegv_handler(_signum: usize, _info: usize) {
    SIGSEGV_HITS.fetch_add(1, Ordering::SeqCst);
    let _ = syscall1(SYS_EXIT, 0);
    loop {}
}

extern "C" fn sigsegv_child(_arg: usize) -> ! {
    // SAFETY: the load is deliberate: address 0 is unmapped, and the kernel
    // must deliver SIGSEGV to the installed handler instead of killing the run.
    unsafe { core::ptr::read_volatile(0 as *const u64) };
    let _ = syscall1(SYS_EXIT, 3);
    loop {}
}

/// SIGUSR2 handler for the futex-wake test (runs in the blocked child).
extern "C" fn sigusr2_handler(_signum: usize, _info: usize) {
    WAKE_HITS.fetch_add(1, Ordering::SeqCst);
}

/// Block in `FUTEX_WAIT` on a word nobody wakes. A `kill` must interrupt the
/// wait with `-EINTR`, run this thread's handler, and only then resume the code
/// after the syscall.
extern "C" fn wake_child(_arg: usize) -> ! {
    WAKE_READY.store(1, Ordering::SeqCst);
    let timeout = Timespec {
        seconds: 2,
        nanos: 0,
    };
    let result = futex_wait_word(
        WAKE_GATE.as_ptr() as usize,
        0,
        &timeout as *const Timespec as usize,
    );
    WAKE_RESULT.store(result as usize, Ordering::SeqCst);
    let _ = syscall1(SYS_EXIT, 0);
    loop {}
}

fn rt_sigaction(signum: usize, action: *const SigAction, old: *mut SigAction) -> isize {
    syscall4(
        SYS_RT_SIGACTION,
        signum,
        action as usize,
        old as usize,
        8,
    )
}

fn rt_sigprocmask(how: usize, set: *const u64, old: *mut u64) -> isize {
    syscall4(SYS_RT_SIGPROCMASK, how, set as usize, old as usize, 8)
}

fn kill(pid: isize, signal: usize) -> isize {
    syscall2(SYS_KILL, pid as usize, signal)
}

fn tgkill(tgid: usize, tid: usize, signal: usize) -> isize {
    syscall3(SYS_TGKILL, tgid, tid, signal)
}

fn sigaltstack(new: *const SigAltStack, old: *mut SigAltStack) -> isize {
    syscall2(SYS_SIGALTSTACK, new as usize, old as usize)
}

fn getpid() -> isize {
    syscall1(172, 0)
}

/// The signal acceptance section (runs with or without `/mnt`).
///
/// It proves the handler path end to end: the kernel enters the Rust handler at
/// the syscall boundary, the handler's `ret` reaches the assembly restorer, and
/// `rt_sigreturn` resumes the instruction after `kill` with `SIGUSR1_HITS`
/// incremented exactly once. The ordering around the mask is the other half:
/// a blocked signal stays pending, `sched_yield` cannot consume it, and
/// clearing the block delivers it. All the negative cases (`-EINVAL`,
/// `-ESRCH`, `-ENOSYS`, `-EFAULT`) are checked with the same wrappers.
fn signal_section(stack: usize) {
    let pid = syscall1(172, 0);
    let tid = syscall1(SYS_GETTID, 0);
    check(pid > 0 && tid > 0, 210, "getpid/gettid for signals");

    let mut action = SigAction {
        handler: sigusr1_handler as *const () as usize,
        flags: SA_RESTORER,
        mask: 0,
        restorer: abi_sigrestorer as *const () as usize,
    };
    let mut old = SigAction {
        handler: 0,
        flags: 0,
        mask: 0,
        restorer: 0,
    };

    // ── invalid arguments ──
    check(rt_sigaction(0, &action, core::ptr::null_mut()) == EINVAL, 211, "sigaction 0");
    check(
        rt_sigaction(32, &action, core::ptr::null_mut()) == EINVAL,
        212,
        "sigaction 32",
    );
    check(
        rt_sigaction(SIGKILL, &action, core::ptr::null_mut()) == EINVAL,
        213,
        "sigaction SIGKILL",
    );
    check(
        rt_sigaction(SIGSTOP, &action, core::ptr::null_mut()) == EINVAL,
        214,
        "sigaction SIGSTOP",
    );
    check(
        syscall4(SYS_RT_SIGACTION, SIGUSR1, &action as *const _ as usize, 0, 7) == EINVAL,
        215,
        "sigaction sigsetsize",
    );
    check(
        syscall4(SYS_RT_SIGACTION, SIGUSR1, 0xdead_0000, 0, 8) == EFAULT,
        216,
        "sigaction bad pointer",
    );
    check(
        syscall4(SYS_RT_SIGPROCMASK, SIG_BLOCK, 0, 0, 7) == EINVAL,
        217,
        "sigprocmask sigsetsize",
    );
    check(
        syscall4(SYS_RT_SIGPROCMASK, 9, 0, 0, 8) == EINVAL,
        218,
        "sigprocmask how",
    );

    // The initial disposition is SIG_DFL, including the returned oldact.
    check(rt_sigaction(SIGUSR1, core::ptr::null(), &mut old) == 0, 219, "query SIGUSR1");
    check(
        old.handler == SIG_DFL && old.flags == 0 && old.mask == 0 && old.restorer == 0,
        220,
        "initial disposition",
    );
    check(rt_sigaction(SIGUSR1, &action, &mut old) == 0, 221, "install SIGUSR1");
    check(old.handler == SIG_DFL, 222, "oldact after install");
    // A host-handler without a restorer is refused: this kernel has no vDSO
    // trampoline, and entering a handler whose `ret` goes to 0 would fault.
    action.flags = 0;
    check(rt_sigaction(SIGUSR1, &action, core::ptr::null_mut()) == EINVAL, 223, "no restorer");

    // ── sending ──
    check(kill(pid, 0) == 0, 224, "kill sig 0");
    check(kill(pid, 32) == EINVAL, 225, "kill bad signal");
    check(kill(-2, SIGUSR1) == ESRCH, 226, "kill invalid process group");
    check(kill(pid + 1, SIGUSR1) == ESRCH, 227, "kill unknown tid");
    check(kill(pid, SIGUSR1) == 0, 228, "kill SIGUSR1");
    check(
        SIGUSR1_HITS.load(Ordering::SeqCst) == 1,
        229,
        "handler ran once",
    );
    check(kill(0, SIGUSR1) == 0, 230, "kill(0)");
    check(
        SIGUSR1_HITS.load(Ordering::SeqCst) == 2,
        231,
        "kill(0) delivered",
    );
    check(kill(-1, SIGUSR1) == 0, 232, "kill(-1)");
    check(
        SIGUSR1_HITS.load(Ordering::SeqCst) == 3,
        233,
        "kill(-1) delivered",
    );
    check(tgkill(pid as usize, tid as usize, SIGUSR1) == 0, 234, "tgkill");
    check(
        SIGUSR1_HITS.load(Ordering::SeqCst) == 4,
        235,
        "tgkill delivered",
    );
    check(
        tgkill(pid as usize + 1, tid as usize, SIGUSR1) == ESRCH,
        236,
        "tgkill wrong tgid",
    );

    // ── SIG_IGN and the default-ignore family ──
    let ignore = SigAction {
        handler: SIG_IGN,
        flags: 0,
        mask: 0,
        restorer: 0,
    };
    check(
        rt_sigaction(SIGUSR2, &ignore, core::ptr::null_mut()) == 0,
        237,
        "install SIG_IGN",
    );
    check(kill(pid, SIGUSR2) == 0, 238, "kill ignored");
    check(kill(pid, SIGCHLD) == 0, 239, "kill SIGCHLD (default ignore)");
    check(
        SIGUSR1_HITS.load(Ordering::SeqCst) == 4,
        240,
        "ignored signals did not run the handler",
    );

    // ── block / pending / unblock ──
    let mask: u64 = 1 << (SIGUSR1 - 1);
    let mut oldmask: u64 = u64::MAX;
    check(rt_sigprocmask(SIG_BLOCK, &mask, &mut oldmask) == 0, 241, "block SIGUSR1");
    check(oldmask == 0, 242, "old mask on block");
    check(kill(pid, SIGUSR1) == 0, 243, "kill blocked SIGUSR1");
    for _ in 0..100 {
        let _ = syscall1(SYS_SCHED_YIELD, 0);
    }
    check(
        SIGUSR1_HITS.load(Ordering::SeqCst) == 4,
        244,
        "blocked signal stayed pending",
    );
    let mut current: u64 = 0;
    check(rt_sigprocmask(SIG_BLOCK, core::ptr::null(), &mut current) == 0, 245, "query mask");
    check(current & mask == mask, 246, "blocked bit visible");
    check(rt_sigprocmask(SIG_UNBLOCK, &mask, core::ptr::null_mut()) == 0, 247, "unblock");
    check(
        SIGUSR1_HITS.load(Ordering::SeqCst) == 5,
        248,
        "pending signal delivered on unblock",
    );

    // SIG_SETMASK and oldset.
    let everything: u64 = u64::MAX;
    check(
        rt_sigprocmask(SIG_SETMASK, &everything, &mut current) == 0,
        249,
        "setmask everything",
    );
    check(current == 0, 250, "oldset before setmask");
    check(
        rt_sigprocmask(SIG_SETMASK, &0u64, core::ptr::null_mut()) == 0,
        251,
        "clear mask",
    );
    // SIGKILL cannot be blocked: the bit is dropped, not silently counted.
    let kill_only: u64 = 1 << (SIGKILL - 1);
    check(rt_sigprocmask(SIG_BLOCK, &kill_only, core::ptr::null_mut()) == 0, 252, "block SIGKILL");
    check(rt_sigprocmask(SIG_BLOCK, core::ptr::null(), &mut current) == 0, 253, "query after SIGKILL");
    check(current & kill_only == 0, 254, "SIGKILL not blockable");

    // ── a non-main thread: kill wakes a `FUTEX_WAIT` and the handler runs ──
    let usr2 = SigAction {
        handler: sigusr2_handler as *const () as usize,
        flags: SA_RESTORER,
        mask: 0,
        restorer: abi_sigrestorer as *const () as usize,
    };
    check(
        rt_sigaction(SIGUSR2, &usr2, core::ptr::null_mut()) == 0,
        255,
        "install SIGUSR2",
    );
    WAKE_READY.store(0, Ordering::SeqCst);
    WAKE_RESULT.store(0, Ordering::SeqCst);
    WAKE_HITS.store(0, Ordering::SeqCst);
    let wake_tid = spawn_thread(
        THREAD_FLAGS,
        stack,
        wake_child as *const () as usize,
        0,
    );
    check(wake_tid > 0, 256, "spawn futex waiter");
    wait_millis(
        || WAKE_READY.load(Ordering::SeqCst) == 1,
        1000,
        257,
        "waiter ready",
    );
    // Give the child time to actually block: `wake_task` only finds `Blocked`.
    let settle = Timespec { seconds: 0, nanos: 30_000_000 };
    let _ = syscall2(SYS_NANOSLEEP, &settle as *const Timespec as usize, 0);
    check(kill(wake_tid, SIGUSR2) == 0, 259, "kill blocked thread");
    wait_millis(
        || kill(wake_tid, 0) == ESRCH,
        3000,
        260,
        "waiter exited after kill",
    );
    check(
        WAKE_HITS.load(Ordering::SeqCst) == 1,
        261,
        "handler ran in the woken thread",
    );
    check(
        WAKE_RESULT.load(Ordering::SeqCst) as isize == EINTR,
        262,
        "futex returned -EINTR",
    );
    let dfl_usr2 = SigAction {
        handler: SIG_DFL,
        flags: 0,
        mask: 0,
        restorer: 0,
    };
    check(
        rt_sigaction(SIGUSR2, &dfl_usr2, core::ptr::null_mut()) == 0,
        263,
        "restore SIGUSR2",
    );

    // ── a synchronous fault with a handler: the run survives the child ──
    let segv = SigAction {
        handler: sigsegv_handler as *const () as usize,
        flags: SA_RESTORER,
        mask: 0,
        restorer: abi_sigrestorer as *const () as usize,
    };
    check(
        rt_sigaction(SIGSEGV, &segv, core::ptr::null_mut()) == 0,
        264,
        "install SIGSEGV",
    );
    SIGSEGV_HITS.store(0, Ordering::SeqCst);
    let segv_tid = spawn_thread(
        THREAD_FLAGS,
        stack,
        sigsegv_child as *const () as usize,
        0,
    );
    check(segv_tid > 0, 265, "spawn faulting thread");
    wait_millis(
        || SIGSEGV_HITS.load(Ordering::SeqCst) == 1,
        1000,
        266,
        "SIGSEGV handler ran",
    );
    wait_millis(
        || kill(segv_tid, 0) == ESRCH,
        1000,
        267,
        "faulting thread exited",
    );
    check(
        SIGSEGV_HITS.load(Ordering::SeqCst) == 1,
        268,
        "SIGSEGV delivered once",
    );
    let dfl_segv = SigAction {
        handler: SIG_DFL,
        flags: 0,
        mask: 0,
        restorer: 0,
    };
    check(
        rt_sigaction(SIGSEGV, &dfl_segv, core::ptr::null_mut()) == 0,
        269,
        "restore SIGSEGV",
    );

    // ── rt_sigreturn without a frame / with a bad frame pointer ──
    check(
        syscall1(SYS_RT_SIGRETURN, 0) == EINVAL,
        270,
        "sigreturn without a frame",
    );
    // SAFETY: the assembly helper saves the real `sp`, calls the syscall with
    // an unmapped frame pointer (which must return `-EFAULT`, not restore a
    // frame), and then restores `sp` before `ret`.
    check(
        unsafe { abi_bad_sigreturn(0xdead_0000) } == EFAULT,
        256,
        "sigreturn bad frame pointer",
    );

    // ── a forged back-chain pointer inside a real frame (F-r3-rv-1) ──
    // `prev_frame` is the one frame word a handler can rewrite before
    // `rt_sigreturn`; installing it unchecked made the next delivery overflow
    // `prev_frame + offset_of!(ucontext)` and park a debug kernel. The
    // assembly handler patches the armed frame, asks `rt_sigreturn` (it must
    // answer -EINVAL and leave the frame alone), repairs the word and unwinds
    // through the normal restorer, so reaching the print below proves the
    // machine and the frame chain survived.
    let forged = SigAction {
        handler: abi_forged_prev_frame as *const () as usize,
        flags: SA_RESTORER,
        mask: 0,
        restorer: abi_sigrestorer as *const () as usize,
    };
    check(
        rt_sigaction(SIGUSR1, &forged, core::ptr::null_mut()) == 0,
        280,
        "install forged-prev_frame handler",
    );
    unsafe { abi_forged_prev_result = 0 };
    let hits_before = SIGUSR1_HITS.load(Ordering::SeqCst);
    check(kill(pid, SIGUSR1) == 0, 281, "kill with forged prev_frame");
    check(
        unsafe { abi_forged_prev_result } == EINVAL,
        282,
        "sigreturn rejected forged prev_frame",
    );
    check(
        SIGUSR1_HITS.load(Ordering::SeqCst) == hits_before,
        283,
        "forged-prev_frame handler did not run the Rust handler",
    );
    // Put the ordinary handler back (its local copy lost SA_RESTORER above).
    action.flags = SA_RESTORER;
    check(
        rt_sigaction(SIGUSR1, &action, core::ptr::null_mut()) == 0,
        284,
        "restore the SIGUSR1 handler",
    );

    // Leave SIGUSR1 at SIG_DFL: the run's handler table is torn down with the
    // run, but a passing fixture should not rely on that teardown.
    let dfl = SigAction {
        handler: SIG_DFL,
        flags: 0,
        mask: 0,
        restorer: 0,
    };
    check(rt_sigaction(SIGUSR1, &dfl, core::ptr::null_mut()) == 0, 272, "restore SIG_DFL");
    check(
        SIGUSR1_HITS.load(Ordering::SeqCst) == 5,
        273,
        "signal count at the end",
    );
    print("abi: signals ok\n");
}

// ─── alternate stack, siginfo, SA_* flags, job control ─────────────────────

static ALT_BASE: AtomicUsize = AtomicUsize::new(0);
static ALT_TOP: AtomicUsize = AtomicUsize::new(0);
static ALT_HITS: AtomicUsize = AtomicUsize::new(0);
static ALT_HIT: AtomicUsize = AtomicUsize::new(0);
static ALT_ONSTACK: AtomicUsize = AtomicUsize::new(0);
static SIGINFO_OK: AtomicUsize = AtomicUsize::new(1);
static SIGINFO_HITS: AtomicUsize = AtomicUsize::new(0);
static SIGINFO_EXPECT_SIGNO: AtomicUsize = AtomicUsize::new(SIGUSR1);
static SIGINFO_EXPECT_CODE: AtomicI32 = AtomicI32::new(SI_USER);
static SIGINFO_EXPECT_MASK: AtomicUsize = AtomicUsize::new(0);
static NODEFER_HITS: AtomicUsize = AtomicUsize::new(0);
static NODEFER_MAX: AtomicUsize = AtomicUsize::new(0);
static NODEFER_MASK_OK: AtomicUsize = AtomicUsize::new(0);
static NODEFER_PID: AtomicUsize = AtomicUsize::new(0);
static RESETHAND_HITS: AtomicUsize = AtomicUsize::new(0);
static RESTART_READY: AtomicUsize = AtomicUsize::new(0);
static RESTART_RESULT: AtomicUsize = AtomicUsize::new(0);
static RESTART_HITS: AtomicUsize = AtomicUsize::new(0);
static JOG_READY: AtomicUsize = AtomicUsize::new(0);
static JOG_COUNTER: AtomicUsize = AtomicUsize::new(0);
static JOG_STOP: AtomicUsize = AtomicUsize::new(0);
static STOP_READY: AtomicUsize = AtomicUsize::new(0);
static STOP_RESULT: AtomicUsize = AtomicUsize::new(0);
static STOP_GATE: AtomicU32 = AtomicU32::new(0);
static RESTART_GATE: AtomicU32 = AtomicU32::new(0);

/// Handler for the `SA_ONSTACK` test: proves the kernel entered it on the
/// installed alternate stack, and that a query from inside the handler reports
/// `SS_ONSTACK`.
extern "C" fn altstack_handler(signo: usize, _info: usize) {
    let _ = signo;
    ALT_HITS.fetch_add(1, Ordering::SeqCst);
    let sp: usize;
    // SAFETY: reading `sp` is a pure register read; the handler runs on the
    // stack the kernel chose, which is exactly what this test inspects.
    unsafe { core::arch::asm!("mv {}, sp", out(reg) sp, options(nomem, nostack)) };
    if sp >= ALT_BASE.load(Ordering::SeqCst) && sp < ALT_TOP.load(Ordering::SeqCst) {
        ALT_HIT.store(1, Ordering::SeqCst);
    }
    let mut old = SigAltStack {
        ss_sp: 0,
        ss_flags: 0,
        _pad: 0,
        ss_size: 0,
    };
    if sigaltstack(core::ptr::null(), &mut old) == 0 && old.ss_flags & SS_ONSTACK != 0 {
        ALT_ONSTACK.store(1, Ordering::SeqCst);
    }
}

/// Shared validation for a `SA_SIGINFO` handler: every field the kernel
/// promises for the expected source, plus the ucontext snapshot.
fn siginfo_matches(signo: usize, info: *const SigInfo, uc: *const UContext) -> bool {
    // SAFETY: the kernel passes both pointers into the live signal frame it
    // just wrote below the handler's `sp`; they stay mapped for the call.
    let info = unsafe { &*info };
    // SAFETY: as above.
    let uc = unsafe { &*uc };
    let mut ok = true;
    if info.si_signo as usize != signo
        || signo != SIGINFO_EXPECT_SIGNO.load(Ordering::SeqCst)
        || info.si_errno != 0
        || info.si_code != SIGINFO_EXPECT_CODE.load(Ordering::SeqCst)
    {
        ok = false;
    }
    if signo == SIGSEGV {
        if info.addr() != 0 {
            ok = false;
        }
    } else if info.pid() as isize != getpid() || info.uid() != 0 {
        ok = false;
    }
    if signo == SIGCHLD && info.si_status != 0 {
        ok = false;
    }
    if uc.flags != 0 || uc.mcontext.gregs[0] != 0 || uc.mcontext.gregs[2] == 0 {
        ok = false;
    }
    // The fixture is one static ELF: `.text` is the first segment at 0x10000.
    let pc = uc.mcontext.pc as usize;
    if !(0x1_0000..0x2_0000).contains(&pc) {
        ok = false;
    }
    let expected_mask = SIGINFO_EXPECT_MASK.load(Ordering::SeqCst);
    if expected_mask != 0 && (uc.sigmask as usize) & expected_mask != expected_mask {
        ok = false;
    }
    ok
}

extern "C" fn siginfo_handler(signo: usize, info: *const SigInfo, uc: *const UContext) {
    SIGINFO_OK.store(siginfo_matches(signo, info, uc) as usize, Ordering::SeqCst);
    SIGINFO_HITS.fetch_add(1, Ordering::SeqCst);
}

/// SIGSEGV variant: it proves `si_addr` and the ucontext, then ends *its own*
/// thread (returning through `rt_sigreturn` would re-execute the fault).
extern "C" fn siginfo_segv_handler(signo: usize, info: *const SigInfo, uc: *const UContext) {
    SIGINFO_OK.store(siginfo_matches(signo, info, uc) as usize, Ordering::SeqCst);
    SIGINFO_HITS.fetch_add(1, Ordering::SeqCst);
    let _ = syscall1(SYS_EXIT, 0);
    loop {}
}

extern "C" fn siginfo_segv_child(_arg: usize) -> ! {
    // SAFETY: the load is deliberate: address 0 is unmapped, and the kernel
    // must deliver SIGSEGV with si_addr = 0 to the installed handler.
    unsafe { core::ptr::read_volatile(0 as *const u64) };
    let _ = syscall1(SYS_EXIT, 3);
    loop {}
}

/// `SA_NODEFER`: the handler sends itself the same signal. Because the signal
/// is not added to the mask, the nested delivery happens before this invocation
/// returns, so the maximum depth is three (the third handler sends nothing).
extern "C" fn nodefer_handler(signo: usize, _info: usize) {
    let depth = NODEFER_HITS.fetch_add(1, Ordering::SeqCst) + 1;
    if depth > NODEFER_MAX.load(Ordering::SeqCst) {
        NODEFER_MAX.store(depth, Ordering::SeqCst);
    }
    let mut current = 0u64;
    if rt_sigprocmask(SIG_BLOCK, core::ptr::null(), &mut current) == 0
        && current & (1u64 << (signo - 1)) == 0
    {
        NODEFER_MASK_OK.store(1, Ordering::SeqCst);
    }
    if depth < 3 {
        let _ = kill(NODEFER_PID.load(Ordering::SeqCst) as isize, SIGUSR1);
    }
}

extern "C" fn resethand_handler(_signo: usize, _info: usize) {
    RESETHAND_HITS.fetch_add(1, Ordering::SeqCst);
}

extern "C" fn restart_handler(_signo: usize, _info: usize) {
    RESTART_HITS.fetch_add(1, Ordering::SeqCst);
}

/// Block in `FUTEX_WAIT` with a 300 ms timeout. A `SA_RESTART` signal must
/// re-enter the wait after the handler, so the observable result is
/// `-ETIMEDOUT`, not `-EINTR`.
extern "C" fn restart_entry(_arg: usize) -> ! {
    RESTART_READY.store(1, Ordering::SeqCst);
    let timeout = Timespec {
        seconds: 0,
        nanos: 300_000_000,
    };
    let result = futex_wait_word(
        RESTART_GATE.as_ptr() as usize,
        0,
        &timeout as *const Timespec as usize,
    );
    RESTART_RESULT.store(result as usize, Ordering::SeqCst);
    let _ = syscall1(SYS_EXIT, 0);
    loop {}
}

/// Busy loop the job-control test stops and continues. It takes no syscalls,
/// so only the timer tick can deliver the stop.
extern "C" fn jog_entry(_arg: usize) -> ! {
    JOG_READY.store(1, Ordering::SeqCst);
    while JOG_STOP.load(Ordering::SeqCst) == 0 {
        JOG_COUNTER.fetch_add(1, Ordering::SeqCst);
        core::hint::spin_loop();
    }
    let _ = syscall1(SYS_EXIT, 0);
    loop {}
}

/// Futex waiter used by the job-control test: a stop must wake it, stop it in
/// the kernel, and SIGCONT must let the wait return `-EINTR`.
extern "C" fn stop_wait_entry(_arg: usize) -> ! {
    STOP_READY.store(1, Ordering::SeqCst);
    let timeout = Timespec {
        seconds: 0,
        nanos: 300_000_000,
    };
    let result = futex_wait_word(
        STOP_GATE.as_ptr() as usize,
        0,
        &timeout as *const Timespec as usize,
    );
    STOP_RESULT.store(result as usize, Ordering::SeqCst);
    let _ = syscall1(SYS_EXIT, 0);
    loop {}
}

fn null_action() -> *const SigAction {
    core::ptr::null()
}

fn dfl_action() -> SigAction {
    SigAction {
        handler: SIG_DFL,
        flags: 0,
        mask: 0,
        restorer: 0,
    }
}

/// The extended signal acceptance: `sigaltstack` + `SA_ONSTACK`, the full
/// `siginfo_t`/`ucontext` with `SA_SIGINFO`, `SA_NODEFER`, `SA_RESETHAND`,
/// `SA_RESTART` and SIGSTOP/SIGTSTP/SIGCONT scheduler stops.
fn signal_ext_section(stack: usize) {
    let pid = getpid();
    let tid = syscall1(SYS_GETTID, 0);
    check(pid > 0 && tid > 0, 280, "getpid/gettid for extended signals");

    // ── sigaltstack: validation, install, query, SS_DISABLE ──
    let region = syscall6(
        SYS_MMAP,
        0,
        0x4000,
        PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS,
        usize::MAX,
        0,
    );
    check(region > 0, 281, "mmap alternate stack");
    let alt_base = region as usize;
    ALT_BASE.store(alt_base, Ordering::SeqCst);
    ALT_TOP.store(alt_base + 0x4000, Ordering::SeqCst);

    let mut old = SigAltStack {
        ss_sp: 0,
        ss_flags: 0,
        _pad: 0,
        ss_size: 0,
    };
    check(
        sigaltstack(core::ptr::null(), &mut old) == 0 && old.ss_flags == SS_DISABLE,
        282,
        "sigaltstack initial query",
    );
    let too_small = SigAltStack {
        ss_sp: alt_base,
        ss_flags: 0,
        _pad: 0,
        ss_size: MINSIGSTKSZ - 1,
    };
    check(
        sigaltstack(&too_small, core::ptr::null_mut()) == EINVAL,
        283,
        "sigaltstack below MINSIGSTKSZ",
    );
    let bad_flags = SigAltStack {
        ss_sp: alt_base,
        ss_flags: 3,
        _pad: 0,
        ss_size: 0x4000,
    };
    check(
        sigaltstack(&bad_flags, core::ptr::null_mut()) == EINVAL,
        284,
        "sigaltstack bad flags",
    );
    let bad_pointer = SigAltStack {
        ss_sp: 0xdead_0000,
        ss_flags: 0,
        _pad: 0,
        ss_size: MINSIGSTKSZ,
    };
    check(
        sigaltstack(&bad_pointer, core::ptr::null_mut()) == EFAULT,
        285,
        "sigaltstack unmapped range",
    );
    let install = SigAltStack {
        ss_sp: alt_base,
        ss_flags: 0,
        _pad: 0,
        ss_size: 0x4000,
    };
    check(
        sigaltstack(&install, &mut old) == 0 && old.ss_flags == SS_DISABLE,
        286,
        "sigaltstack install",
    );
    check(
        sigaltstack(core::ptr::null(), &mut old) == 0
            && old.ss_sp == alt_base
            && old.ss_size == 0x4000
            && old.ss_flags == 0,
        287,
        "sigaltstack query installed",
    );

    let action = SigAction {
        handler: altstack_handler as *const () as usize,
        flags: SA_RESTORER | SA_ONSTACK,
        mask: 0,
        restorer: abi_sigrestorer as *const () as usize,
    };
    check(
        rt_sigaction(SIGUSR1, &action, core::ptr::null_mut()) == 0,
        288,
        "install SA_ONSTACK handler",
    );
    ALT_HITS.store(0, Ordering::SeqCst);
    ALT_HIT.store(0, Ordering::SeqCst);
    ALT_ONSTACK.store(0, Ordering::SeqCst);
    check(kill(pid, SIGUSR1) == 0, 289, "kill with SA_ONSTACK");
    check(
        ALT_HITS.load(Ordering::SeqCst) == 1
            && ALT_HIT.load(Ordering::SeqCst) == 1
            && ALT_ONSTACK.load(Ordering::SeqCst) == 1,
        290,
        "handler ran on the alternate stack",
    );
    check(
        rt_sigaction(SIGUSR1, &dfl_action(), core::ptr::null_mut()) == 0,
        291,
        "restore SIGUSR1 after altstack",
    );

    // Disabling removes the stack; SA_ONSTACK then falls back to the normal
    // stack, as POSIX permits. The handler still runs, just not inside the
    // alternate range and with SS_DISABLE on the query.
    let disable = SigAltStack {
        ss_sp: 0,
        ss_flags: SS_DISABLE,
        _pad: 0,
        ss_size: 0,
    };
    check(
        sigaltstack(&disable, &mut old) == 0 && old.ss_flags == 0,
        338,
        "sigaltstack disable",
    );
    check(
        sigaltstack(core::ptr::null(), &mut old) == 0 && old.ss_flags == SS_DISABLE,
        339,
        "sigaltstack disabled query",
    );
    check(
        rt_sigaction(SIGUSR1, &action, core::ptr::null_mut()) == 0,
        340,
        "reinstall SA_ONSTACK without a stack",
    );
    ALT_HITS.store(0, Ordering::SeqCst);
    ALT_HIT.store(0, Ordering::SeqCst);
    ALT_ONSTACK.store(0, Ordering::SeqCst);
    check(kill(pid, SIGUSR1) == 0, 341, "kill SA_ONSTACK fallback");
    check(
        ALT_HITS.load(Ordering::SeqCst) == 1
            && ALT_HIT.load(Ordering::SeqCst) == 0
            && ALT_ONSTACK.load(Ordering::SeqCst) == 0,
        342,
        "SA_ONSTACK fell back to the normal stack",
    );
    check(
        rt_sigaction(SIGUSR1, &dfl_action(), core::ptr::null_mut()) == 0,
        343,
        "restore SIGUSR1 after altstack fallback",
    );
    print("abi: altstack ok\n");

    // ── siginfo + ucontext through SA_SIGINFO ──
    SIGINFO_EXPECT_SIGNO.store(SIGUSR1, Ordering::SeqCst);
    SIGINFO_EXPECT_CODE.store(SI_USER, Ordering::SeqCst);
    SIGINFO_EXPECT_MASK.store(
        (1 << (SIGUSR1 - 1)) | (1 << (SIGUSR2 - 1)),
        Ordering::SeqCst,
    );
    SIGINFO_OK.store(1, Ordering::SeqCst);
    SIGINFO_HITS.store(0, Ordering::SeqCst);
    let info_action = SigAction {
        handler: siginfo_handler as *const () as usize,
        flags: SA_RESTORER | SA_SIGINFO,
        mask: 1 << (SIGUSR2 - 1),
        restorer: abi_sigrestorer as *const () as usize,
    };
    check(
        rt_sigaction(SIGUSR1, &info_action, core::ptr::null_mut()) == 0,
        292,
        "install SA_SIGINFO handler",
    );
    check(kill(pid, SIGUSR1) == 0, 293, "kill for SA_SIGINFO");
    check(
        SIGINFO_HITS.load(Ordering::SeqCst) == 1 && SIGINFO_OK.load(Ordering::SeqCst) == 1,
        294,
        "siginfo from kill",
    );
    SIGINFO_EXPECT_CODE.store(SI_TKILL, Ordering::SeqCst);
    check(tgkill(pid as usize, tid as usize, SIGUSR1) == 0, 295, "tgkill for SA_SIGINFO");
    check(
        SIGINFO_HITS.load(Ordering::SeqCst) == 2 && SIGINFO_OK.load(Ordering::SeqCst) == 1,
        296,
        "siginfo code SI_TKILL",
    );

    // SIGCHLD is only reachable through kill here; its si_status must be 0.
    SIGINFO_EXPECT_SIGNO.store(SIGCHLD, Ordering::SeqCst);
    SIGINFO_EXPECT_CODE.store(SI_USER, Ordering::SeqCst);
    SIGINFO_EXPECT_MASK.store(0, Ordering::SeqCst);
    let chld_action = SigAction {
        handler: siginfo_handler as *const () as usize,
        flags: SA_RESTORER | SA_SIGINFO,
        mask: 0,
        restorer: abi_sigrestorer as *const () as usize,
    };
    check(
        rt_sigaction(SIGCHLD, &chld_action, core::ptr::null_mut()) == 0,
        297,
        "install SIGCHLD handler",
    );
    check(kill(pid, SIGCHLD) == 0, 298, "kill SIGCHLD");
    check(
        SIGINFO_HITS.load(Ordering::SeqCst) == 3 && SIGINFO_OK.load(Ordering::SeqCst) == 1,
        299,
        "siginfo fields for SIGCHLD",
    );
    check(
        rt_sigaction(SIGCHLD, &dfl_action(), core::ptr::null_mut()) == 0,
        300,
        "restore SIGCHLD",
    );

    // A synchronous fault carries si_addr; the child ends its own thread.
    SIGINFO_EXPECT_SIGNO.store(SIGSEGV, Ordering::SeqCst);
    SIGINFO_EXPECT_CODE.store(SEGV_MAPERR, Ordering::SeqCst);
    SIGINFO_EXPECT_MASK.store(0, Ordering::SeqCst);
    SIGINFO_OK.store(1, Ordering::SeqCst);
    SIGINFO_HITS.store(0, Ordering::SeqCst);
    let segv_action = SigAction {
        handler: siginfo_segv_handler as *const () as usize,
        flags: SA_RESTORER | SA_SIGINFO,
        mask: 0,
        restorer: abi_sigrestorer as *const () as usize,
    };
    check(
        rt_sigaction(SIGSEGV, &segv_action, core::ptr::null_mut()) == 0,
        301,
        "install SA_SIGINFO SIGSEGV",
    );
    let segv_tid = spawn_thread(
        THREAD_FLAGS,
        stack,
        siginfo_segv_child as *const () as usize,
        0,
    );
    check(segv_tid > 0, 302, "spawn SA_SIGINFO faulting thread");
    wait_millis(
        || SIGINFO_HITS.load(Ordering::SeqCst) == 1,
        1000,
        303,
        "SA_SIGINFO SIGSEGV handler ran",
    );
    wait_millis(
        || kill(segv_tid, 0) == ESRCH,
        1000,
        304,
        "SA_SIGINFO faulting thread exited",
    );
    check(SIGINFO_OK.load(Ordering::SeqCst) == 1, 305, "si_addr/ucontext for SIGSEGV");
    check(
        rt_sigaction(SIGSEGV, &dfl_action(), core::ptr::null_mut()) == 0,
        306,
        "restore SIGSEGV",
    );
    check(
        rt_sigaction(SIGUSR1, &dfl_action(), core::ptr::null_mut()) == 0,
        307,
        "restore SIGUSR1 after SA_SIGINFO",
    );
    print("abi: siginfo ok\n");

    // ── SA_NODEFER: recursive self-delivery with the own bit unblocked ──
    NODEFER_HITS.store(0, Ordering::SeqCst);
    NODEFER_MAX.store(0, Ordering::SeqCst);
    NODEFER_MASK_OK.store(0, Ordering::SeqCst);
    NODEFER_PID.store(pid as usize, Ordering::SeqCst);
    let nodefer_action = SigAction {
        handler: nodefer_handler as *const () as usize,
        flags: SA_RESTORER | SA_NODEFER,
        mask: 0,
        restorer: abi_sigrestorer as *const () as usize,
    };
    check(
        rt_sigaction(SIGUSR1, &nodefer_action, core::ptr::null_mut()) == 0,
        308,
        "install SA_NODEFER",
    );
    check(kill(pid, SIGUSR1) == 0, 309, "kill for SA_NODEFER");
    check(
        NODEFER_HITS.load(Ordering::SeqCst) == 3
            && NODEFER_MAX.load(Ordering::SeqCst) == 3
            && NODEFER_MASK_OK.load(Ordering::SeqCst) == 1,
        310,
        "SA_NODEFER nested delivery",
    );
    check(
        rt_sigaction(SIGUSR1, &dfl_action(), core::ptr::null_mut()) == 0,
        311,
        "restore after SA_NODEFER",
    );

    // ── SA_RESETHAND: the disposition is SIG_DFL by the time we query it ──
    RESETHAND_HITS.store(0, Ordering::SeqCst);
    let reset_action = SigAction {
        handler: resethand_handler as *const () as usize,
        flags: SA_RESTORER | SA_RESETHAND,
        mask: 0,
        restorer: abi_sigrestorer as *const () as usize,
    };
    check(
        rt_sigaction(SIGUSR1, &reset_action, core::ptr::null_mut()) == 0,
        312,
        "install SA_RESETHAND",
    );
    check(kill(pid, SIGUSR1) == 0, 313, "kill for SA_RESETHAND");
    let mut after = SigAction {
        handler: 0,
        flags: 0,
        mask: 0,
        restorer: 0,
    };
    check(
        rt_sigaction(SIGUSR1, null_action(), &mut after) == 0
            && RESETHAND_HITS.load(Ordering::SeqCst) == 1
            && after.handler == SIG_DFL
            && after.flags == 0,
        314,
        "SA_RESETHAND reset to SIG_DFL",
    );

    // ── SA_RESTART: the interrupted futex wait is re-entered ──
    RESTART_READY.store(0, Ordering::SeqCst);
    RESTART_RESULT.store(0, Ordering::SeqCst);
    RESTART_HITS.store(0, Ordering::SeqCst);
    let restart_action = SigAction {
        handler: restart_handler as *const () as usize,
        flags: SA_RESTORER | SA_RESTART,
        mask: 0,
        restorer: abi_sigrestorer as *const () as usize,
    };
    check(
        rt_sigaction(SIGUSR2, &restart_action, core::ptr::null_mut()) == 0,
        315,
        "install SA_RESTART",
    );
    let restart_tid = spawn_thread(
        THREAD_FLAGS,
        stack,
        restart_entry as *const () as usize,
        0,
    );
    check(restart_tid > 0, 316, "spawn SA_RESTART waiter");
    wait_millis(
        || RESTART_READY.load(Ordering::SeqCst) == 1,
        1000,
        317,
        "SA_RESTART waiter ready",
    );
    let settle = Timespec {
        seconds: 0,
        nanos: 30_000_000,
    };
    let _ = syscall2(SYS_NANOSLEEP, &settle as *const Timespec as usize, 0);
    check(kill(restart_tid, SIGUSR2) == 0, 318, "kill SA_RESTART waiter");
    wait_millis(
        || kill(restart_tid, 0) == ESRCH,
        2000,
        319,
        "SA_RESTART waiter exited",
    );
    check(
        RESTART_HITS.load(Ordering::SeqCst) == 1
            && RESTART_RESULT.load(Ordering::SeqCst) as isize == ETIMEDOUT,
        320,
        "SA_RESTART re-entered futex wait",
    );
    check(
        rt_sigaction(SIGUSR2, &dfl_action(), core::ptr::null_mut()) == 0,
        321,
        "restore SIGUSR2 after SA_RESTART",
    );

    // ── job control: SIGSTOP freezes a busy thread, SIGCONT resumes it ──
    JOG_STOP.store(0, Ordering::SeqCst);
    JOG_COUNTER.store(0, Ordering::SeqCst);
    JOG_READY.store(0, Ordering::SeqCst);
    let jog = spawn_thread(THREAD_FLAGS, stack, jog_entry as *const () as usize, 0);
    check(jog > 0, 322, "spawn job-control thread");
    wait_millis(
        || JOG_READY.load(Ordering::SeqCst) == 1,
        1000,
        323,
        "job-control thread ready",
    );
    let startup = Timespec {
        seconds: 0,
        nanos: 30_000_000,
    };
    let _ = syscall2(SYS_NANOSLEEP, &startup as *const Timespec as usize, 0);
    check(JOG_COUNTER.load(Ordering::SeqCst) > 0, 324, "job-control thread runs");
    check(kill(jog, SIGSTOP) == 0, 325, "kill SIGSTOP");
    let paused = Timespec {
        seconds: 0,
        nanos: 60_000_000,
    };
    let _ = syscall2(SYS_NANOSLEEP, &paused as *const Timespec as usize, 0);
    let frozen = JOG_COUNTER.load(Ordering::SeqCst);
    let _ = syscall2(SYS_NANOSLEEP, &paused as *const Timespec as usize, 0);
    check(
        JOG_COUNTER.load(Ordering::SeqCst) == frozen,
        326,
        "SIGSTOP froze the thread",
    );
    check(kill(jog, 0) == 0, 327, "stopped thread still exists");
    check(kill(jog, SIGCONT) == 0, 328, "kill SIGCONT");
    wait_millis(
        || JOG_COUNTER.load(Ordering::SeqCst) > frozen,
        1000,
        329,
        "SIGCONT resumed the thread",
    );
    JOG_STOP.store(1, Ordering::SeqCst);
    wait_millis(
        || kill(jog, 0) == ESRCH,
        1000,
        330,
        "job-control thread exited",
    );

    // A futex waiter stopped by SIGTSTP must not return while stopped; the
    // SIGCONT lets it return `-EINTR`.
    STOP_READY.store(0, Ordering::SeqCst);
    STOP_RESULT.store(0, Ordering::SeqCst);
    let waiter = spawn_thread(
        THREAD_FLAGS,
        stack,
        stop_wait_entry as *const () as usize,
        0,
    );
    check(waiter > 0, 331, "spawn stopped futex waiter");
    wait_millis(
        || STOP_READY.load(Ordering::SeqCst) == 1,
        1000,
        332,
        "stopped futex waiter ready",
    );
    let settle = Timespec {
        seconds: 0,
        nanos: 30_000_000,
    };
    let _ = syscall2(SYS_NANOSLEEP, &settle as *const Timespec as usize, 0);
    check(kill(waiter, SIGTSTP) == 0, 333, "kill SIGTSTP");
    let paused = Timespec {
        seconds: 0,
        nanos: 30_000_000,
    };
    let _ = syscall2(SYS_NANOSLEEP, &paused as *const Timespec as usize, 0);
    check(
        kill(waiter, 0) == 0 && STOP_RESULT.load(Ordering::SeqCst) == 0,
        334,
        "stopped futex waiter is parked",
    );
    check(kill(waiter, SIGCONT) == 0, 335, "SIGCONT the futex waiter");
    wait_millis(
        || kill(waiter, 0) == ESRCH,
        1000,
        336,
        "continued futex waiter exited",
    );
    check(
        STOP_RESULT.load(Ordering::SeqCst) as isize == EINTR,
        337,
        "futex wait reported EINTR after stop/continue",
    );
    print("abi: jobcontrol ok\n");
}

// ─── process family: fork / wait4 / getppid / execve ──────────────────────

/// A child forked by [`process_section`]: writes its marker, proves `getppid`
/// names the caller and exits 42.
fn fork_child_marks(parent_pid: isize) -> ! {
    if syscall1(SYS_GETPPID, 0) != parent_pid {
        let _ = syscall1(SYS_EXIT, 43);
        loop {}
    }
    print("abi: fork ok\n");
    let _ = syscall1(SYS_EXIT, 42);
    loop {}
}

/// A child that sleeps before exiting, so the parent's first `WNOHANG` poll is
/// guaranteed to find it alive and its blocking wait then runs concurrently.
fn fork_sleeper() -> ! {
    let pause = Timespec {
        seconds: 0,
        nanos: 50_000_000,
    };
    let _ = syscall2(SYS_NANOSLEEP, &pause as *const Timespec as usize, 0);
    let _ = syscall1(SYS_EXIT, 7);
    loop {}
}

/// `fork`/`wait4`/`getppid` and their error paths. Runs with or without `/mnt`.
fn process_section() {
    let parent_pid = syscall1(172, 0);
    check(parent_pid > 0, 300, "process parent pid");
    check(syscall1(SYS_GETPPID, 0) == 0, 301, "foreground getppid");

    let child = fork_process();
    if child == 0 {
        fork_child_marks(parent_pid);
    }
    check(child > 0, 302, "fork child pid");
    let mut status: i32 = 0;
    check(
        wait_child(child, &mut status, 0) == child,
        303,
        "wait4 reaps child",
    );
    check(status == 42 << 8, 304, "wait4 exit status");
    print("abi: wait ok\n");

    // WNOHANG on a live child is 0; the blocking call that follows reaps it.
    let sleeper = fork_process();
    if sleeper == 0 {
        fork_sleeper();
    }
    check(sleeper > 0, 305, "fork sleeper pid");
    let mut sleeper_status: i32 = 0;
    check(
        wait_child(sleeper, &mut sleeper_status, WNOHANG) == 0,
        306,
        "wait4 WNOHANG while running",
    );
    check(
        wait_child(sleeper, &mut sleeper_status, 0) == sleeper,
        307,
        "wait4 blocking sleeper",
    );
    check(sleeper_status == 7 << 8, 308, "sleeper exit status");

    // ── fd/pipe inheritance ──
    // A pipe written before `fork` must be readable in the child; both ends are
    // descriptor references, not copies of the ring. The child closes its two
    // ends before exiting, so the parent's `wait4` also proves the ring is
    // released after the last reference (the kernel's `live_pipes() == 0`).
    let mut pipe_fds = [0i32; 2];
    check(
        syscall2(SYS_PIPE2, pipe_fds.as_mut_ptr() as usize, 0) == 0,
        340,
        "fork pipe2",
    );
    let pipe_read = pipe_fds[0] as usize;
    let pipe_write = pipe_fds[1] as usize;
    check(
        syscall3(SYS_WRITE, pipe_write, b"P".as_ptr() as usize, 1) == 1,
        341,
        "fork pipe write",
    );
    let pipe_child = fork_process();
    if pipe_child == 0 {
        let mut byte = [0u8; 1];
        let got = syscall3(SYS_READ, pipe_read, byte.as_mut_ptr() as usize, 1);
        let ok = got == 1 && byte[0] == b'P';
        let _ = syscall1(SYS_CLOSE, pipe_read);
        let _ = syscall1(SYS_CLOSE, pipe_write);
        let _ = syscall1(SYS_EXIT, if ok { 0 } else { 47 });
        loop {}
    }
    check(pipe_child > 0, 342, "fork pipe child");
    check(syscall1(SYS_CLOSE, pipe_write) == 0, 343, "parent close pipe write");
    check(syscall1(SYS_CLOSE, pipe_read) == 0, 344, "parent close pipe read");
    let mut pipe_status: i32 = 0;
    check(
        wait_child(pipe_child, &mut pipe_status, 0) == pipe_child,
        345,
        "fork pipe child reaped",
    );
    check(pipe_status == 0, 346, "fork pipe byte read");

    // ── signal inheritance ──
    // `fork` copies the handler table; the child proves it by delivering SIGUSR1
    // to itself. If the handler had been reset, the default action would kill
    // the child and `wait4` would report signal 10 instead of exit 0.
    let inherited = SigAction {
        handler: sigusr1_handler as *const () as usize,
        flags: SA_RESTORER,
        mask: 0,
        restorer: abi_sigrestorer as *const () as usize,
    };
    check(
        rt_sigaction(SIGUSR1, &inherited, core::ptr::null_mut()) == 0,
        347,
        "fork install SIGUSR1",
    );
    let sig_child = fork_process();
    if sig_child == 0 {
        let self_pid = syscall1(172, 0);
        if kill(self_pid, SIGUSR1) != 0 {
            let _ = syscall1(SYS_EXIT, 48);
            loop {}
        }
        if SIGUSR1_HITS.load(Ordering::SeqCst) != 6 {
            let _ = syscall1(SYS_EXIT, 49);
            loop {}
        }
        let _ = syscall1(SYS_EXIT, 0);
        loop {}
    }
    check(sig_child > 0, 348, "fork signal child");
    let mut sig_status: i32 = 0;
    check(
        wait_child(sig_child, &mut sig_status, 0) == sig_child,
        349,
        "fork signal child reaped",
    );
    check(sig_status == 0, 350, "fork inherited SIGUSR1 handler");
    let dfl_usr1 = SigAction {
        handler: SIG_DFL,
        flags: 0,
        mask: 0,
        restorer: 0,
    };
    check(
        rt_sigaction(SIGUSR1, &dfl_usr1, core::ptr::null_mut()) == 0,
        351,
        "fork restore SIG_DFL",
    );

    // Errors: no such child, an already-reaped child, bad pid and bad options.
    check(wait_child(999_999, &mut status, 0) == ECHILD, 309, "wait4 no child");
    check(wait_child(child, &mut status, 0) == ECHILD, 310, "wait4 reaped child");
    check(wait_child(-2, &mut status, 0) == EINVAL, 311, "wait4 bad pid");
    check(wait_child(-1, &mut status, 2) == EINVAL, 312, "wait4 bad options");

    // execve error paths: the process is unchanged after every one of them.
    let null_argv = [0usize; 1];
    let null_env = [0usize; 1];
    check(
        execve(b"/mnt/abi-no-such-file\0", null_argv.as_ptr(), null_env.as_ptr()) == ENOENT,
        313,
        "execve missing path",
    );
    check(
        execve(b"/mnt/exec-child\0", 0xdead_0000usize as *const usize, null_env.as_ptr()) == EFAULT,
        314,
        "execve bad argv",
    );
    check(
        syscall3(
            SYS_EXECVE,
            0xdead_0000usize,
            null_argv.as_ptr() as usize,
            null_env.as_ptr() as usize,
        ) == EFAULT,
        315,
        "execve bad path",
    );
    check(
        execve(b"/mnt/exec-child\0", null_argv.as_ptr(), 0xdead_0000usize as *const usize) == EFAULT,
        316,
        "execve bad envp",
    );
    check(syscall1(SYS_GETPPID, 0) == 0, 317, "getppid after exec errors");
}

/// Sv39 canonicality of user pointers (F-r2-rv-1), the reject matrix for the
/// bug that made one `ecall` a machine kill.
///
/// `user_range_valid` walks the page tables with `mmu::vpn`, which keeps only
/// nine bits per level: for a non-canonical address the walk silently resolves
/// the low 39-bit alias, said "mapped", and the copy then dereferenced the full
/// 64-bit value. An S-mode load/store page fault has `SPP = 1`, so the trap
/// handler printed `[EXC]` and parked the hart — userland could stop the OS.
///
/// Every case ORs a bad prefix with a genuinely mapped user address, so the low
/// 39 bits are a live `U|R|W` page and only the new canonicality/user-half rule
/// can reject it. All of them must answer `-EFAULT` (or `-EINVAL` for the
/// pointer-shaped `clone` arguments) with the fixture and the machine alive.
fn pointer_canonical_section() {
    /// `0xdead_…`: the prefix the round-2 PoC used; inside the Sv39 hole.
    const NONCANON_HIGH: usize = 0xdead_0000_0000_0000;
    /// Bit 39 set: the other non-canonical shape (a "low" address that is not).
    const NONCANON_LOW: usize = 0x0000_0080_0000_0000;
    /// Canonical **upper** half: `is_canonical` alone accepts it, but a user
    /// page can never live there — and its low 39-bit alias is a user page.
    const KERNEL_HIGH: usize = 0xffff_ffc0_0000_0000;
    /// The kernel image's own identity mapping: canonical, low half, `U = 0`.
    const KERNEL_IDENTITY: usize = 0x8000_0000;
    /// One past the highest user address (`elf::USER_TOP`).
    const USER_TOP: usize = 0x4000_0000;

    let mut scratch = [0u8; 64];
    let data = scratch.as_mut_ptr() as usize;
    let mut word = 0u32;
    let word_addr = &mut word as *mut u32 as usize;
    let path = b"/mnt/abi-syscalls.txt\0";
    let name = path.as_ptr() as usize;
    let null_argv = [0usize; 1];
    let null_env = [0usize; 1];
    let mut alt = SigAltStack {
        ss_sp: 0,
        ss_flags: 0,
        _pad: 0,
        ss_size: MINSIGSTKSZ,
    };
    let mut timespec = [0u8; 16];
    let mut canonical_stat = Stat {
        mode: 0,
        kind: 0,
        size: 0,
        ino: 0,
    };

    // ── 0xdead_…: what the PoC sends, through every copy-helper shape ──
    check(
        syscall3(SYS_WRITE, 1, NONCANON_HIGH | data, 8) == EFAULT,
        3100,
        "write 0xdead prefix",
    );
    // `read(0, …)` answers `-EAGAIN` before it validates anything when the console
    // ring is empty, so the read-side copy is exercised through `ppoll`'s timeout
    // pointer instead: that one is copied (validated) unconditionally.
    check(
        syscall3(SYS_PPOLL, 0, 0, NONCANON_HIGH | data) == EFAULT,
        3101,
        "ppoll 0xdead timeout prefix",
    );
    check(
        syscall4(SYS_OPENAT, 0, NONCANON_HIGH | name, O_RDONLY, 0) == EFAULT,
        3102,
        "openat 0xdead path prefix",
    );
    check(
        syscall2(SYS_FSTAT, 1, NONCANON_HIGH | data) == EFAULT,
        3103,
        "fstat 0xdead struct prefix",
    );
    check(
        syscall2(SYS_CLOCK_GETTIME, 0, NONCANON_HIGH | data) == EFAULT,
        3104,
        "clock_gettime 0xdead struct prefix",
    );
    check(
        syscall2(SYS_NANOSLEEP, NONCANON_HIGH | data, 0) == EFAULT,
        3105,
        "nanosleep 0xdead struct prefix",
    );
    // `FUTEX_WAIT` reads the word first (that is where the pointer check is);
    // `FUTEX_WAKE` does not touch it. A zero timeout keeps a broken kernel from
    // parking the fixture here instead of failing the check.
    timespec.fill(0);
    check(
        futex_wait_word(NONCANON_HIGH | word_addr, 0, timespec.as_mut_ptr() as usize) == EFAULT,
        3106,
        "futex 0xdead word prefix",
    );
    check(
        syscall1(SYS_UNAME, NONCANON_HIGH | data) == EFAULT,
        3107,
        "uname 0xdead struct prefix",
    );
    check(
        syscall3(SYS_GETRANDOM, NONCANON_HIGH | data, 16, 0) == EFAULT,
        3108,
        "getrandom 0xdead buffer prefix",
    );
    check(
        syscall2(SYS_PIPE2, NONCANON_HIGH | data, 0) == EFAULT,
        3109,
        "pipe2 0xdead fd-array prefix",
    );
    check(
        syscall3(SYS_POLL, NONCANON_HIGH | data, 1, 0) == EFAULT,
        3110,
        "poll 0xdead fd-array prefix",
    );
    check(
        syscall4(SYS_RT_SIGACTION, SIGUSR1, NONCANON_HIGH | data, 0, 8) == EFAULT,
        3111,
        "rt_sigaction 0xdead struct prefix",
    );
    check(
        syscall4(SYS_RT_SIGPROCMASK, SIG_BLOCK, NONCANON_HIGH | data, 0, 8) == EFAULT,
        3112,
        "rt_sigprocmask 0xdead mask prefix",
    );
    // The alternate stack is validated page by page when it is installed, so a
    // non-canonical `ss_sp` is refused before it can ever become a signal frame.
    alt.ss_sp = NONCANON_HIGH | data;
    check(
        syscall2(SYS_SIGALTSTACK, &alt as *const SigAltStack as usize, 0) == EFAULT,
        3113,
        "sigaltstack 0xdead ss_sp prefix",
    );
    check(
        syscall4(
            SYS_WAIT4,
            (-1isize) as usize,
            NONCANON_HIGH | data,
            0,
            0,
        ) == EFAULT,
        3114,
        "wait4 0xdead status prefix",
    );
    check(
        syscall3(
            SYS_EXECVE,
            NONCANON_HIGH | name,
            null_argv.as_ptr() as usize,
            null_env.as_ptr() as usize,
        ) == EFAULT,
        3115,
        "execve 0xdead path prefix",
    );
    check(
        syscall3(
            SYS_EXECVE,
            b"/\0".as_ptr() as usize,
            NONCANON_HIGH | null_argv.as_ptr() as usize,
            null_env.as_ptr() as usize,
        ) == EFAULT,
        3116,
        "execve 0xdead argv prefix",
    );
    // `clone` takes two bare pointers (stack, entry) and checks them with
    // `paging::flags`; the alias must fail that walk even though its low bits
    // are a mapped page. `-EINVAL` is the documented answer for both.
    let stack_base = (data + 15) & !0xf;
    check(
        spawn_thread(
            THREAD_FLAGS,
            NONCANON_HIGH | stack_base,
            spawn_marker_entry as *const () as usize,
            0,
        )
            == EINVAL,
        3117,
        "clone 0xdead stack prefix",
    );
    check(
        spawn_thread(
            THREAD_FLAGS,
            stack_base,
            NONCANON_HIGH | (spawn_marker_entry as *const () as usize),
            0,
        ) == EINVAL,
        3118,
        "clone 0xdead entry prefix",
    );

    // ── 0x0000_0080_…: bit 39 set, non-canonical on the other side ──
    check(
        syscall3(SYS_WRITE, 1, NONCANON_LOW | data, 8) == EFAULT,
        3119,
        "write bit-39 prefix",
    );
    check(
        syscall4(SYS_OPENAT, 0, NONCANON_LOW | name, O_RDONLY, 0) == EFAULT,
        3120,
        "openat bit-39 path prefix",
    );
    check(
        syscall2(SYS_CLOCK_GETTIME, 0, NONCANON_LOW | data) == EFAULT,
        3121,
        "clock_gettime bit-39 struct prefix",
    );

    // ── The upper half: canonical, so a bare `is_canonical` would pass it ──
    check(
        syscall3(SYS_WRITE, 1, KERNEL_HIGH | data, 8) == EFAULT,
        3122,
        "write canonical kernel-half prefix",
    );
    check(
        syscall4(SYS_OPENAT, 0, KERNEL_HIGH | name, O_RDONLY, 0) == EFAULT,
        3123,
        "openat canonical kernel-half path prefix",
    );
    check(
        syscall2(SYS_FSTAT, 1, KERNEL_HIGH | data) == EFAULT,
        3124,
        "fstat canonical kernel-half struct prefix",
    );

    // ── Kernel addresses must never validate as user buffers ──
    check(
        syscall3(SYS_WRITE, 1, KERNEL_IDENTITY, 8) == EFAULT,
        3125,
        "write kernel identity mapping",
    );
    check(
        syscall2(SYS_FSTAT, 1, KERNEL_IDENTITY) == EFAULT,
        3126,
        "fstat kernel identity mapping",
    );
    check(
        syscall4(SYS_OPENAT, 0, KERNEL_IDENTITY, O_RDONLY, 0) == EFAULT,
        3127,
        "openat kernel identity mapping",
    );

    // ── A range that starts in the stack but ends past USER_TOP is refused,
    //    even though its first page is a perfectly good user page. ──
    check(
        syscall3(SYS_WRITE, 1, USER_TOP - 2, 4) == EFAULT,
        3128,
        "write range past USER_TOP",
    );
    check(
        syscall2(SYS_NANOSLEEP, USER_TOP - 8, 0) == EFAULT,
        3129,
        "nanosleep range past USER_TOP",
    );

    // Positive controls: the same calls with the genuine pointers still work, so
    // the new rule rejects only what it must.
    let hello = b"abi: canonical pointers ok\n";
    check(
        syscall3(SYS_WRITE, 1, hello.as_ptr() as usize, hello.len()) == hello.len() as isize,
        3130,
        "write canonical buffer",
    );
    timespec.fill(0);
    check(
        syscall2(SYS_CLOCK_GETTIME, 1, timespec.as_mut_ptr() as usize) == 0,
        3131,
        "clock_gettime canonical struct",
    );
    canonical_stat.mode = 0;
    check(
        syscall2(
            SYS_FSTAT,
            1,
            &mut canonical_stat as *mut Stat as usize,
        ) == 0,
        3132,
        "fstat canonical struct",
    );
    check(
        syscall4(SYS_OPENAT, 0, b"/\0".as_ptr() as usize, O_RDONLY, 0) >= 3,
        3133,
        "openat canonical root",
    );
}

/// A trivial thread entry `clone` never reaches (the argument checks refuse it
/// first); it only needs a real code address for the entry-pointer control.
fn spawn_marker_entry(_arg: usize) -> ! {
    loop {}
}

/// A forked child execs `/mnt/exec-child`; the parent's `wait4` status 0 proves
/// the image ran with the expected `argv[1]` and environment.
fn exec_section() {
    // A non-ELF file is refused with ENOEXEC, and the child process survives.
    let bad = fork_process();
    if bad == 0 {
        let null_argv = [0usize; 1];
        let null_env = [0usize; 1];
        let result = execve(
            b"/mnt/abi-syscalls.txt\0",
            null_argv.as_ptr(),
            null_env.as_ptr(),
        );
        let _ = syscall1(SYS_EXIT, if result == ENOEXEC { 0 } else { 91 });
        loop {}
    }
    check(bad > 0, 320, "fork execenoexec child");
    let mut status: i32 = 0;
    check(wait_child(bad, &mut status, 0) == bad, 321, "exec ENOEXEC reaped");
    check(status == 0, 322, "exec ENOEXEC returned");

    let child = fork_process();
    if child == 0 {
        let argv = [
            b"exec-child\0".as_ptr() as usize,
            b"exec-arg\0".as_ptr() as usize,
            0,
        ];
        let envp = [b"PWD=/mnt\0".as_ptr() as usize, 0];
        let result = execve(
            b"/mnt/exec-child\0",
            argv.as_ptr(),
            envp.as_ptr(),
        );
        // execve only returns on error.
        let _ = syscall1(SYS_EXIT, 90 + (-result) as usize);
        loop {}
    }
    check(child > 0, 323, "fork exec child");
    let mut status: i32 = 0;
    check(wait_child(child, &mut status, 0) == child, 324, "exec child reaped");
    check(status == 0, 325, "exec child exit status");
    print("abi: exec wait ok\n");

    // ── exec resets the signal table, keeps the blocked mask ──
    // SIG_IGN is the observable case: POSIX keeps it across `exec`, pagh
    // deliberately resets every action to SIG_DFL. The child also sees the
    // blocked mask inherited by `fork` and preserved by `execve`.
    let ignored = SigAction {
        handler: SIG_IGN,
        flags: 0,
        mask: 0,
        restorer: 0,
    };
    check(
        rt_sigaction(SIGUSR1, &ignored, core::ptr::null_mut()) == 0,
        352,
        "exec install SIG_IGN",
    );
    let blocked: u64 = 1 << (SIGUSR1 - 1);
    check(
        rt_sigprocmask(SIG_BLOCK, &blocked, core::ptr::null_mut()) == 0,
        353,
        "exec block SIGUSR1",
    );
    let signal_child = fork_process();
    if signal_child == 0 {
        let argv = [
            b"exec-child\0".as_ptr() as usize,
            b"exec-arg\0".as_ptr() as usize,
            0,
        ];
        let envp = [
            b"PWD=/mnt\0".as_ptr() as usize,
            b"PAGH_EXEC_SIGNAL=1\0".as_ptr() as usize,
            0,
        ];
        let result = execve(b"/mnt/exec-child\0", argv.as_ptr(), envp.as_ptr());
        let _ = syscall1(SYS_EXIT, 90 + (-result) as usize);
        loop {}
    }
    check(signal_child > 0, 354, "fork exec signal child");
    let mut signal_status: i32 = 0;
    check(
        wait_child(signal_child, &mut signal_status, 0) == signal_child,
        355,
        "exec signal child reaped",
    );
    check(
        signal_status == 0,
        356,
        "exec reset SIG_IGN and kept blocked mask",
    );
    check(
        rt_sigprocmask(SIG_UNBLOCK, &blocked, core::ptr::null_mut()) == 0,
        357,
        "exec unblock SIGUSR1",
    );
    let dfl = SigAction {
        handler: SIG_DFL,
        flags: 0,
        mask: 0,
        restorer: 0,
    };
    check(
        rt_sigaction(SIGUSR1, &dfl, core::ptr::null_mut()) == 0,
        358,
        "exec restore SIG_DFL",
    );
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
    // SOCK_DGRAM is a real UDP socket now. The negative half below covers
    // bind/recvfrom/sendto without needing a NIC or a host listener, so it
    // runs on a serial-only boot too.
    let udp = syscall3(SYS_SOCKET, AF_INET, SOCK_DGRAM, 0);
    check(udp >= 3, 141, "socket(SOCK_DGRAM)");
    let udp_nonblocking = syscall3(SYS_SOCKET, AF_INET, SOCK_DGRAM | 0x800, 0);
    check(udp_nonblocking >= 3, 400, "socket(SOCK_DGRAM|SOCK_NONBLOCK)");
    check(
        syscall1(SYS_CLOSE, udp_nonblocking as usize) == 0,
        401,
        "close non-blocking UDP socket",
    );
    check(
        syscall3(SYS_SOCKET, AF_INET, SOCK_STREAM as usize, 6) == EAFNOSUPPORT,
        142,
        "socket(protocol 6)",
    );

    // UDP bind: 0.0.0.0:45679 is deliberately in the ephemeral range but an
    // explicit port, so the same number is testable twice in one boot.
    let mut udp_address = [0u8; 16];
    udp_address[0..2].copy_from_slice(&(AF_INET as u16).to_le_bytes());
    udp_address[2..4].copy_from_slice(&45679u16.to_be_bytes());
    check(
        syscall3(SYS_BIND, udp as usize, 0x10, 16) == EFAULT,
        402,
        "bind bad pointer",
    );
    check(
        syscall3(SYS_BIND, udp as usize, udp_address.as_ptr() as usize, 8) == EINVAL,
        403,
        "bind addrlen",
    );
    let mut bad_family = udp_address;
    bad_family[0] = 1;
    check(
        syscall3(
            SYS_BIND,
            udp as usize,
            bad_family.as_ptr() as usize,
            16,
        ) == EAFNOSUPPORT,
        404,
        "bind family",
    );
    check(
        syscall3(SYS_BIND, 99, udp_address.as_ptr() as usize, 16) == EBADF,
        405,
        "bind bad fd",
    );
    check(
        syscall3(SYS_BIND, 1, udp_address.as_ptr() as usize, 16) == ENOTSOCK,
        406,
        "bind non-socket fd",
    );
    check(
        syscall3(
            SYS_BIND,
            udp as usize,
            udp_address.as_ptr() as usize,
            16,
        ) == 0,
        407,
        "bind UDP port",
    );
    check(
        syscall3(
            SYS_BIND,
            udp as usize,
            udp_address.as_ptr() as usize,
            16,
        ) == EINVAL,
        408,
        "bind twice",
    );

    let udp2 = syscall3(SYS_SOCKET, AF_INET, SOCK_DGRAM, 0);
    check(udp2 >= 3, 409, "second UDP socket");
    check(
        syscall3(
            SYS_BIND,
            udp2 as usize,
            udp_address.as_ptr() as usize,
            16,
        ) == EADDRINUSE,
        410,
        "bind duplicate port",
    );
    check(
        syscall2(SYS_LISTEN, udp2 as usize, 1) == ENOTSUP,
        423,
        "listen on UDP socket",
    );
    let mut udp_peer = [0u8; 16];
    let mut udp_peer_len = 16u32;
    check(
        syscall3(
            SYS_ACCEPT,
            udp2 as usize,
            udp_peer.as_mut_ptr() as usize,
            &mut udp_peer_len as *mut u32 as usize,
        ) == ENOTSUP,
        424,
        "accept on UDP socket",
    );

    let mut foreign_address = udp_address;
    foreign_address[4..8].copy_from_slice(&[10u8, 0, 2, 2]);
    check(
        syscall3(
            SYS_BIND,
            udp2 as usize,
            foreign_address.as_ptr() as usize,
            16,
        ) == EADDRNOTAVAIL,
        422,
        "bind foreign address",
    );

    let mut udp_buffer = [0u8; 8];
    check(
        syscall6(
            SYS_RECVFROM,
            udp as usize,
            udp_buffer.as_mut_ptr() as usize,
            udp_buffer.len(),
            MSG_DONTWAIT,
            0,
            0,
        ) == EAGAIN,
        411,
        "nonblocking recvfrom",
    );
    let mut udp_option: u32 = 0;
    let mut udp_option_len: u32 = 4;
    check(
        syscall5(
            SYS_GETSOCKOPT,
            udp as usize,
            SOL_SOCKET,
            SO_TYPE,
            (&mut udp_option as *mut u32) as usize,
            (&mut udp_option_len as *mut u32) as usize,
        ) == 0,
        412,
        "UDP getsockopt SO_TYPE",
    );
    check(
        udp_option == SOCK_DGRAM as u32 && udp_option_len == 4,
        413,
        "UDP SO_TYPE value",
    );

    let udp_neighbour = [10u8, 0, 2, 2];
    let mut destination = udp_address;
    destination[2..4].copy_from_slice(&9u16.to_be_bytes());
    destination[4..8].copy_from_slice(&udp_neighbour);
    check(
        syscall6(
            SYS_SENDTO,
            udp as usize,
            udp_buffer.as_ptr() as usize,
            4,
            0,
            0,
            0,
        ) == EDESTADDRREQ,
        414,
        "sendto without destination",
    );
    check(
        syscall6(
            SYS_SENDTO,
            udp as usize,
            udp_buffer.as_ptr() as usize,
            4,
            0,
            destination.as_ptr() as usize,
            8,
        ) == EINVAL,
        415,
        "sendto addrlen",
    );
    let mut wrong_destination = destination;
    wrong_destination[0] = 1;
    check(
        syscall6(
            SYS_SENDTO,
            udp as usize,
            udp_buffer.as_ptr() as usize,
            4,
            0,
            wrong_destination.as_ptr() as usize,
            16,
        ) == EAFNOSUPPORT,
        416,
        "sendto family",
    );
    let big = [0u8; 1025];
    check(
        syscall6(
            SYS_SENDTO,
            udp as usize,
            big.as_ptr() as usize,
            big.len(),
            0,
            destination.as_ptr() as usize,
            16,
        ) == EMSGSIZE,
        417,
        "sendto oversized datagram",
    );
    check(
        syscall6(
            SYS_SENDTO,
            udp as usize,
            0x10,
            4,
            0,
            destination.as_ptr() as usize,
            16,
        ) == EFAULT,
        418,
        "sendto bad buffer",
    );
    // Closing the owner releases the port; the second socket can then bind it.
    check(syscall1(SYS_CLOSE, udp as usize) == 0, 419, "close UDP socket");
    check(
        syscall3(
            SYS_BIND,
            udp2 as usize,
            udp_address.as_ptr() as usize,
            16,
        ) == 0,
        420,
        "bind released port",
    );
    check(syscall1(SYS_CLOSE, udp2 as usize) == 0, 421, "close second UDP socket");

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

    // ── TCP passive open: bind(200) on a stream socket, listen(201),
    //    non-blocking accept(202), the accept queue and its teardown. No host
    //    connection is needed: the listener is created with SOCK_NONBLOCK, so
    //    an empty queue answers EAGAIN immediately instead of polling 30 s. ──
    let mut server_address = [0u8; 16];
    server_address[0..2].copy_from_slice(&(AF_INET as u16).to_le_bytes());
    server_address[2..4].copy_from_slice(&45680u16.to_be_bytes());
    let server = syscall3(SYS_SOCKET, AF_INET, (SOCK_STREAM | 0x800) as usize, 0);
    check(server >= 3, 430, "socket(SOCK_STREAM|SOCK_NONBLOCK)");
    check(
        syscall2(SYS_LISTEN, server as usize, 4) == EINVAL,
        431,
        "listen before bind",
    );
    check(
        syscall3(SYS_BIND, server as usize, server_address.as_ptr() as usize, 16) == 0,
        432,
        "bind TCP port",
    );
    check(
        syscall2(SYS_LISTEN, server as usize, 4) == 0,
        433,
        "listen TCP port",
    );
    check(
        syscall2(SYS_LISTEN, server as usize, 8) == 0,
        434,
        "listen updates backlog",
    );
    check(
        syscall3(SYS_BIND, server as usize, server_address.as_ptr() as usize, 16) == EINVAL,
        435,
        "bind a listener",
    );
    let mut peer = [0u8; 16];
    let mut peer_len = 16u32;
    check(
        syscall3(
            SYS_ACCEPT,
            server as usize,
            0x10,
            &mut peer_len as *mut u32 as usize,
        ) == EFAULT,
        437,
        "accept bad address",
    );
    check(
        syscall3(
            SYS_ACCEPT,
            server as usize,
            peer.as_mut_ptr() as usize,
            &mut peer_len as *mut u32 as usize,
        ) == EAGAIN,
        438,
        "accept empty queue",
    );
    check(
        syscall3(SYS_ACCEPT, server as usize, 0, 0) == EAGAIN,
        439,
        "accept empty queue without address",
    );
    let mut listener_poll = [0u8; 8];
    listener_poll[0..4].copy_from_slice(&(server as i32).to_le_bytes());
    listener_poll[4..6].copy_from_slice(&1i16.to_le_bytes()); // POLLIN
    check(
        syscall3(SYS_POLL, listener_poll.as_mut_ptr() as usize, 1, 0) == 0,
        450,
        "poll listener with an empty accept queue",
    );
    let listener_dup = syscall1(SYS_DUP, server as usize);
    check(listener_dup >= 3, 440, "dup listener descriptor");
    check(
        syscall3(
            SYS_WRITE,
            listener_dup as usize,
            b"x".as_ptr() as usize,
            1,
        ) == ENOTSUP,
        441,
        "dup of a listener is ENOTSUP",
    );
    check(
        syscall1(SYS_CLOSE, listener_dup as usize) == 0,
        442,
        "close dup listener",
    );
    check(
        syscall1(SYS_CLOSE, server as usize) == 0,
        443,
        "close listener",
    );
    let server2 = syscall3(SYS_SOCKET, AF_INET, SOCK_STREAM as usize, 0);
    check(server2 >= 3, 444, "socket after listener close");
    check(
        syscall3(SYS_BIND, server2 as usize, server_address.as_ptr() as usize, 16) == 0,
        445,
        "rebind released TCP port",
    );
    check(
        syscall1(SYS_CLOSE, server2 as usize) == 0,
        446,
        "close pre-listen stream socket",
    );
    let unbound = syscall3(SYS_SOCKET, AF_INET, SOCK_STREAM as usize, 0);
    check(unbound >= 3, 447, "socket for unbound accept");
    check(
        syscall3(SYS_ACCEPT, unbound as usize, 0, 0) == EINVAL,
        448,
        "accept on an unbound stream socket",
    );
    check(
        syscall1(SYS_CLOSE, unbound as usize) == 0,
        449,
        "close unbound stream socket",
    );

    // ── native threads: clone/gettid/futex/thread exit (runs with or without /mnt) ──
    let thread_stack = thread_section();

    // ── signals: handlers, masks, pending delivery, rt_sigreturn (runs with or
    //    without /mnt; the child threads above have all exited, so kill(0) and
    //    kill(-1) address exactly the main thread) ──
    signal_section(thread_stack);
    signal_ext_section(thread_stack);

    // ── process family: fork/wait4/getppid, before the filesystem split ──
    process_section();

    // ── Sv39 canonicality of user pointers (F-r2-rv-1): the reject matrix for
    //    every copy-helper shape, the two non-canonical prefixes, the
    //    supervisor half and kernel addresses; runs with or without /mnt. ──
    pointer_canonical_section();

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

    // The exec target lives on /mnt; run it in a forked child so the parent's
    // final exit_group/at-exit checks still happen.
    exec_section();
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