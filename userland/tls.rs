//! The entropy + TLS-syscall acceptance fixture: one static RV64 program that
//! exercises `getrandom(278)` and the pagh-native TLS family (1100–1103) without
//! libpagh.
//!
//! What it proves, and what it deliberately does not:
//!
//!   * `getrandom` fills a user buffer from the kernel pool, refuses unknown
//!     flags with `-EINVAL`, refuses a bad pointer with `-EFAULT` *before*
//!     writing anything, returns 0 for a zero-length request, and answers a
//!     request larger than the ABI's per-call cap with a short read that stops
//!     exactly at the cap;
//!   * the TLS family fails closed on every descriptor that is not a session:
//!     `-EBADF` for a free number, `-ENOTSOCK` for the console, `-EINVAL` for a
//!     non-TLS socket and for unknown flags, `-ENOTCONN` for an unconnected
//!     socket, `-EFAULT` for a bad host pointer, and a refused `tls_close` that
//!     leaves the descriptor intact (it is closed normally afterwards).
//!
//! It cannot prove a *handshake*: that needs a TLS server, which this fixture
//! has no way to reach from the in-guest suite. The live handshake is exercised
//! by `tools/test_tls.py` (the shell's `https` and the libpagh `https_get`
//! example against `example.com`), which is the acceptance run this fixture
//! complements rather than replaces.
//!
//! Exit code 0 when every check passed. A failure prints `tls: FAIL <what>` on
//! the console and exits with a distinct non-zero code, which the selftest maps
//! to FAIL; the selftest also requires the `tls: ok` marker.
//!
//! No allocator, no `core::fmt`, no libc. Build with `userland/build.sh`.

#![no_std]
#![no_main]

core::arch::global_asm!(
    r#"
    .section .text.entry, "ax", @progbits
    .globl _start
_start:
    call    tls_main
    li      a7, 93
    ecall
1:  j       1b
"#
);

// ─── syscall wrapper ───────────────────────────────────────────────────────

fn syscall4(number: usize, a0: usize, a1: usize, a2: usize, a3: usize) -> isize {
    let result: isize;
    // SAFETY: `ecall` enters the kernel with `a7` as the number and `a0..a5` as
    // arguments; the kernel validates every user pointer and restores `sp` and
    // the callee-saved registers on return.
    unsafe {
        core::arch::asm!(
            "ecall",
            in("a7") number,
            inlateout("a0") a0 => result,
            in("a1") a1,
            in("a2") a2,
            in("a3") a3,
        )
    };
    result
}

fn syscall3(number: usize, a0: usize, a1: usize, a2: usize) -> isize {
    syscall4(number, a0, a1, a2, 0)
}

fn syscall1(number: usize, a0: usize) -> isize {
    syscall4(number, a0, 0, 0, 0)
}

// ─── the ABI ───────────────────────────────────────────────────────────────

const SYS_WRITE: usize = 64;
const SYS_CLOSE: usize = 57;
const SYS_EXIT: usize = 93;
const SYS_SOCKET: usize = 198;
/// `getrandom(buf, len, flags)` — the asm-generic number.
const SYS_GETRANDOM: usize = 278;
/// The pagh-native TLS family (outside the asm-generic table, like `fork`).
const SYS_TLS_CONNECT: usize = 1100;
const SYS_TLS_WRITE: usize = 1101;
const SYS_TLS_READ: usize = 1102;
const SYS_TLS_CLOSE: usize = 1103;

const AF_INET: usize = 2;
const SOCK_STREAM: usize = 1;

const EFAULT: isize = -14;
const EINVAL: isize = -22;
const ENOTSOCK: isize = -88;
const ENOTCONN: isize = -107;
const EBADF: isize = -9;

/// What `getrandom` returns at most per call (`MAX_GETRANDOM` in the kernel).
const GETRANDOM_CAP: usize = 64 * 1024;

fn print(text: &str) {
    let _ = syscall3(SYS_WRITE, 1, text.as_ptr() as usize, text.len());
}

fn exit(code: i32) -> ! {
    let _ = syscall1(SYS_EXIT, code as usize);
    loop {}
}

/// Print the diagnostic and exit with `code` when `condition` is false.
fn check(condition: bool, code: i32, what: &str) {
    if condition {
        return;
    }
    print("tls: FAIL ");
    print(what);
    print("\n");
    exit(code);
}

// ─── buffers (in .bss: this program has no allocator and a 64 KiB stack) ────

/// One `getrandom` fill plus the canary that must survive a short read.
const RANDOM_BYTES: usize = GETRANDOM_CAP + 64;
const CANARY: u8 = 0xa5;

static mut RANDOM: [u8; RANDOM_BYTES] = [0; RANDOM_BYTES];
static mut OTHER: [u8; 64] = [0; 64];

/// The host name handed to `tls_connect`. Nothing here can complete a
/// handshake, so the contents only have to be well-formed.
const HOST: &[u8] = b"example.com";

/// 256 bytes: one past the ABI's host-name limit.
static LONG_HOST: [u8; 256] = [b'a'; 256];

fn random_buffer() -> &'static mut [u8; RANDOM_BYTES] {
    // SAFETY: the fixture is single-threaded, and `RANDOM` is a `static mut`
    // it owns for the whole run. No reference outlives the call it is made in.
    unsafe { &mut *core::ptr::addr_of_mut!(RANDOM) }
}

fn other_buffer() -> &'static mut [u8; 64] {
    // SAFETY: as `random_buffer`.
    unsafe { &mut *core::ptr::addr_of_mut!(OTHER) }
}

// ─── getrandom ─────────────────────────────────────────────────────────────

fn getrandom_section() -> i32 {
    // A zero-length request is 0 and touches nothing.
    check(syscall3(SYS_GETRANDOM, 0, 0, 0) == 0, 10, "getrandom(0) is not 0");

    // The ABI defines no flags: anything but 0 is refused.
    check(
        syscall3(SYS_GETRANDOM, 0, 8, 1) == EINVAL,
        11,
        "getrandom accepted an unknown flag",
    );

    // A bad pointer is refused before a single byte is produced.
    let buffer = random_buffer();
    check(
        syscall3(SYS_GETRANDOM, 0, 32, 0) == EFAULT,
        12,
        "getrandom(0) did not fault",
    );

    // Fill 32 bytes: not all zero, and two draws differ (the pool ratchets, so
    // a repeat would mean the counter or the state is not moving).
    check(
        syscall3(SYS_GETRANDOM, buffer.as_mut_ptr() as usize, 32, 0) == 32,
        13,
        "getrandom(32) did not return 32 bytes",
    );
    check(
        buffer[..32].iter().any(|byte| *byte != 0),
        14,
        "getrandom returned 32 zero bytes",
    );
    let second = other_buffer();
    check(
        syscall3(SYS_GETRANDOM, second.as_mut_ptr() as usize, 32, 0) == 32,
        15,
        "the second getrandom(32) did not return 32 bytes",
    );
    check(
        buffer[..32] != second[..32],
        16,
        "two getrandom draws returned the same bytes",
    );

    // A request past the ABI's per-call cap is a short read that stops exactly
    // at the cap; the canary behind it must be untouched.
    for byte in buffer.iter_mut() {
        *byte = 0;
    }
    for byte in buffer[GETRANDOM_CAP..].iter_mut() {
        *byte = CANARY;
    }
    let read = syscall3(SYS_GETRANDOM, buffer.as_mut_ptr() as usize, RANDOM_BYTES, 0);
    check(
        read == GETRANDOM_CAP as isize,
        17,
        "a request past the cap was not answered with a short read at the cap",
    );
    check(
        buffer[..GETRANDOM_CAP].iter().any(|byte| *byte != 0),
        18,
        "the capped getrandom returned zeros",
    );
    check(
        buffer[GETRANDOM_CAP..].iter().all(|byte| *byte == CANARY),
        19,
        "getrandom wrote past its per-call cap",
    );

    print("tls: getrandom ok\n");
    0
}

// ─── the TLS syscall family, fail-closed on every non-session descriptor ────

fn tls_abi_section() -> i32 {
    let host = HOST.as_ptr() as usize;
    let host_len = HOST.len();

    // A free descriptor number.
    check(
        syscall4(SYS_TLS_CONNECT, 99, host, host_len, 0) == EBADF,
        30,
        "tls_connect on a free fd was not -EBADF",
    );
    // The console is not a socket.
    check(
        syscall4(SYS_TLS_CONNECT, 1, host, host_len, 0) == ENOTSOCK,
        31,
        "tls_connect on the console was not -ENOTSOCK",
    );
    // Unknown flags are refused: the ABI defines none, and there is no bit that
    // relaxes verification.
    check(
        syscall4(SYS_TLS_CONNECT, 1, host, host_len, 1) == EINVAL,
        32,
        "tls_connect accepted unknown flags",
    );
    // Host name bounds and validity.
    check(
        syscall4(SYS_TLS_CONNECT, 1, host, 0, 0) == EINVAL,
        33,
        "tls_connect accepted an empty host name",
    );
    check(
        syscall4(
            SYS_TLS_CONNECT,
            1,
            LONG_HOST.as_ptr() as usize,
            LONG_HOST.len(),
            0,
        ) == EINVAL,
        34,
        "tls_connect accepted a host name past the ABI limit",
    );
    check(
        syscall4(SYS_TLS_CONNECT, 1, 0, host_len, 0) == EFAULT,
        35,
        "tls_connect did not fault on a bad host pointer",
    );

    // A real stream socket, not connected: the handshake needs a peer.
    let socket = syscall3(SYS_SOCKET, AF_INET, SOCK_STREAM, 0);
    check(socket >= 3, 36, "socket() did not return a stream descriptor");
    let fd = socket as usize;
    check(
        syscall4(SYS_TLS_CONNECT, fd, host, host_len, 0) == ENOTCONN,
        37,
        "tls_connect on an unconnected socket was not -ENOTCONN",
    );

    // I/O and close on a descriptor that is not a session.
    check(
        syscall3(SYS_TLS_READ, fd, other_buffer().as_mut_ptr() as usize, 8) == EINVAL,
        38,
        "tls_read on a non-session descriptor was not -EINVAL",
    );
    check(
        syscall3(SYS_TLS_WRITE, fd, other_buffer().as_ptr() as usize, 8) == EINVAL,
        39,
        "tls_write on a non-session descriptor was not -EINVAL",
    );
    check(
        syscall1(SYS_TLS_CLOSE, fd) == EINVAL,
        40,
        "tls_close on a non-session descriptor was not -EINVAL",
    );
    // ...and the refused `tls_close` must not have consumed the descriptor.
    check(
        syscall1(SYS_CLOSE, fd) == 0,
        41,
        "the descriptor did not survive a refused tls_close",
    );

    // A closed descriptor is `-EBADF` for the whole family, not a panic.
    check(
        syscall3(SYS_TLS_READ, fd, other_buffer().as_mut_ptr() as usize, 1) == EBADF,
        42,
        "tls_read on a closed fd was not -EBADF",
    );
    check(
        syscall1(SYS_TLS_CLOSE, fd) == EBADF,
        43,
        "tls_close on a closed fd was not -EBADF",
    );

    print("tls: abi ok\n");
    0
}

#[no_mangle]
pub extern "C" fn tls_main() -> i32 {
    print("tls: start\n");
    let code = getrandom_section();
    if code != 0 {
        return code;
    }
    let code = tls_abi_section();
    if code != 0 {
        return code;
    }
    print("tls: ok\n");
    0
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // A panic is a failure, but printing from the panic handler would need
    // formatting the fixture does not carry. Park instead: the selftest fails on
    // the missing exit code and the missing `tls: ok` marker.
    loop {}
}
