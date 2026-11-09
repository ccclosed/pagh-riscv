//! The native system-call ABI (libpagh): the RISC-V syscall numbers from
//! `pagh-api/docs/abi.md`, a trap frame that carries the arguments, a
//! per-process file-descriptor table, and pointer validation that never trusts
//! a U-mode address.
//!
//! Dispatch runs in the trap handler, with `sstatus.SIE = 0` (hardware cleared
//! it on entry), so a syscall cannot be preempted by the timer halfway through a
//! user-buffer copy. The one voluntary switch this layer performs is
//! `sched_yield`; `nanosleep` deliberately re-enables interrupts while it waits
//! in `wfi` (see that function) and holds no lock across it.
//!
//! # The file-descriptor table
//!
//! Each process owns one row of 16 slots (`FdTables`); `run_source` resets the
//! row before spawning, `fork` copies it, `execve` keeps it, and the teardown
//! releases it after the last thread: 0 = console input, 1/2 = console output,
//! 3.. = VFS nodes with an offset and the `openat` flags (or a pipe end/socket).
//! A directory's children are snapshotted at open time (`getdents64`), because
//! the VFS has no seekable directory cursor. The table's lock is never held
//! across a VFS/block call: a read or write clones the `Arc` and the offset,
//! releases the table, does the I/O, then re-locks to publish the new offset.
//! That matters because the block driver polls its device and may sleep on
//! `wfi`; a spinlock held across it would be held across an interrupt.
//!
//! # Pointers
//!
//! Every user pointer is validated page by page against the *current* map
//! before it is touched: `U=1` and `R=1` for a read, plus `W=1` for a write
//! (`W` without `R` is an unusual leaf and only writable, so the syscall layer
//! requires both). A bad address is `-EFAULT` with the fault counter bumped, not
//! an S-mode page fault — the kernel must survive userland. `SUM` is set only
//! around the copy itself and restored from the exact value the handler entered
//! with; the copy helpers are the only place that touches it.
//!
//! Unknown numbers are `-ENOSYS` and are counted, so the userland side can tell
//! "old kernel" from "bad arguments" (the ABI document promises this).
//!
//! # Path mutations
//!
//! The directory-mutating calls (`mkdirat`, `unlinkat`, `symlinkat`, `linkat`,
//! `renameat2`, `fchmodat`) share one shape: read the path through
//! [`read_user_path`], split it with [`vfs::parent_and_leaf`], resolve the parent
//! with [`vfs::walk`] (`follow_final = true` — a parent may be reached through a
//! link) and then call the mutation on that parent directory. The final component
//! is never followed for `unlinkat`/`linkat` (that is what makes `rm link` remove
//! the link, not its target); `fchmodat` follows it like `chmod` does. Every
//! mutation is a single VFS call that the filesystem commits transactionally, so
//! an error leaves the tree as it was — the one exception is handled explicitly
//! in [`openat`], where a created file is removed again if the descriptor cannot
//! be installed.

use core::sync::atomic::{AtomicU64, Ordering};

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::arch::riscv64::csr;
use crate::arch::riscv64::mmu::{PTE_A, PTE_D, PTE_R, PTE_U, PTE_V, PTE_W, PTE_X};
use crate::arch::riscv64::traps::TrapFrame;
use crate::memory::{paging, FRAME_SIZE};
use crate::net::tcp;
use crate::sync::Spinlock;
use crate::user::process::MAX_PROCESSES;
use crate::user::{MmapError, USER_MMAP_PAGES};
use crate::vfs::{self, VfsError, VfsNode};

// ─── syscall numbers (RISC-V Linux generic, per the ABI document) ──────────
const SYS_POLL: usize = 7;
const SYS_DUP: usize = 23;
/// RISC-V asm-generic calls number 24 `dup3`; pagh answers it with the `dup2`
/// rule (`oldfd == newfd` is a successful no-op) and accepts only `flags = 0`,
/// which is exactly `dup2` for every caller that exists today. See
/// `pagh-api/docs/abi.md` §2.
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
const SYS_EXIT: usize = 93;
const SYS_EXIT_GROUP: usize = 94;
const SYS_SET_TID_ADDRESS: usize = 96;
const SYS_FUTEX: usize = 98;
// POSIX-style signals. Numbers are the RISC-V generic table; the structures
// are pagh's own (see `pagh-api/docs/abi.md` and `src/user/signal.rs`).
const SYS_KILL: usize = 129;
const SYS_TGKILL: usize = 131;
const SYS_SIGALTSTACK: usize = 132;
const SYS_RT_SIGACTION: usize = 134;
const SYS_RT_SIGPROCMASK: usize = 135;
const SYS_RT_SIGRETURN: usize = 139;
const SYS_SCHED_YIELD: usize = 124;
const SYS_GETPID: usize = 172;
const SYS_GETPPID: usize = 173;
const SYS_GETTID: usize = 178;
// Process family. `execve`/`wait4`/`getppid` use the asm-generic numbers.
// asm-generic has **no** fork; `fork = 1000` is a pagh-native extension outside
// the Linux table, and `clone(220)` with all-zero arguments is accepted as its
// alias so a libc-shaped caller does not need to know the private number.
const SYS_EXECVE: usize = 221;
const SYS_WAIT4: usize = 260;
const SYS_FORK: usize = 1000;
// Native threads (pagh's own signature, not Linux's raw `clone`):
// `clone(flags, stack, entry, arg)`. See `clone` for the accepted bits.
const SYS_CLONE: usize = 220;
// The directory mutators (path family). Numbers follow the generic RISC-V table
// like the rest of this layer; the semantics are pagh's own and are frozen in
// `pagh-api/docs/abi.md`.
const SYS_MKDIRAT: usize = 34;
const SYS_UNLINKAT: usize = 35;
const SYS_SYMLINKAT: usize = 36;
const SYS_LINKAT: usize = 37;
const SYS_FCHMODAT: usize = 53;
const SYS_FTRUNCATE: usize = 46;
const SYS_RENAMEAT2: usize = 276;
// The BSD-socket subset. Numbers are the generic RISC-V table: 198 socket,
// 200 bind, 201 listen, 202 accept, 203 connect, 206 sendto, 207 recvfrom,
// 208/209 setsockopt/getsockopt, 210 shutdown. Linux's `send`/`recv` are libc wrappers
// around 206/207, so those two carry the full
// `(fd, buf, len, flags, addr, addrlen)` signature; `read`/`write` are routed
// to the same socket paths.
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
// `getrandom(278)` keeps its Linux/asm-generic number: the signature and the
// "may return a short read" contract are the same, and there is no reason to
// invent a private number for a call every runtime already knows.
const SYS_GETRANDOM: usize = 278;
// The userland TLS family. TLS has no Linux syscall to borrow a number from, so
// these are pagh-native extensions outside the asm-generic table (like
// `fork = 1000`), in a block of their own so a reader can see they are ours.
// See `docs/abi.md` §2 for the signatures and `net::tls` for the implementation.
const SYS_TLS_CONNECT: usize = 1100;
const SYS_TLS_WRITE: usize = 1101;
const SYS_TLS_READ: usize = 1102;
const SYS_TLS_CLOSE: usize = 1103;

// ─── errno values (negated in `a0`) ────────────────────────────────────────
const ENOENT: isize = -2;
const EINTR: isize = -4;
const EIO: isize = -5;
const EBADF: isize = -9;
const EAGAIN: isize = -11;
const ENOMEM: isize = -12;
const EFAULT: isize = -14;
const EEXIST: isize = -17;
const ENOTDIR: isize = -20;
const EISDIR: isize = -21;
const EINVAL: isize = -22;
const ENOSPC: isize = -28;
const EPIPE: isize = -32;
const EMSGSIZE: isize = -90;
const EADDRINUSE: isize = -98;
const EADDRNOTAVAIL: isize = -99;
const EDESTADDRREQ: isize = -89;
const ENAMETOOLONG: isize = -36;
const EOVERFLOW: isize = -75;
const EPROTO: isize = -71;
/// The server certificate was rejected (chain, SAN, clock or signature). Linux's
/// "key was rejected by service"; chosen so a caller can tell a trust refusal
/// from a broken handshake.
const EKEYREJECTED: isize = -129;
const ENOSYS: isize = -38;
const ENOTEMPTY: isize = -39;
const ENOTSUP: isize = -95;
const ENOTSOCK: isize = -88;
const ENOPROTOOPT: isize = -92;
const EAFNOSUPPORT: isize = -97;
const ENETUNREACH: isize = -101;
const ECONNRESET: isize = -104;
const EISCONN: isize = -106;
const ENOTCONN: isize = -107;
const ETIMEDOUT: isize = -110;
const ECONNREFUSED: isize = -111;
const EHOSTUNREACH: isize = -113;

// ─── `openat` flags (the ABI's own small set) ──────────────────────────────
const O_ACCMODE: usize = 0x3;
const O_RDONLY: usize = 0;
const O_WRONLY: usize = 1;
const O_RDWR: usize = 2;
const O_CREAT: usize = 0x40;
const O_TRUNC: usize = 0x80;
const O_APPEND: usize = 0x100;

// ─── `unlinkat` flags (the ABI's own small set) ────────────────────────────
/// `unlinkat` flag: remove a directory (must be empty) instead of a file.
const AT_REMOVEDIR: usize = 0x200;

// ─── `mmap`/`mprotect` protection and flags ────────────────────────────────
const PROT_READ: usize = 1;
const PROT_WRITE: usize = 2;
const PROT_EXEC: usize = 4;
const MAP_SHARED: usize = 1;
const MAP_PRIVATE: usize = 2;
const MAP_ANONYMOUS: usize = 0x20;
const MAP_FIXED: usize = 0x10;

// ─── `poll` events (Linux values; the ABI documents this small set) ────────
const POLLIN: i16 = 0x1;
const POLLOUT: i16 = 0x4;
const POLLERR: i16 = 0x8;
const POLLHUP: i16 = 0x10;
const POLLNVAL: i16 = 0x20;

/// Most `pollfd`s one `poll`/`ppoll` call may pass. The array is copied into a
/// fixed kernel buffer, so this is a resource limit, not a policy.
const MAX_POLL_FDS: usize = 64;
/// `struct pollfd`: `i32 fd`, `i16 events`, `i16 revents`, no padding.
const POLLFD_SIZE: usize = 8;

// ─── pipes ─────────────────────────────────────────────────────────────────
/// Bytes in one pipe ring. Fixed and small on purpose: this is a signalling
/// buffer between two descriptors, not a socket.
const PIPE_CAPACITY: usize = 4096;
/// Pipes the kernel may hold open at once (shared by all processes through
/// descriptor references). Each is 4 KiB of static RAM; eight is enough for a
/// shell pipeline and bounded.
const MAX_PIPES: usize = 8;

/// File descriptors per process, including 0/1/2 (the ABI's limit).
pub(crate) const MAX_FDS: usize = 16;

/// Longest absolute path accepted by a path-taking syscall, including the NUL.
/// ext2 limits a name to 255 bytes; a longer *path* is refused with
/// `-ENAMETOOLONG` rather than truncated.
const PATH_CAPACITY: usize = 256;

/// Bytes copied from/to a user page per chunk. Small enough for the trap stack,
/// large enough that the UART, not the copy, is the cost.
const COPY_CHUNK: usize = 256;

/// Most bytes one `getrandom` call returns. A larger request is answered with
/// this many bytes and a short count — Linux's contract for `getrandom` beyond
/// 256 bytes — which also keeps the user-range walk (one step per page) bounded
/// by a constant instead of by the caller's `len`.
const MAX_GETRANDOM: usize = 64 * 1024;

/// Counters the selftest reads as evidence that the ABI actually ran.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Successful `write`s to the console (fd 1/2).
    pub writes: u64,
    /// Bytes handed to the console by those writes.
    pub bytes: u64,
    /// `sched_yield` calls that returned.
    pub yields: u64,
    /// `getpid` calls.
    pub getpids: u64,
    /// `read` calls that returned bytes.
    pub reads: u64,
    /// Bytes delivered to user buffers by `read`.
    pub read_bytes: u64,
    /// `openat` calls that returned a descriptor.
    pub opens: u64,
    /// `close` calls that released a descriptor.
    pub closes: u64,
    /// `lseek` calls that moved an offset.
    pub seeks: u64,
    /// `fstat` calls that filled a `Stat`.
    pub fstats: u64,
    /// Directory records delivered by `getdents64`.
    pub dirents: u64,
    /// `brk` calls that returned a break (including the query).
    pub brks: u64,
    /// `clock_gettime` calls that filled a `Timespec`.
    pub clocks: u64,
    /// `nanosleep` calls that returned.
    pub sleeps: u64,
    /// `uname` calls that filled the `Uname`.
    pub unames: u64,
    /// `readlinkat` calls that returned a target.
    pub readlinks: u64,
    /// `mkdirat` calls that created a directory.
    pub mkdirs: u64,
    /// `unlinkat` calls that removed a file, link or empty directory.
    pub unlinks: u64,
    /// `symlinkat` calls that created a symbolic link.
    pub symlinks: u64,
    /// `linkat` calls that created a hard link.
    pub links: u64,
    /// `renameat2` calls that moved or renamed an entry.
    pub renames: u64,
    /// `fchmodat` calls that recorded a mode override.
    pub chmods: u64,
    /// `ftruncate` calls that resized a file.
    pub truncates: u64,
    /// `mmap` calls that returned a user address.
    pub mmaps: u64,
    /// `munmap` calls that removed tracked pages.
    pub munmaps: u64,
    /// `mprotect` calls that rewrote page permissions.
    pub mprotects: u64,
    /// `pipe2` calls that created a pipe.
    pub pipes: u64,
    /// `dup`/`dup2`/`dup3` calls that returned a descriptor.
    pub dups: u64,
    /// `poll`/`ppoll` calls that returned to user mode.
    pub polls: u64,
    /// `socket()` calls that returned a descriptor.
    pub sockets: u64,
    /// `connect` calls that completed the handshake.
    pub connects: u64,
    /// `listen(201)` calls that put a socket into the listening state.
    pub listens: u64,
    /// `accept(202)` calls that returned a connection.
    pub accepts: u64,
    /// `send`/`sendto`/`write` calls on a socket that handed bytes to TCP.
    pub socket_sends: u64,
    /// `recv`/`recvfrom`/`read` calls on a socket that returned bytes.
    pub socket_recvs: u64,
    /// `shutdown` calls that closed the connection.
    pub shutdowns: u64,
    /// `setsockopt`/`getsockopt` calls that succeeded.
    pub sockopts: u64,
    /// `clone` calls that returned a child tid.
    pub clones: u64,
    /// `futex` `WAIT` calls that reached the block (including timeouts).
    pub futex_waits: u64,
    /// `futex` `WAKE` calls that returned.
    pub futex_wakes: u64,
    /// `exit_group` calls (each ends one process).
    pub exit_groups: u64,
    /// Successful `fork` calls (including the `clone(0,0,0,0)` form).
    pub forks: u64,
    /// Successful `execve` calls.
    pub execs: u64,
    /// `wait4` calls that returned a child pid.
    pub waits: u64,
    /// `getppid` calls.
    pub getppids: u64,
    /// `tls_connect` calls that completed a handshake (1100).
    pub tls_connects: u64,
    /// `tls_read` calls that returned plaintext (1102).
    pub tls_reads: u64,
    /// `tls_write` calls that handed plaintext to the record layer (1101).
    pub tls_writes: u64,
    /// `tls_close` calls that released a session (1103).
    pub tls_closes: u64,
    /// `getrandom` calls that filled a user buffer.
    pub getrandoms: u64,
    /// Bytes handed to userland by `getrandom`.
    pub random_bytes: u64,
    /// Unknown syscall numbers answered with `-ENOSYS`.
    pub unknown: u64,
    /// Bad user buffers answered with `-EFAULT`.
    pub faults: u64,
}

static WRITES: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);
static YIELDS: AtomicU64 = AtomicU64::new(0);
static GETPIDS: AtomicU64 = AtomicU64::new(0);
static READS: AtomicU64 = AtomicU64::new(0);
static READ_BYTES: AtomicU64 = AtomicU64::new(0);
static OPENS: AtomicU64 = AtomicU64::new(0);
static CLOSES: AtomicU64 = AtomicU64::new(0);
static SEEKS: AtomicU64 = AtomicU64::new(0);
static FSTATS: AtomicU64 = AtomicU64::new(0);
static DIRENTS: AtomicU64 = AtomicU64::new(0);
static BRKS: AtomicU64 = AtomicU64::new(0);
static CLOCKS: AtomicU64 = AtomicU64::new(0);
static SLEEPS: AtomicU64 = AtomicU64::new(0);
static UNAMES: AtomicU64 = AtomicU64::new(0);
static READLINKS: AtomicU64 = AtomicU64::new(0);
static MKDIRS: AtomicU64 = AtomicU64::new(0);
static UNLINKS: AtomicU64 = AtomicU64::new(0);
static SYMLINKS: AtomicU64 = AtomicU64::new(0);
static LINKS: AtomicU64 = AtomicU64::new(0);
static RENAMES: AtomicU64 = AtomicU64::new(0);
static CHMODS: AtomicU64 = AtomicU64::new(0);
static TRUNCATES: AtomicU64 = AtomicU64::new(0);
static MMAPS: AtomicU64 = AtomicU64::new(0);
static MUNMAPS: AtomicU64 = AtomicU64::new(0);
static MPROTECTS: AtomicU64 = AtomicU64::new(0);
static PIPE_CREATES: AtomicU64 = AtomicU64::new(0);
static DUPS: AtomicU64 = AtomicU64::new(0);
static POLLS: AtomicU64 = AtomicU64::new(0);
static SOCKETS: AtomicU64 = AtomicU64::new(0);
static CONNECTS: AtomicU64 = AtomicU64::new(0);
static LISTENS: AtomicU64 = AtomicU64::new(0);
static ACCEPTS: AtomicU64 = AtomicU64::new(0);
static SOCKET_SENDS: AtomicU64 = AtomicU64::new(0);
static SOCKET_RECVS: AtomicU64 = AtomicU64::new(0);
static SHUTDOWNS: AtomicU64 = AtomicU64::new(0);
static SOCKOPTS: AtomicU64 = AtomicU64::new(0);
static CLONES: AtomicU64 = AtomicU64::new(0);
static FUTEX_WAITS: AtomicU64 = AtomicU64::new(0);
static FUTEX_WAKES: AtomicU64 = AtomicU64::new(0);
static EXIT_GROUPS: AtomicU64 = AtomicU64::new(0);
static FORKS: AtomicU64 = AtomicU64::new(0);
static EXECS: AtomicU64 = AtomicU64::new(0);
static WAITS: AtomicU64 = AtomicU64::new(0);
static GETPPIDS: AtomicU64 = AtomicU64::new(0);
static TLS_CONNECTS: AtomicU64 = AtomicU64::new(0);
static TLS_READS: AtomicU64 = AtomicU64::new(0);
static TLS_WRITES: AtomicU64 = AtomicU64::new(0);
static TLS_CLOSES: AtomicU64 = AtomicU64::new(0);
static GETRANDOMS: AtomicU64 = AtomicU64::new(0);
static RANDOM_BYTES: AtomicU64 = AtomicU64::new(0);
static UNKNOWN: AtomicU64 = AtomicU64::new(0);
static FAULTS: AtomicU64 = AtomicU64::new(0);

/// A snapshot of the syscall counters (all relaxed: they are evidence, not
/// synchronisation).
pub fn stats() -> Stats {
    Stats {
        writes: WRITES.load(Ordering::Relaxed),
        bytes: BYTES.load(Ordering::Relaxed),
        yields: YIELDS.load(Ordering::Relaxed),
        getpids: GETPIDS.load(Ordering::Relaxed),
        reads: READS.load(Ordering::Relaxed),
        read_bytes: READ_BYTES.load(Ordering::Relaxed),
        opens: OPENS.load(Ordering::Relaxed),
        closes: CLOSES.load(Ordering::Relaxed),
        seeks: SEEKS.load(Ordering::Relaxed),
        fstats: FSTATS.load(Ordering::Relaxed),
        dirents: DIRENTS.load(Ordering::Relaxed),
        brks: BRKS.load(Ordering::Relaxed),
        clocks: CLOCKS.load(Ordering::Relaxed),
        sleeps: SLEEPS.load(Ordering::Relaxed),
        unames: UNAMES.load(Ordering::Relaxed),
        readlinks: READLINKS.load(Ordering::Relaxed),
        mkdirs: MKDIRS.load(Ordering::Relaxed),
        unlinks: UNLINKS.load(Ordering::Relaxed),
        symlinks: SYMLINKS.load(Ordering::Relaxed),
        links: LINKS.load(Ordering::Relaxed),
        renames: RENAMES.load(Ordering::Relaxed),
        chmods: CHMODS.load(Ordering::Relaxed),
        truncates: TRUNCATES.load(Ordering::Relaxed),
        mmaps: MMAPS.load(Ordering::Relaxed),
        munmaps: MUNMAPS.load(Ordering::Relaxed),
        mprotects: MPROTECTS.load(Ordering::Relaxed),
        pipes: PIPE_CREATES.load(Ordering::Relaxed),
        dups: DUPS.load(Ordering::Relaxed),
        polls: POLLS.load(Ordering::Relaxed),
        sockets: SOCKETS.load(Ordering::Relaxed),
        connects: CONNECTS.load(Ordering::Relaxed),
        listens: LISTENS.load(Ordering::Relaxed),
        accepts: ACCEPTS.load(Ordering::Relaxed),
        socket_sends: SOCKET_SENDS.load(Ordering::Relaxed),
        socket_recvs: SOCKET_RECVS.load(Ordering::Relaxed),
        shutdowns: SHUTDOWNS.load(Ordering::Relaxed),
        sockopts: SOCKOPTS.load(Ordering::Relaxed),
        clones: CLONES.load(Ordering::Relaxed),
        futex_waits: FUTEX_WAITS.load(Ordering::Relaxed),
        futex_wakes: FUTEX_WAKES.load(Ordering::Relaxed),
        exit_groups: EXIT_GROUPS.load(Ordering::Relaxed),
        forks: FORKS.load(Ordering::Relaxed),
        execs: EXECS.load(Ordering::Relaxed),
        waits: WAITS.load(Ordering::Relaxed),
        getppids: GETPPIDS.load(Ordering::Relaxed),
        tls_connects: TLS_CONNECTS.load(Ordering::Relaxed),
        tls_reads: TLS_READS.load(Ordering::Relaxed),
        tls_writes: TLS_WRITES.load(Ordering::Relaxed),
        tls_closes: TLS_CLOSES.load(Ordering::Relaxed),
        getrandoms: GETRANDOMS.load(Ordering::Relaxed),
        random_bytes: RANDOM_BYTES.load(Ordering::Relaxed),
        unknown: UNKNOWN.load(Ordering::Relaxed),
        faults: FAULTS.load(Ordering::Relaxed),
    }
}

/// Count a bad user pointer and hand back the ABI's answer.
pub(crate) fn fault() -> isize {
    FAULTS.fetch_add(1, Ordering::Relaxed);
    EFAULT
}

// ─── descriptor table ──────────────────────────────────────────────────────

/// One open file: the node (shared with the VFS), the read/write offset and the
/// `openat` flags. `Arc` keeps the node alive after the mount table is replaced,
/// which is what `close` releases.
#[derive(Clone)]
struct FileHandle {
    node: Arc<dyn VfsNode>,
    offset: u64,
    flags: usize,
    /// Directory children captured at open time, with the `getdents64` cursor.
    /// `Some` exactly when the node is a directory and was opened read-only.
    dir: Option<DirSnapshot>,
}

/// A directory's entries and how many records have been returned so far.
///
/// The VFS has no seekable directory cursor, so the syscall layer snapshots the
/// children at `openat` (the same thing the x86 shell does) and walks the
/// snapshot. A directory that changes while it is open is not reflected in the
/// stream; that is the v1 contract.
#[derive(Clone)]
struct DirSnapshot {
    entries: Vec<Arc<dyn VfsNode>>,
    index: usize,
}

// ─── pipes ─────────────────────────────────────────────────────────────────

/// One fixed 4 KiB ring shared by every descriptor that names it.
///
/// `readers`/`writers` are descriptor-reference counts, not processes: a `dup`
/// increments the matching one and `close` decrements it. EOF is `writers == 0`
/// with an empty ring; a write with no readers is `-EPIPE`; the storage is
/// released when both reach zero, which is also what makes the pipe table
/// idempotent across a `selftest` pass (every run resets it).
struct Pipe {
    in_use: bool,
    data: [u8; PIPE_CAPACITY],
    /// Index of the oldest byte.
    head: usize,
    /// Bytes currently stored (`0..=PIPE_CAPACITY`).
    len: usize,
    readers: usize,
    writers: usize,
}

impl Pipe {
    const fn empty() -> Self {
        Pipe {
            in_use: false,
            data: [0; PIPE_CAPACITY],
            head: 0,
            len: 0,
            readers: 0,
            writers: 0,
        }
    }

    fn free(&self) -> usize {
        PIPE_CAPACITY - self.len
    }

    /// Put the ring back into the free state without moving 4 KiB of data.
    ///
    /// The buffer is left as it is: `in_use = false` and `len = 0` make it
    /// unreadable, and a later `create` overwrites bytes as they are written. A
    /// `*self = Pipe::empty()` would build a 4 KiB temporary on the task stack,
    /// which is exactly the kind of stack overflow the boot stack has paid for
    /// before.
    fn reset(&mut self) {
        self.in_use = false;
        self.head = 0;
        self.len = 0;
        self.readers = 0;
        self.writers = 0;
    }
}

struct PipeTable {
    pipes: [Pipe; MAX_PIPES],
}

impl PipeTable {
    const fn empty() -> Self {
        PipeTable {
            pipes: [const { Pipe::empty() }; MAX_PIPES],
        }
    }

    /// Reserve a free pipe with one reader and one writer reference.
    fn create(&mut self) -> Option<usize> {
        for (id, pipe) in self.pipes.iter_mut().enumerate() {
            if !pipe.in_use {
                pipe.reset();
                pipe.in_use = true;
                pipe.readers = 1;
                pipe.writers = 1;
                return Some(id);
            }
        }
        None
    }

    /// Drop one descriptor reference; both ends gone frees the ring.
    fn release(&mut self, id: usize, read: bool) {
        let Some(pipe) = self.pipes.get_mut(id) else {
            return;
        };
        if !pipe.in_use {
            return;
        }
        if read {
            pipe.readers = pipe.readers.saturating_sub(1);
        } else {
            pipe.writers = pipe.writers.saturating_sub(1);
        }
        if pipe.readers == 0 && pipe.writers == 0 {
            pipe.reset();
        }
    }

    fn retain(&mut self, id: usize, read: bool) {
        let Some(pipe) = self.pipes.get_mut(id) else {
            return;
        };
        if !pipe.in_use {
            return;
        }
        if read {
            pipe.readers += 1;
        } else {
            pipe.writers += 1;
        }
    }
}

static PIPES: Spinlock<PipeTable> = Spinlock::new(PipeTable::empty());

/// Add one descriptor reference to a pipe (the `dup` side of the lifetime).
fn pipe_retain(id: usize, read: bool) {
    PIPES.lock().retain(id, read);
}

/// Drop one descriptor reference to a pipe.
fn pipe_release(id: usize, read: bool) {
    PIPES.lock().release(id, read);
}

/// Pipes currently allocated. The selftest reads this after a run to prove
/// every end was closed; it is not part of the user ABI.
pub(crate) fn live_pipes() -> usize {
    PIPES.lock().pipes.iter().filter(|pipe| pipe.in_use).count()
}

enum FdEntry {
    /// fd 0: console input (UART ring, then the virtio keyboard).
    Stdin,
    /// fd 1/2: console output.
    Stdout,
    File(FileHandle),
    /// One end of a pipe; `read` selects the direction. The ring is shared by
    /// every descriptor with the same `id`.
    Pipe {
        id: usize,
        read: bool,
    },
    /// A BSD-style stream socket descriptor that is not yet a listener or an
    /// accepted connection. At most one exists in a live process (the ABI's
    /// "one active-open client" rule); `id` is the stack connection once a
    /// `connect` succeeds, `bound` is the local port reserved by `bind(200)`
    /// (0 when unbound), and `nonblocking` is inherited by a later listener
    /// and makes its `accept` answer `-EAGAIN` immediately.
    Socket {
        id: u8,
        connected: bool,
        bound: u16,
        nonblocking: bool,
    },
    /// A listening stream socket (`listen(201)`). The stack has one listener;
    /// `port` is the reserved local port and `nonblocking` controls `accept`.
    Listener {
        port: u16,
        nonblocking: bool,
    },
    /// One accepted stream connection (`accept(202)`). Its `id` names a slot in
    /// the TCP table, which makes several accepted connections possible.
    TcpConn {
        id: u8,
    },
    /// A descriptor with a TLS session on it (`tls_connect`, 1100). `session`
    /// names a slot in `net::tls`'s userland session table and `tcp` the TCP
    /// connection the session encrypts onto; the session owns the connection,
    /// and releasing the descriptor releases both. Reads and writes on this
    /// descriptor are plaintext: the record layer is below the syscall ABI.
    Tls {
        session: u8,
        tcp: u8,
    },
    /// A userland UDP socket. `port == 0` is unbound; `bind` sets it, and the
    /// first `sendto` auto-binds an ephemeral port. `nonblocking` is the
    /// `SOCK_NONBLOCK` bit from `socket()`, so `recvfrom` can answer `-EAGAIN`
    /// instead of polling for a datagram.
    UdpSocket {
        port: u16,
        nonblocking: bool,
    },
    /// A forked child's copy of a parent socket descriptor. It is not a second
    /// owner of the connection/binding; operations answer `-ENOTSUP`.
    Unsupported,
}

impl Clone for FdEntry {
    fn clone(&self) -> Self {
        match self {
            FdEntry::Stdin => FdEntry::Stdin,
            FdEntry::Stdout => FdEntry::Stdout,
            FdEntry::File(file) => FdEntry::File(file.clone()),
            FdEntry::Pipe { id, read } => {
                pipe_retain(*id, *read);
                FdEntry::Pipe {
                    id: *id,
                    read: *read,
                }
            }
            // A dup/fork of a socket would be a second owner of one
            // connection or binding; without a reference count, closing either
            // copy would tear the shared state down under the other. The
            // child/copy gets the same tombstone UDP has always used. A TLS
            // session is in the same position, only more so: it owns heap
            // buffers and a key schedule.
            FdEntry::Socket { .. }
            | FdEntry::Listener { .. }
            | FdEntry::TcpConn { .. }
            | FdEntry::Tls { .. }
            | FdEntry::UdpSocket { .. } => FdEntry::Unsupported,
            FdEntry::Unsupported => FdEntry::Unsupported,
        }
    }
}

struct FdTable {
    slots: [Option<FdEntry>; MAX_FDS],
}

impl FdTable {
    const fn empty() -> Self {
        FdTable {
            slots: [const { None }; MAX_FDS],
        }
    }

    fn console() -> Self {
        let mut table = FdTable::empty();
        table.slots[0] = Some(FdEntry::Stdin);
        table.slots[1] = Some(FdEntry::Stdout);
        table.slots[2] = Some(FdEntry::Stdout);
        table
    }
}

/// Every process's descriptor row. A process owns its table: `fork` copies rows,
/// `execve` keeps them, teardown drops one row. Pipe rings stay global and
/// reference-counted, which is the one resource a fork shares.
struct FdTables {
    rows: [FdTable; MAX_PROCESSES],
}

impl FdTables {
    const fn empty() -> Self {
        FdTables {
            rows: [const { FdTable::empty() }; MAX_PROCESSES],
        }
    }
}

static FDS: Spinlock<FdTables> = Spinlock::new(FdTables::empty());

/// Run `f` on the calling process's descriptor row. `None` when the caller is
/// not a user process (a stale syscall after teardown).
fn with_fds<R>(f: impl FnOnce(&mut FdTable) -> R) -> Option<R> {
    let slot = crate::user::process::current_slot()?;
    let mut tables = FDS.lock();
    Some(f(&mut *tables.rows.get_mut(slot)?))
}

/// Fill one process's row with the three console descriptors. Called before the
/// process's first task can issue an `ecall`.
pub(crate) fn reset_fds(slot: usize) {
    let mut udp_ports = [0u16; MAX_FDS];
    let mut udp_count = 0usize;
    let mut conn_ids = [SOCKET_NONE; MAX_FDS];
    let mut conn_count = 0usize;
    let mut bound_ports = [0u16; MAX_FDS];
    let mut bound_count = 0usize;
    let mut tls_sessions = [0u8; MAX_FDS];
    let mut tls_count = 0usize;
    let mut had_listener = false;
    {
        let mut tables = FDS.lock();
        for entry in tables.rows[slot].slots.iter() {
            match entry {
                Some(FdEntry::Socket {
                    id,
                    connected,
                    bound,
                    ..
                }) => {
                    if *connected && conn_count < conn_ids.len() {
                        conn_ids[conn_count] = *id;
                        conn_count += 1;
                    } else if *bound != 0 && bound_count < bound_ports.len() {
                        bound_ports[bound_count] = *bound;
                        bound_count += 1;
                    }
                }
                Some(FdEntry::Listener { .. }) => had_listener = true,
                Some(FdEntry::TcpConn { id }) => {
                    if conn_count < conn_ids.len() {
                        conn_ids[conn_count] = *id;
                        conn_count += 1;
                    }
                }
                // A TLS session owns its TCP connection: the session goes first
                // (its buffers are freed there and no alert is attempted — this
                // is teardown, not a graceful close), then the connection.
                Some(FdEntry::Tls { session, tcp }) => {
                    if tls_count < tls_sessions.len() {
                        tls_sessions[tls_count] = *session;
                        tls_count += 1;
                    }
                    if conn_count < conn_ids.len() {
                        conn_ids[conn_count] = *tcp;
                        conn_count += 1;
                    }
                }
                Some(FdEntry::UdpSocket { port, .. }) if *port != 0 => {
                    if udp_count < udp_ports.len() {
                        udp_ports[udp_count] = *port;
                        udp_count += 1;
                    }
                }
                Some(_) | None => {}
            }
        }
        tables.rows[slot] = FdTable::console();
    }
    for &session in tls_sessions[..tls_count].iter() {
        crate::net::tls::user_close(session, false);
    }
    for &port in udp_ports[..udp_count].iter() {
        crate::net::udp_unbind(port);
    }
    for &port in bound_ports[..bound_count].iter() {
        crate::net::tcp::release_binding(port);
    }
    for &id in conn_ids[..conn_count].iter() {
        crate::net::tcp::close_id(id);
    }
    if had_listener {
        crate::net::tcp::close_listener();
    }
    if let Some(row) = MODES.lock().rows.get_mut(slot) {
        *row = ModeTable::empty();
    }
}

/// Drop one process's open nodes and pipe references. A socket close sends a
/// FIN, so the table lock is released first.
pub(crate) fn release_fds(slot: usize) {
    // Collect the resources to release outside the table lock: a socket close
    // sends a FIN and may poll the NIC, and a pipe reference is a second lock
    // this path does not need to hold while doing it.
    let mut pipes = [(0usize, false); MAX_FDS];
    let mut pipe_count = 0usize;
    let mut udp_ports = [0u16; MAX_FDS];
    let mut udp_count = 0usize;
    let mut conn_ids = [SOCKET_NONE; MAX_FDS];
    let mut conn_count = 0usize;
    let mut bound_ports = [0u16; MAX_FDS];
    let mut bound_count = 0usize;
    let mut tls_sessions = [0u8; MAX_FDS];
    let mut tls_count = 0usize;
    let mut had_listener = false;
    {
        let mut tables = FDS.lock();
        let Some(row) = tables.rows.get_mut(slot) else {
            return;
        };
        for entry in row.slots.iter_mut() {
            match entry.take() {
                Some(FdEntry::Pipe { id, read }) => {
                    if pipe_count < pipes.len() {
                        pipes[pipe_count] = (id, read);
                        pipe_count += 1;
                    }
                }
                Some(FdEntry::Socket {
                    id,
                    connected,
                    bound,
                    ..
                }) => {
                    if connected && conn_count < conn_ids.len() {
                        conn_ids[conn_count] = id;
                        conn_count += 1;
                    } else if bound != 0 && bound_count < bound_ports.len() {
                        bound_ports[bound_count] = bound;
                        bound_count += 1;
                    }
                }
                Some(FdEntry::Listener { .. }) => had_listener = true,
                Some(FdEntry::TcpConn { id }) => {
                    if conn_count < conn_ids.len() {
                        conn_ids[conn_count] = id;
                        conn_count += 1;
                    }
                }
                Some(FdEntry::Tls { session, tcp }) => {
                    if tls_count < tls_sessions.len() {
                        tls_sessions[tls_count] = session;
                        tls_count += 1;
                    }
                    if conn_count < conn_ids.len() {
                        conn_ids[conn_count] = tcp;
                        conn_count += 1;
                    }
                }
                Some(FdEntry::UdpSocket { port, .. }) if port != 0 => {
                    if udp_count < udp_ports.len() {
                        udp_ports[udp_count] = port;
                        udp_count += 1;
                    }
                }
                Some(_) | None => {}
            }
        }
    }
    for &(id, read) in pipes[..pipe_count].iter() {
        pipe_release(id, read);
    }
    // Sessions before connections: the session owns the connection's TLS state
    // and its record buffers, and releasing it does not touch the socket.
    for &session in tls_sessions[..tls_count].iter() {
        crate::net::tls::user_close(session, false);
    }
    for &port in udp_ports[..udp_count].iter() {
        crate::net::udp_unbind(port);
    }
    for &port in bound_ports[..bound_count].iter() {
        crate::net::tcp::release_binding(port);
    }
    for &id in conn_ids[..conn_count].iter() {
        crate::net::tcp::close_id(id);
    }
    if had_listener {
        crate::net::tcp::close_listener();
    }
    if let Some(row) = MODES.lock().rows.get_mut(slot) {
        *row = ModeTable::empty();
    }
}

/// Copy a parent's descriptor row into a child's.
///
/// Files and pipes are cloned (the pipe's ring gains one reference for each end);
/// every socket descriptor (active client, listener, accepted connection or UDP
/// port) is replaced by [`FdEntry::Unsupported`] because a child/copy must not
/// become a second owner of the kernel's connection state. A later
/// `read`/`write`/`poll` on that descriptor is `-ENOTSUP`, and `close` simply
/// drops it.
pub(crate) fn fork_fds(parent: usize, child: usize) -> bool {
    let mut tables = FDS.lock();
    if parent == child || parent >= MAX_PROCESSES || child >= MAX_PROCESSES {
        return false;
    }
    let mut copy: [Option<FdEntry>; MAX_FDS] = [const { None }; MAX_FDS];
    for (index, entry) in tables.rows[parent].slots.iter().enumerate() {
        copy[index] = match entry {
            Some(FdEntry::Socket { .. })
            | Some(FdEntry::Listener { .. })
            | Some(FdEntry::TcpConn { .. })
            | Some(FdEntry::Tls { .. })
            | Some(FdEntry::UdpSocket { .. }) => Some(FdEntry::Unsupported),
            Some(entry) => Some(entry.clone()),
            None => None,
        };
    }
    tables.rows[child].slots = copy;
    true
}

/// What a syscall needs from a descriptor, copied out of the table so the lock
/// is not held across a VFS/block call. `ConsoleOut` is also returned for fd 2;
/// the two write to the same console.
enum OpenFd {
    ConsoleIn,
    ConsoleOut,
    File {
        node: Arc<dyn VfsNode>,
        offset: u64,
        flags: usize,
    },
    Pipe {
        id: usize,
        read: bool,
    },
    Socket {
        id: u8,
        connected: bool,
        bound: u16,
        nonblocking: bool,
    },
    Listener {
        port: u16,
        nonblocking: bool,
    },
    TcpConn {
        id: u8,
    },
    /// A descriptor with a TLS session (`tls_connect`, 1100): `session` names
    /// the session in `net::tls`. The TCP connection underneath it is owned by
    /// the session, so nothing above this layer needs its id.
    Tls {
        session: u8,
    },
    UdpSocket {
        port: u16,
        nonblocking: bool,
    },
    /// A descriptor inherited from a parent's socket. The connection/binding has
    /// an owner; this row is a tombstone for the fd number.
    Unsupported,
}

fn lookup(fd: usize) -> Result<OpenFd, isize> {
    with_fds(
        |table| match table.slots.get(fd).and_then(|slot| slot.as_ref()) {
            Some(FdEntry::Stdin) => Ok(OpenFd::ConsoleIn),
            Some(FdEntry::Stdout) => Ok(OpenFd::ConsoleOut),
            Some(FdEntry::File(file)) => Ok(OpenFd::File {
                node: Arc::clone(&file.node),
                offset: file.offset,
                flags: file.flags,
            }),
            Some(FdEntry::Pipe { id, read }) => Ok(OpenFd::Pipe {
                id: *id,
                read: *read,
            }),
            Some(FdEntry::Socket {
                id,
                connected,
                bound,
                nonblocking,
            }) => Ok(OpenFd::Socket {
                id: *id,
                connected: *connected,
                bound: *bound,
                nonblocking: *nonblocking,
            }),
            Some(FdEntry::Listener { port, nonblocking }) => Ok(OpenFd::Listener {
                port: *port,
                nonblocking: *nonblocking,
            }),
            Some(FdEntry::TcpConn { id }) => Ok(OpenFd::TcpConn { id: *id }),
            Some(FdEntry::Tls { session, .. }) => Ok(OpenFd::Tls { session: *session }),
            Some(FdEntry::UdpSocket { port, nonblocking }) => Ok(OpenFd::UdpSocket {
                port: *port,
                nonblocking: *nonblocking,
            }),
            Some(FdEntry::Unsupported) => Ok(OpenFd::Unsupported),
            None => Err(EBADF),
        },
    )
    .unwrap_or(Err(EBADF))
}

/// First empty descriptor slot, including 0/1/2 once a program has closed them.
fn alloc_fd() -> Option<usize> {
    with_fds(|table| table.slots.iter().position(|slot| slot.is_none())).flatten()
}

/// Put an entry in `fd`, returning the previous occupant (released by the
/// caller). `None` means the slot was free or out of range.
fn take_slot(fd: usize) -> Option<FdEntry> {
    with_fds(|table| table.slots.get_mut(fd).and_then(|slot| slot.take())).flatten()
}

/// Release one descriptor entry, dropping a pipe reference if it was one end.
fn drop_entry(entry: FdEntry) {
    match entry {
        FdEntry::Pipe { id, read } => pipe_release(id, read),
        FdEntry::Socket {
            id,
            connected,
            bound,
            ..
        } => {
            if connected {
                with_network_interrupts(|| crate::net::tcp::close_id(id));
            } else if bound != 0 {
                crate::net::tcp::release_binding(bound);
            }
        }
        FdEntry::Listener { .. } => with_network_interrupts(crate::net::tcp::close_listener),
        FdEntry::TcpConn { id } => {
            with_network_interrupts(|| crate::net::tcp::close_id(id));
        }
        // `close(2)` on a TLS descriptor releases the session without a
        // `close_notify`: this path is also process teardown, and neither may
        // block on a peer. `tls_close(1103)` is the graceful close.
        FdEntry::Tls { session, tcp } => {
            crate::net::tls::user_close(session, false);
            with_network_interrupts(|| crate::net::tcp::close_id(tcp));
        }
        FdEntry::UdpSocket { port, .. } => {
            if port != 0 {
                crate::net::udp_unbind(port);
            }
        }
        _ => {}
    }
}

/// Install a pipe end in the first free slot.
fn install_pipe(id: usize, read: bool) -> Result<usize, isize> {
    with_fds(|table| {
        let Some(fd) = table.slots.iter().position(|slot| slot.is_none()) else {
            return Err(ENOSPC);
        };
        table.slots[fd] = Some(FdEntry::Pipe { id, read });
        Ok(fd)
    })
    .unwrap_or(Err(EBADF))
}

/// Publish the new offset of an open file.
fn set_offset(fd: usize, offset: u64) {
    let _ = with_fds(|table| {
        if let Some(Some(FdEntry::File(file))) = table.slots.get_mut(fd) {
            file.offset = offset;
        }
    });
}

/// Install an unconnected stream socket descriptor. The active-open client is
/// still one per kernel, so a second `Socket` row anywhere is refused with
/// `ENOSPC`; listeners and accepted connections are separate variants and do
/// not count.
fn install_socket(nonblocking: bool) -> Result<usize, isize> {
    let slot = crate::user::process::current_slot().ok_or(EBADF)?;
    let mut tables = FDS.lock();
    for row in tables.rows.iter() {
        for entry in row.slots.iter() {
            if matches!(entry, Some(FdEntry::Socket { .. })) {
                return Err(ENOSPC);
            }
        }
    }
    let table = &mut tables.rows[slot];
    for fd in 3..MAX_FDS {
        if table.slots[fd].is_none() {
            table.slots[fd] = Some(FdEntry::Socket {
                id: SOCKET_NONE,
                connected: false,
                bound: 0,
                nonblocking,
            });
            return Ok(fd);
        }
    }
    Err(ENOSPC)
}

/// Install a UDP socket descriptor. Unlike the one TCP connection, any number
/// of datagram sockets may be open; each starts unbound (`port == 0`) and
/// `bind`/the first `sendto` gives it a port.
fn install_udp(nonblocking: bool) -> Result<usize, isize> {
    let slot = crate::user::process::current_slot().ok_or(EBADF)?;
    let mut tables = FDS.lock();
    let table = &mut tables.rows[slot];
    for fd in 3..MAX_FDS {
        if table.slots[fd].is_none() {
            table.slots[fd] = Some(FdEntry::UdpSocket {
                port: 0,
                nonblocking,
            });
            return Ok(fd);
        }
    }
    Err(ENOSPC)
}

/// Publish the local port chosen by `bind` or the first `sendto`.
fn set_udp_port(fd: usize, port: u16) -> bool {
    with_fds(|table| {
        if let Some(Some(FdEntry::UdpSocket { port: slot, .. })) = table.slots.get_mut(fd) {
            *slot = port;
            true
        } else {
            false
        }
    })
    .unwrap_or(false)
}

/// Record that the socket on `fd` completed its handshake and which TCP slot
/// owns the connection.
fn mark_socket_connected(fd: usize, id: u8) {
    let _ = with_fds(|table| {
        if let Some(Some(FdEntry::Socket {
            id: slot,
            connected,
            bound,
            ..
        })) = table.slots.get_mut(fd)
        {
            *connected = true;
            *slot = id;
            *bound = 0;
        }
    });
}

/// Publish the local port chosen by TCP `bind`.
fn set_socket_bound(fd: usize, port: u16) -> bool {
    with_fds(|table| {
        if let Some(Some(FdEntry::Socket { bound, .. })) = table.slots.get_mut(fd) {
            *bound = port;
            true
        } else {
            false
        }
    })
    .unwrap_or(false)
}

/// Turn a bound stream socket into a listener after `listen` succeeded.
fn convert_to_listener(fd: usize, port: u16, nonblocking: bool) -> bool {
    with_fds(|table| match table.slots.get_mut(fd) {
        Some(slot @ Some(FdEntry::Socket { .. })) => {
            *slot = Some(FdEntry::Listener { port, nonblocking });
            true
        }
        _ => false,
    })
    .unwrap_or(false)
}

/// Install one accepted stream connection in the first free descriptor slot.
fn install_tcp_conn(id: u8) -> Result<usize, isize> {
    with_fds(|table| {
        for fd in 3..MAX_FDS {
            if table.slots[fd].is_none() {
                table.slots[fd] = Some(FdEntry::TcpConn { id });
                return Ok(fd);
            }
        }
        Err(ENOSPC)
    })
    .unwrap_or(Err(EBADF))
}

/// Install a node in the first free slot at or above 3.
fn install(node: Arc<dyn VfsNode>, flags: usize, dir: Option<DirSnapshot>) -> Result<usize, isize> {
    with_fds(|table| {
        for fd in 3..MAX_FDS {
            if table.slots[fd].is_none() {
                table.slots[fd] = Some(FdEntry::File(FileHandle {
                    node,
                    offset: 0,
                    flags,
                    dir,
                }));
                return Ok(fd);
            }
        }
        // The ABI's error list does not include EMFILE; ENOSPC is the kernel's
        // honest "no space left" for "all 16 descriptors are open".
        Err(ENOSPC)
    })
    .unwrap_or(Err(EBADF))
}

// ─── permission-mode overrides (fchmodat) ─────────────────────────────────

/// Distinct inodes one process may retarget with `fchmodat`.
const MAX_MODE_OVERRIDES: usize = 16;

#[derive(Clone, Copy)]
struct ModeOverride {
    ino: u64,
    mode: u16,
}

/// `fchmodat` records the new `0o7777` bits here and [`fstat`] reports them for
/// the rest of the process's life; the table is keyed by [`VfsNode::fs_ino`] and
/// deliberately not persisted. One row per process: a child does not inherit
/// chmod overrides any more than it inherits the parent's file descriptors by
/// reference.
struct ModeTable {
    entries: [ModeOverride; MAX_MODE_OVERRIDES],
    count: usize,
}

impl ModeTable {
    const fn empty() -> Self {
        ModeTable {
            entries: [ModeOverride { ino: 0, mode: 0 }; MAX_MODE_OVERRIDES],
            count: 0,
        }
    }

    fn get(&self, ino: u64) -> Option<u16> {
        self.entries[..self.count]
            .iter()
            .find(|entry| entry.ino == ino)
            .map(|entry| entry.mode)
    }

    fn set(&mut self, ino: u64, mode: u16) -> Result<(), isize> {
        if let Some(entry) = self.entries[..self.count]
            .iter_mut()
            .find(|entry| entry.ino == ino)
        {
            entry.mode = mode;
            return Ok(());
        }
        if self.count == MAX_MODE_OVERRIDES {
            return Err(ENOSPC);
        }
        self.entries[self.count] = ModeOverride { ino, mode };
        self.count += 1;
        Ok(())
    }
}

struct ModeTables {
    rows: [ModeTable; MAX_PROCESSES],
}

impl ModeTables {
    const fn empty() -> Self {
        ModeTables {
            rows: [const { ModeTable::empty() }; MAX_PROCESSES],
        }
    }
}

static MODES: Spinlock<ModeTables> = Spinlock::new(ModeTables::empty());

/// The override recorded for `ino` by the calling process.
fn mode_override(ino: u64) -> Option<u16> {
    if ino == 0 {
        return None;
    }
    let slot = crate::user::process::current_slot()?;
    let tables = MODES.lock();
    tables.rows.get(slot)?.get(ino)
}

/// Record an override for the calling process.
fn mode_set(ino: u64, mode: u16) -> Result<(), isize> {
    let slot = crate::user::process::current_slot().ok_or(EBADF)?;
    let mut tables = MODES.lock();
    tables.rows.get_mut(slot).ok_or(EBADF)?.set(ino, mode)
}

// ─── dispatch ──────────────────────────────────────────────────────────────

/// Handle one `ecall` from U-mode. The result is placed in the trap frame's
/// `a0`, which the trap epilogue restores before `sret`.
///
/// `exit` and a bad trap never return here; everything else does, and the same
/// frame carries the answer back to the user instruction after `ecall`.
pub fn dispatch(frame: &mut TrapFrame) {
    let number = frame.a7;
    let result = match number {
        SYS_READ => read(frame.a0, frame.a1, frame.a2),
        SYS_WRITE => write(frame.a0, frame.a1, frame.a2),
        SYS_OPENAT => openat(frame.a0, frame.a1, frame.a2, frame.a3),
        SYS_CLOSE => close(frame.a0),
        SYS_LSEEK => lseek(frame.a0, frame.a1, frame.a2),
        SYS_FSTAT => fstat(frame.a0, frame.a1),
        SYS_GETDENTS64 => getdents64(frame.a0, frame.a1, frame.a2),
        SYS_READLINKAT => readlinkat(frame.a0, frame.a1, frame.a2, frame.a3),
        SYS_MKDIRAT => mkdirat(frame.a0, frame.a1, frame.a2),
        SYS_UNLINKAT => unlinkat(frame.a0, frame.a1, frame.a2),
        // Linux's `symlinkat(target, newdirfd, linkpath)`: the target is the
        // first argument, not the path.
        SYS_SYMLINKAT => symlinkat(frame.a0, frame.a1, frame.a2),
        SYS_LINKAT => linkat(frame.a0, frame.a1, frame.a2, frame.a3, frame.a4),
        SYS_RENAMEAT2 => renameat2(frame.a0, frame.a1, frame.a2, frame.a3, frame.a4),
        SYS_FCHMODAT => fchmodat(frame.a0, frame.a1, frame.a2, frame.a3),
        SYS_FTRUNCATE => ftruncate(frame.a0, frame.a1),
        SYS_MMAP => mmap(frame.a0, frame.a1, frame.a2, frame.a3, frame.a4, frame.a5),
        SYS_MUNMAP => munmap(frame.a0, frame.a1),
        SYS_MPROTECT => mprotect(frame.a0, frame.a1, frame.a2),
        SYS_PIPE2 => pipe2(frame.a0, frame.a1),
        SYS_DUP => dup(frame.a0),
        SYS_DUP2 => dup2(frame.a0, frame.a1, frame.a2),
        SYS_POLL => poll(frame.a0, frame.a1, frame.a2 as isize as i64),
        SYS_PPOLL => ppoll(frame.a0, frame.a1, frame.a2),
        SYS_SOCKET => socket(frame.a0, frame.a1, frame.a2),
        SYS_BIND => bind(frame.a0, frame.a1, frame.a2),
        SYS_LISTEN => listen(frame.a0, frame.a1),
        SYS_ACCEPT => accept(frame.a0, frame.a1, frame.a2),
        SYS_CONNECT => connect(frame.a0, frame.a1, frame.a2),
        // 206/207 carry the `sendto`/`recvfrom` shape; on a connected socket
        // the destination/source address is optional and ignored (send) or
        // filled in (recv).
        SYS_SENDTO => send_socket(frame.a0, frame.a1, frame.a2, frame.a3, frame.a4, frame.a5),
        SYS_RECVFROM => recv_socket(frame.a0, frame.a1, frame.a2, frame.a3, frame.a4, frame.a5),
        SYS_SETSOCKOPT => setsockopt(frame.a0, frame.a1, frame.a2, frame.a3, frame.a4),
        SYS_GETSOCKOPT => getsockopt(frame.a0, frame.a1, frame.a2, frame.a3, frame.a4),
        SYS_SHUTDOWN => shutdown(frame.a0, frame.a1),
        // pagh-native TLS sessions: the handshake is the kernel's (fail-closed,
        // same verifier as the shell's `https`), the I/O is plaintext on the
        // same descriptor.
        SYS_TLS_CONNECT => tls_connect(frame.a0, frame.a1, frame.a2, frame.a3),
        SYS_TLS_WRITE => tls_write(frame.a0, frame.a1, frame.a2),
        SYS_TLS_READ => tls_read(frame.a0, frame.a1, frame.a2),
        SYS_TLS_CLOSE => tls_close(frame.a0),
        SYS_CLOCK_GETTIME => clock_gettime(frame.a0, frame.a1),
        SYS_NANOSLEEP => nanosleep(frame.a0, frame.a1),
        SYS_UNAME => uname(frame.a0),
        SYS_GETRANDOM => getrandom(frame.a0, frame.a1, frame.a2),
        SYS_BRK => match crate::user::brk(frame.a0) {
            Ok(current) => {
                BRKS.fetch_add(1, Ordering::Relaxed);
                current as isize
            }
            Err(()) => ENOMEM,
        },
        SYS_EXIT => crate::user::exit_thread(frame.a0 as u32),
        SYS_EXIT_GROUP => {
            EXIT_GROUPS.fetch_add(1, Ordering::Relaxed);
            crate::user::exit_group(frame.a0 as u32)
        }
        SYS_SET_TID_ADDRESS => crate::user::thread_id() as isize,
        SYS_KILL => crate::user::signal::kill(frame.a0 as isize, frame.a1 as isize),
        SYS_SIGALTSTACK => crate::user::signal::sigaltstack(frame.a0, frame.a1),
        SYS_TGKILL => crate::user::signal::tgkill(frame.a0, frame.a1, frame.a2 as isize),
        SYS_RT_SIGACTION => {
            crate::user::signal::rt_sigaction(frame.a0, frame.a1, frame.a2, frame.a3)
        }
        SYS_RT_SIGPROCMASK => {
            crate::user::signal::rt_sigprocmask(frame.a0, frame.a1, frame.a2, frame.a3)
        }
        // `rt_sigreturn` replaces the whole trap frame and resumes the
        // interrupted instruction: it must not fall through to the `sepc + 4`
        // bump below. On an error nothing was restored, so the ordinary
        // returning path applies.
        SYS_RT_SIGRETURN => match crate::user::signal::sigreturn(frame) {
            Ok(()) => return,
            Err(error) => error,
        },
        SYS_FUTEX => futex(frame.a0, frame.a1, frame.a2, frame.a3),
        // `clone(0, 0, 0, 0)` is the native `fork` spelling. Any other argument
        // value falls through to the thread form, which rejects flags 0. The
        // guard must precede the generic arm: match arms are ordered.
        SYS_CLONE if frame.a0 == 0 && frame.a1 == 0 && frame.a2 == 0 && frame.a3 == 0 => {
            let result = crate::user::fork(frame);
            if result > 0 {
                FORKS.fetch_add(1, Ordering::Relaxed);
            }
            result
        }
        SYS_CLONE => clone(frame.a0, frame.a1, frame.a2, frame.a3, frame.gp, frame.tp),
        SYS_SCHED_YIELD => {
            crate::task::scheduler::yield_now();
            YIELDS.fetch_add(1, Ordering::Relaxed);
            0
        }
        SYS_GETPID => {
            GETPIDS.fetch_add(1, Ordering::Relaxed);
            crate::user::process_id() as isize
        }
        SYS_GETPPID => {
            GETPPIDS.fetch_add(1, Ordering::Relaxed);
            crate::user::parent_id() as isize
        }
        SYS_GETTID => crate::user::thread_id() as isize,
        SYS_FORK => {
            let result = crate::user::fork(frame);
            if result > 0 {
                FORKS.fetch_add(1, Ordering::Relaxed);
            }
            result
        }
        SYS_EXECVE => match crate::user::execve(frame, frame.a0, frame.a1, frame.a2) {
            Ok(()) => {
                EXECS.fetch_add(1, Ordering::Relaxed);
                return;
            }
            Err(error) => error,
        },
        SYS_WAIT4 => {
            let result = crate::user::wait4(frame.a0 as isize, frame.a1, frame.a2, frame.a3);
            if result > 0 {
                WAITS.fetch_add(1, Ordering::Relaxed);
            }
            result
        }
        _ => {
            UNKNOWN.fetch_add(1, Ordering::Relaxed);
            ENOSYS
        }
    };
    // `SA_RESTART` on the signal that interrupted `futex`/`nanosleep`: the
    // syscall answered `-EINTR` and the handler is about to run at this
    // boundary, but the trap frame must keep the original `a0..a5` and `sepc`
    // so `rt_sigreturn` re-executes the `ecall` with its arguments intact.
    // `take_restart` is armed only by `signal::interrupted`, i.e. only for the
    // two wait syscalls that document restart support.
    if result == EINTR && crate::user::signal::take_restart() {
        return;
    }
    frame.a0 = result as usize;
    // `sret` resumes at the saved `sepc`, which still points at the `ecall`
    // instruction. A returning syscall resumes at the instruction after it;
    // `exit` never reaches this line (it ends the task), so one increment
    // serves every call that returns.
    frame.sepc = frame.sepc.wrapping_add(4);
}

// ─── threads and futex ─────────────────────────────────────────────────────

/// `CLONE_VM`: share the address space (required).
const CLONE_VM: usize = 0x0000_0100;
/// `CLONE_FS`: share filesystem state. pagh has one process-wide cwd-less VFS
/// view, so this flag is accepted and changes nothing.
const CLONE_FS: usize = 0x0000_0200;
/// `CLONE_FILES`: share the descriptor table (required by the implementation,
/// though the native contract only demands `VM|THREAD`).
const CLONE_FILES: usize = 0x0000_0400;
/// `CLONE_SIGHAND`: share signal handlers. Handlers are per process in pagh, so
/// a `clone` child already shares them; this flag is accepted and changes
/// nothing.
const CLONE_SIGHAND: usize = 0x0000_0800;
/// `CLONE_THREAD`: put the new task in the caller's thread group (required).
const CLONE_THREAD: usize = 0x0001_0000;
/// `CLONE_SETTLS`: Linux TLS setup. Not implemented in this MVP.
const CLONE_SETTLS: usize = 0x0008_0000;

/// `futex` command bits (the low seven bits of the op word).
const FUTEX_CMD_MASK: usize = 0x7f;
/// `FUTEX_PRIVATE_FLAG`: the wait queue is process-private. The only flag this
/// kernel accepts, and it changes nothing: there is no shared/global queue.
const FUTEX_PRIVATE_FLAG: usize = 128;
/// `FUTEX_CLOCK_REALTIME`: timed waits against the realtime clock. Rejected.
const FUTEX_CLOCK_REALTIME: usize = 256;
const FUTEX_WAIT: usize = 0;
const FUTEX_WAKE: usize = 1;

/// Native `clone(flags, stack, entry, arg)`.
///
/// The child is a full scheduler task: a PMM kernel stack of its own, a saved
/// context that enters U-mode through the ordinary trap-return path, and the
/// caller's run as its address space. It starts at `entry` with `a0 = arg` and
/// `sp = stack`, and never returns from this syscall — the parent's return value
/// is the child's tid. (The "child returns 0" convention of a libc `clone`
/// wrapper belongs to the wrapper; with an explicit entry there is no syscall
/// return in the child.) `entry` must not return either: the kernel sets no
/// U-mode return trampoline, so a returned `ra` (zero) faults and ends the run.
/// A thread finishes with `exit`/`exit_group`, which is what libpagh's wrapper
/// will call after its closure returns.
///
/// `gp`/`tp` are inherited from the caller's saved trap frame, so the child sees
/// the same small-data anchor and TLS register as its parent. pagh itself has no
/// TLS: `CLONE_SETTLS` is `-ENOTSUP` rather than silently ignored.
///
/// Validation happens before the PMM allocation: unknown flag bits `-EINVAL`,
/// missing `CLONE_VM|CLONE_THREAD` `-EINVAL`, a stack that is not 16-byte
/// aligned or whose first 16 bytes are not in a mapped user read/write page
/// `-EINVAL`, an entry outside a user read/execute page `-EINVAL`. A full task
/// table or frame pool is `-EAGAIN` (the clone-family resource error). The child
/// is charged to the run before the parent returns, and rolled back if the
/// scheduler cannot spawn it, so the runner's `threads` count can never drift.
fn clone(flags: usize, stack: usize, entry: usize, arg: usize, gp: usize, tp: usize) -> isize {
    const SUPPORTED: usize = CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD;
    if flags & !(SUPPORTED | CLONE_SETTLS) != 0 {
        return EINVAL;
    }
    if flags & CLONE_SETTLS != 0 {
        return ENOTSUP;
    }
    if flags & (CLONE_VM | CLONE_THREAD) != CLONE_VM | CLONE_THREAD {
        return EINVAL;
    }
    if stack == 0 || stack % 16 != 0 || entry == 0 || entry % 2 != 0 {
        return EINVAL;
    }
    // `sp` points one past the first byte the child may use; the 16 bytes below
    // it are what a prologue touches before it adjusts the stack. Requiring that
    // page to be a mapped user read/write page is the whole stack contract: the
    // stack is caller-provided and never freed by the kernel.
    let Some(stack_probe) = stack.checked_sub(16) else {
        return EINVAL;
    };
    let Some(stack_flags) = paging::flags(stack_probe & !(FRAME_SIZE - 1)) else {
        return EINVAL;
    };
    if stack_flags.bits() & (PTE_U | PTE_R | PTE_W) != PTE_U | PTE_R | PTE_W {
        return EINVAL;
    }
    let Some(entry_flags) = paging::flags(entry & !(FRAME_SIZE - 1)) else {
        return EINVAL;
    };
    if entry_flags.bits() & (PTE_U | PTE_X) != PTE_U | PTE_X {
        return EINVAL;
    }

    let Some(run_id) = crate::user::current_run_id() else {
        return EINVAL;
    };
    // Charge the thread to the run before spawning. The syscall trap has
    // interrupts masked, so the child cannot run (and exit) between the charge
    // and the spawn; a failed spawn rolls the charge back.
    if !crate::user::charge_thread() {
        return EINVAL;
    }
    match crate::task::scheduler::spawn_user_thread(
        entry,
        stack,
        arg,
        gp,
        tp,
        run_id,
        crate::arch::riscv64::mmu::read_satp(),
    ) {
        Some(tid) => {
            CLONES.fetch_add(1, Ordering::Relaxed);
            tid as isize
        }
        None => {
            crate::user::uncharge_thread();
            EAGAIN
        }
    }
}

/// Extra ticks a `futex` timeout waits, from the ABI's `Timespec`.
///
/// `pointer == 0` means "no deadline". The duration is rounded *up* to the next
/// 10 ms tick, like [`nanosleep`]; a zero duration is `Some(0)` and the caller
/// answers `-ETIMEDOUT` without blocking. Negative or out-of-range fields are
/// `-EINVAL`. `FUTEX_CLOCK_REALTIME` is not accepted, so a timeout is always
/// measured on the kernel's monotonic tick clock.
fn futex_timeout_ticks(pointer: usize) -> Result<Option<u64>, isize> {
    if pointer == 0 {
        return Ok(None);
    }
    let mut raw = [0u8; TIMESPEC_SIZE];
    copy_from_user(&mut raw, pointer)?;
    let seconds = i64::from_le_bytes(raw[0..8].try_into().unwrap_or([0; 8]));
    let nanos = i64::from_le_bytes(raw[8..16].try_into().unwrap_or([0; 8]));
    if seconds < 0 || nanos < 0 || nanos >= NANOS_PER_SEC as i64 {
        return Err(EINVAL);
    }
    let total = (seconds as u128)
        .saturating_mul(NANOS_PER_SEC)
        .saturating_add(nanos as u128);
    let ticks = total
        .saturating_mul(crate::arch::riscv64::timer::TICK_HZ as u128)
        .div_ceil(NANOS_PER_SEC)
        .min(u64::MAX as u128) as u64;
    Ok(Some(ticks))
}

/// `futex(uaddr, op, val, timeout, uaddr2, val3)`.
///
/// Only `FUTEX_WAIT` and `FUTEX_WAKE` exist, plus the private-flag bit every
/// pthread implementation sets. `WAKE_OP`, `REQUEUE`, `CMP_REQUEUE` and the
/// priority-inheritance family are `-ENOSYS`: a program that needs them gets a
/// clear answer instead of a partial operation. `uaddr2`/`val3` are unused by
/// the two implemented operations.
fn futex(uaddr: usize, op: usize, val: usize, timeout: usize) -> isize {
    if op & !(FUTEX_CMD_MASK | FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME) != 0
        || op & FUTEX_CLOCK_REALTIME != 0
    {
        return ENOSYS;
    }
    if uaddr == 0 || uaddr % 4 != 0 {
        return EINVAL;
    }
    let Some(run_id) = crate::user::current_run_id() else {
        return EINVAL;
    };
    match op & FUTEX_CMD_MASK {
        FUTEX_WAIT => {
            let actual = match read_u32_user(uaddr) {
                Ok(value) => value,
                Err(error) => return error,
            };
            // The ABI's `val` is an `int`; only the low 32 bits are compared.
            if actual != val as u32 {
                return EAGAIN;
            }
            let deadline = match futex_timeout_ticks(timeout) {
                Ok(deadline) => deadline,
                Err(error) => return error,
            };
            if deadline == Some(0) {
                return ETIMEDOUT;
            }
            FUTEX_WAITS.fetch_add(1, Ordering::Relaxed);
            // A timed block is woken by the tick scan; without the hardware clock
            // no scan can ever run, so answering immediately is the only honest
            // choice (and matches the `-ETIMEDOUT` an expired deadline returns).
            if deadline.is_some() && !crate::arch::riscv64::timer::running() {
                return ETIMEDOUT;
            }
            // A signal that is already deliverable must not put this thread to
            // sleep: the syscall is interruptible by contract, and the trap
            // boundary delivers the handler right after this returns.
            if crate::user::signal::interrupted() {
                return EINTR;
            }
            let outcome = crate::user::futex::wait(run_id, uaddr, deadline);
            // `kill` wakes a blocked waiter and marks the signal pending. The
            // scheduler cannot tell a signal wake from a futex wake, so the
            // pending mask is the witness; the handler still runs at the trap
            // boundary, not here.
            if crate::user::signal::interrupted() {
                return EINTR;
            }
            match outcome {
                Ok(crate::user::futex::WaitOutcome::Woken) => 0,
                Ok(crate::user::futex::WaitOutcome::TimedOut) => ETIMEDOUT,
                Err(crate::user::futex::WaitError::Full) => EAGAIN,
                Err(crate::user::futex::WaitError::NoMemory) => ENOMEM,
            }
        }
        FUTEX_WAKE => {
            FUTEX_WAKES.fetch_add(1, Ordering::Relaxed);
            crate::user::futex::wake(run_id, uaddr, val) as isize
        }
        _ => ENOSYS,
    }
}

// ─── read / write ──────────────────────────────────────────────────────────

/// `read(fd, buf, len)`.
///
/// fd 0 is the console: one queued byte from the UART ring, else one from the
/// virtio keyboard, else `-EAGAIN` (libpagh retries; the kernel never blocks a
/// user program on input). A file reads at its offset; a directory is `-EISDIR`.
fn read(fd: usize, buf: usize, len: usize) -> isize {
    if len == 0 {
        return 0;
    }
    if buf.checked_add(len).is_none() {
        return fault();
    }
    match lookup(fd) {
        Ok(OpenFd::ConsoleIn) => read_console(buf),
        Ok(OpenFd::ConsoleOut) => EBADF,
        Ok(OpenFd::Unsupported) => ENOTSUP,
        Ok(OpenFd::Pipe { id, read }) => read_pipe(id, read, buf, len),
        // `read` on a UDP descriptor is `recv` with no source address and no
        // per-call flags; the descriptor's own non-blocking bit still applies.
        Ok(OpenFd::UdpSocket { port, nonblocking }) => {
            recv_udp(port, nonblocking, buf, len, 0, 0, 0)
        }
        Ok(OpenFd::Socket {
            id,
            connected: true,
            ..
        })
        | Ok(OpenFd::TcpConn { id }) => read_socket(buf, len, id),
        // A TLS descriptor reads decrypted plaintext (see `tls_read`).
        Ok(OpenFd::Tls { session, .. }) => tls_read_session(session, buf, len),
        Ok(OpenFd::Socket { .. }) | Ok(OpenFd::Listener { .. }) => ENOTCONN,
        Ok(OpenFd::File {
            node,
            offset,
            flags,
        }) => {
            if flags & O_ACCMODE == O_WRONLY {
                return EBADF;
            }
            if node.is_directory() {
                return EISDIR;
            }
            let mut chunk = [0u8; COPY_CHUNK];
            let mut position = offset;
            let mut total = 0usize;
            while total < len {
                let want = (len - total).min(COPY_CHUNK);
                match node.read(position, &mut chunk[..want]) {
                    Ok(0) => break,
                    Ok(read) if read <= want => {
                        if let Err(error) = copy_to_user(buf + total, &chunk[..read]) {
                            return if total > 0 { total as isize } else { error };
                        }
                        total += read;
                        position += read as u64;
                        if read < want {
                            break;
                        }
                    }
                    // A node that claims more than it was given is broken; say
                    // so instead of walking the buffer.
                    Ok(_) => return if total > 0 { total as isize } else { EIO },
                    Err(error) => {
                        return if total > 0 {
                            total as isize
                        } else {
                            errno(error)
                        }
                    }
                }
            }
            if total > 0 {
                set_offset(fd, position);
            }
            READS.fetch_add(1, Ordering::Relaxed);
            READ_BYTES.fetch_add(total as u64, Ordering::Relaxed);
            total as isize
        }
        Err(error) => error,
    }
}

/// One byte from the console, without blocking.
fn read_console(buf: usize) -> isize {
    let byte = crate::drivers::uart::try_get_byte().or_else(crate::drivers::virtio_input::try_key);
    let Some(byte) = byte else {
        return EAGAIN;
    };
    if let Err(error) = copy_to_user(buf, &[byte]) {
        return error;
    }
    READS.fetch_add(1, Ordering::Relaxed);
    READ_BYTES.fetch_add(1, Ordering::Relaxed);
    1
}

/// `write(fd, buf, len)`.
///
/// fd 1/2 go to the console; a file writes at its offset (`O_APPEND` moves the
/// offset to the end first). fd 0 is `-EBADF`, a directory `-EISDIR`.
fn write(fd: usize, buf: usize, len: usize) -> isize {
    if len == 0 {
        return 0;
    }
    if buf.checked_add(len).is_none() {
        return fault();
    }
    match lookup(fd) {
        Ok(OpenFd::ConsoleIn) => EBADF,
        Ok(OpenFd::ConsoleOut) => write_console(buf, len),
        Ok(OpenFd::Unsupported) => ENOTSUP,
        Ok(OpenFd::Pipe { id, read }) => write_pipe(id, read, buf, len),
        // An unconnected datagram socket has no peer; `sendto` is the way to
        // name one. The ABI has no UDP `connect`, so this is the POSIX
        // `EDESTADDRREQ`, never a silent send to nowhere.
        Ok(OpenFd::UdpSocket { .. }) => EDESTADDRREQ,
        Ok(OpenFd::Socket {
            id,
            connected: true,
            ..
        })
        | Ok(OpenFd::TcpConn { id }) => write_socket(buf, len, id),
        // A TLS descriptor writes plaintext: the record layer is inside
        // `net::tls`, below this ABI.
        Ok(OpenFd::Tls { session, .. }) => tls_write_session(session, buf, len),
        Ok(OpenFd::Socket { .. }) | Ok(OpenFd::Listener { .. }) => ENOTCONN,
        Ok(OpenFd::File {
            node,
            offset,
            flags,
        }) => {
            if flags & O_ACCMODE == O_RDONLY {
                return EBADF;
            }
            if node.is_directory() {
                return EISDIR;
            }
            let mut position = if flags & O_APPEND != 0 {
                node.size()
            } else {
                offset
            };
            let mut chunk = [0u8; COPY_CHUNK];
            let mut total = 0usize;
            while total < len {
                let want = (len - total).min(COPY_CHUNK);
                if let Err(error) = copy_from_user(&mut chunk[..want], buf + total) {
                    return if total > 0 { total as isize } else { error };
                }
                match node.write(position, &chunk[..want]) {
                    Ok(written) if written <= want => {
                        total += written;
                        position += written as u64;
                        if written < want {
                            break;
                        }
                    }
                    Ok(_) => return if total > 0 { total as isize } else { EIO },
                    Err(error) => {
                        return if total > 0 {
                            total as isize
                        } else {
                            errno(error)
                        }
                    }
                }
            }
            if total > 0 {
                set_offset(fd, position);
            }
            total as isize
        }
        Err(error) => error,
    }
}

/// Console output: validate the whole buffer first, then copy page by page and
/// hand each chunk to the two sinks. `SUM` covers exactly the copy.
fn write_console(buf: usize, len: usize) -> isize {
    let Some(end) = buf.checked_add(len) else {
        return fault();
    };
    if !user_range_valid(buf, end, false) {
        return fault();
    }
    let was = csr::read_sstatus();
    core::sync::atomic::compiler_fence(Ordering::SeqCst);
    csr::write_sstatus(was | csr::SSTATUS_SUM);
    core::sync::atomic::compiler_fence(Ordering::SeqCst);

    let mut offset = 0usize;
    let mut chunk = [0u8; COPY_CHUNK];
    while offset < len {
        let take = (len - offset)
            .min(COPY_CHUNK)
            .min(FRAME_SIZE - ((buf + offset) & (FRAME_SIZE - 1)));
        // SAFETY: `[buf, end)` was validated page by page above: every page has
        // a live leaf with U=1 and R=1, so reading through `buf + offset` cannot
        // fault, and `take` stops at the page boundary. `SUM` is set for exactly
        // this window. The destination is a local array, disjoint from user
        // memory by construction.
        unsafe {
            core::ptr::copy_nonoverlapping((buf + offset) as *const u8, chunk.as_mut_ptr(), take);
        }
        crate::console::write_bytes(&chunk[..take]);
        // The ABI selftest compares the exact line the argv fixture prints; this
        // records only U-mode `write` bytes and is a no-op on a normal boot.
        crate::user::capture_append(&chunk[..take]);
        offset += take;
    }

    core::sync::atomic::compiler_fence(Ordering::SeqCst);
    csr::write_sstatus(was);
    core::sync::atomic::compiler_fence(Ordering::SeqCst);

    WRITES.fetch_add(1, Ordering::Relaxed);
    BYTES.fetch_add(len as u64, Ordering::Relaxed);
    len as isize
}

// ─── pipes, dup and poll ───────────────────────────────────────────────────

/// `read` on a pipe end: non-blocking, like the console.
///
/// Returns queued bytes, `-EAGAIN` when the ring is empty but a writer is still
/// open, and `0` (EOF) once every write end is closed and the ring drained. The
/// user buffer is validated before a byte is consumed, so `-EFAULT` never loses
/// data. The ring lock is held across the copies: a pipe copy is memory-to-memory
/// and cannot block on a device, which is the property the fd-table rule cares
/// about.
fn read_pipe(id: usize, read: bool, buf: usize, len: usize) -> isize {
    if !read {
        return EBADF;
    }
    if len == 0 {
        return 0;
    }
    let Some(end) = buf.checked_add(len) else {
        return fault();
    };
    if !user_range_valid(buf, end, true) {
        return fault();
    }
    let mut pipes = PIPES.lock();
    let Some(pipe) = pipes.pipes.get_mut(id) else {
        return EBADF;
    };
    if !pipe.in_use {
        return EBADF;
    }
    if pipe.len == 0 {
        return if pipe.writers == 0 { 0 } else { EAGAIN };
    }
    let count = pipe.len.min(len);
    let mut done = 0usize;
    while done < count {
        let start = (pipe.head + done) % PIPE_CAPACITY;
        let take = (count - done).min(PIPE_CAPACITY - start);
        if let Err(error) = copy_to_user(buf + done, &pipe.data[start..start + take]) {
            return if done > 0 { done as isize } else { error };
        }
        done += take;
    }
    pipe.head = (pipe.head + count) % PIPE_CAPACITY;
    pipe.len -= count;
    READS.fetch_add(1, Ordering::Relaxed);
    READ_BYTES.fetch_add(count as u64, Ordering::Relaxed);
    count as isize
}

/// `write` on a pipe end: append what fits, `-EAGAIN` only when the ring is full.
///
/// A partial write is a normal success (the return value says how much was
/// stored). With no read end open the write is `-EPIPE` — there is no signal
/// layer in this ABI, so an applet must notice the error. The source is validated
/// before the ring is touched.
fn write_pipe(id: usize, read: bool, buf: usize, len: usize) -> isize {
    if read {
        return EBADF;
    }
    if len == 0 {
        return 0;
    }
    let Some(end) = buf.checked_add(len) else {
        return fault();
    };
    if !user_range_valid(buf, end, false) {
        return fault();
    }
    let mut pipes = PIPES.lock();
    let Some(pipe) = pipes.pipes.get_mut(id) else {
        return EBADF;
    };
    if !pipe.in_use {
        return EBADF;
    }
    if pipe.readers == 0 {
        return EPIPE;
    }
    if pipe.free() == 0 {
        return EAGAIN;
    }
    let count = pipe.free().min(len);
    let start = (pipe.head + pipe.len) % PIPE_CAPACITY;
    let mut done = 0usize;
    while done < count {
        let at = (start + done) % PIPE_CAPACITY;
        let take = (count - done).min(PIPE_CAPACITY - at);
        if let Err(error) = copy_from_user(&mut pipe.data[at..at + take], buf + done) {
            pipe.len += done;
            return if done > 0 { done as isize } else { error };
        }
        done += take;
    }
    pipe.len += count;
    count as isize
}

/// `pipe2(*fds, flags)`: create a pipe and install its two ends.
///
/// `flags` must be 0: `O_NONBLOCK` is already the only mode and `O_CLOEXEC` is
/// not implemented (it would need a close-on-exec flag in `FdEntry`), so a bit
/// the ABI does not implement is `-EINVAL` rather than silently dropped. The read end is installed first, the write end
/// second; if either the second slot or the `fds` write fails, both references
/// are dropped, so a failed `pipe2` leaves neither a descriptor nor a ring.
fn pipe2(fds: usize, flags: usize) -> isize {
    if flags != 0 {
        return EINVAL;
    }
    let Some(end) = fds.checked_add(2 * core::mem::size_of::<i32>()) else {
        return fault();
    };
    if !user_range_valid(fds, end, true) {
        return fault();
    }
    let id = match PIPES.lock().create() {
        Some(id) => id,
        None => return ENOSPC,
    };
    let read_fd = match install_pipe(id, true) {
        Ok(fd) => fd,
        Err(error) => {
            pipe_release(id, true);
            pipe_release(id, false);
            return error;
        }
    };
    let write_fd = match install_pipe(id, false) {
        Ok(fd) => fd,
        Err(error) => {
            release_slot(read_fd);
            pipe_release(id, false);
            return error;
        }
    };
    let mut raw = [0u8; 8];
    raw[0..4].copy_from_slice(&(read_fd as i32).to_le_bytes());
    raw[4..8].copy_from_slice(&(write_fd as i32).to_le_bytes());
    if let Err(error) = copy_to_user(fds, &raw) {
        release_slot(read_fd);
        release_slot(write_fd);
        return error;
    }
    PIPE_CREATES.fetch_add(1, Ordering::Relaxed);
    0
}

/// Clone one descriptor table entry, if it exists.
fn clone_slot(fd: usize) -> Option<FdEntry> {
    with_fds(|table| table.slots.get(fd).and_then(|slot| slot.as_ref()).cloned()).flatten()
}

/// Publish `entry` in `fd` of the calling process's row.
fn put_slot(fd: usize, entry: FdEntry) -> bool {
    with_fds(|table| {
        if let Some(slot) = table.slots.get_mut(fd) {
            *slot = Some(entry);
            true
        } else {
            false
        }
    })
    .unwrap_or(false)
}

/// `dup(oldfd)`: duplicate into the first free descriptor slot.
///
/// The duplicate shares the underlying object: a file node (`Arc`), a directory
/// snapshot (independent cursor — this ABI has no shared file description), or a
/// pipe ring. The last sentence is what makes `dup` on a pipe end meaningful: the
/// two descriptors read/write the same 4 KiB ring and the ring is freed when the
/// last reference on both sides is closed.
fn dup(oldfd: usize) -> isize {
    let Some(entry) = clone_slot(oldfd) else {
        return EBADF;
    };
    let Some(fd) = alloc_fd() else {
        drop_entry(entry);
        return ENOSPC;
    };
    if !put_slot(fd, entry) {
        return EBADF;
    }
    DUPS.fetch_add(1, Ordering::Relaxed);
    fd as isize
}

/// `dup2/dup3(oldfd, newfd, flags)`: duplicate onto `newfd`.
///
/// `flags` must be 0. `oldfd == newfd` returns `newfd` without touching the table
/// (the `dup2` rule; Linux's `dup3` would answer `-EINVAL`, a difference the ABI
/// document records). `newfd` is closed first if it is open; the table is
/// updated before the old occupant is dropped, so no error path can lose both.
fn dup2(oldfd: usize, newfd: usize, flags: usize) -> isize {
    if flags != 0 {
        return EINVAL;
    }
    if newfd >= MAX_FDS {
        return EBADF;
    }
    let Some(entry) = clone_slot(oldfd) else {
        return EBADF;
    };
    if oldfd == newfd {
        drop_entry(entry);
        return newfd as isize;
    }
    let previous = take_slot(newfd);
    if !put_slot(newfd, entry) {
        return EBADF;
    }
    if let Some(previous) = previous {
        drop_entry(previous);
    }
    DUPS.fetch_add(1, Ordering::Relaxed);
    newfd as isize
}

// ─── poll / ppoll ─────────────────────────────────────────────────────────

/// True when the console would hand out a byte right now, without consuming it.
///
/// The UART ring is interrupt-fed; the keyboard needs a device poll first, which
/// is exactly what the shell's idle loop does. `poll` performs that device poll
/// itself before every scan so a program waiting for typed input makes progress
/// without depending on the idle loop being scheduled in the right order.
fn console_readable() -> bool {
    crate::drivers::uart::rx_ready() || crate::drivers::virtio_input::key_ready()
}

/// Revision of one descriptor for `poll`.
///
/// `POLLERR`/`POLLHUP` are reported regardless of `events` (they are conditions,
/// not requests), `POLLIN`/`POLLOUT` only when asked for. A closed or invalid fd
/// becomes `POLLNVAL`, the standard "this array entry is not a descriptor".
fn poll_one(fd: usize, events: i16) -> i16 {
    match lookup(fd) {
        Ok(OpenFd::ConsoleIn) => {
            if events & POLLIN != 0 && console_readable() {
                POLLIN
            } else {
                0
            }
        }
        Ok(OpenFd::ConsoleOut) => {
            if events & POLLOUT != 0 {
                POLLOUT
            } else {
                0
            }
        }
        // A regular file (and a directory) is always ready: there is no page
        // cache wait and no blocking backend behind the VFS.
        Ok(OpenFd::File { .. }) => {
            let mut revents = 0;
            if events & POLLIN != 0 {
                revents |= POLLIN;
            }
            if events & POLLOUT != 0 {
                revents |= POLLOUT;
            }
            revents
        }
        Ok(OpenFd::Pipe { id, read }) => {
            let pipes = PIPES.lock();
            let Some(pipe) = pipes.pipes.get(id).filter(|pipe| pipe.in_use) else {
                return POLLNVAL;
            };
            let mut revents = 0;
            if read {
                if events & POLLIN != 0 && pipe.len > 0 {
                    revents |= POLLIN;
                }
                if pipe.writers == 0 {
                    revents |= POLLHUP;
                }
            } else {
                if events & POLLOUT != 0 && pipe.readers > 0 && pipe.free() > 0 {
                    revents |= POLLOUT;
                }
                if pipe.readers == 0 {
                    revents |= POLLERR;
                }
            }
            revents
        }
        // A bound datagram socket is readable exactly when its queue has a
        // datagram; it is always writable (the send path has no flow control).
        Ok(OpenFd::UdpSocket { port, .. }) => {
            let mut revents = 0;
            if events & POLLIN != 0 && crate::net::udp_pending(port) {
                revents |= POLLIN;
            }
            if events & POLLOUT != 0 {
                revents |= POLLOUT;
            }
            revents
        }
        Ok(OpenFd::Unsupported) => POLLNVAL,
        // A TLS session is readable when plaintext is buffered *or* the socket
        // has bytes — the second can be a partial record, so a `read` after
        // `POLLIN` may still wait for the rest of it (bounded by the session's
        // idle budget; the exact rule is in `net::tls::user_ready` and in the
        // ABI document). `POLLHUP` is reported once the peer closed.
        Ok(OpenFd::Tls { session, .. }) => {
            let Some(ready) = crate::net::tls::user_ready(session) else {
                return POLLNVAL;
            };
            let mut revents = 0;
            if events & POLLIN != 0 && ready.readable {
                revents |= POLLIN;
            }
            if events & POLLOUT != 0 && ready.writable {
                revents |= POLLOUT;
            }
            if ready.eof {
                revents |= POLLHUP;
            }
            revents
        }
        Ok(OpenFd::Listener { .. }) => {
            if events & POLLIN != 0 && crate::net::tcp::accept_pending() {
                POLLIN
            } else {
                0
            }
        }
        Ok(OpenFd::Socket {
            id,
            connected: true,
            ..
        })
        | Ok(OpenFd::TcpConn { id }) => {
            let mut revents = 0;
            if events & POLLIN != 0 && crate::net::tcp::readable_id(id) {
                revents |= POLLIN;
            }
            if crate::net::tcp::reset_id_state(id) {
                revents |= POLLERR;
            } else if events & POLLOUT != 0 {
                revents |= POLLOUT;
            }
            revents
        }
        Ok(OpenFd::Socket { .. }) => POLLERR,
        Err(_) => POLLNVAL,
    }
}

/// Read the `i`th `pollfd` from user memory as `(fd, events, revents_offset)`.
///
/// Byte-wise on purpose: a `pollfd` array has only C alignment (8 bytes per
/// record), so a struct-shaped load could be unaligned and `copy_from_user` is
/// the same contract every other pointer in this layer follows.
fn read_pollfd(base: usize, index: usize) -> Result<(i32, i16), isize> {
    let mut raw = [0u8; POLLFD_SIZE];
    copy_from_user(&mut raw, base + index * POLLFD_SIZE)?;
    let fd = i32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
    let events = i16::from_le_bytes([raw[4], raw[5]]);
    Ok((fd, events))
}

/// Write back `revents` for one `pollfd`; `fd`, `events` stay untouched.
fn write_pollfd(base: usize, index: usize, revents: i16) -> Result<(), isize> {
    let offset = base + index * POLLFD_SIZE + 6;
    copy_to_user(offset, &revents.to_le_bytes())
}

/// Shared body of [`poll`] and [`ppoll`]: scan, wait, scan until a deadline.
///
/// `timeout_ms < 0` waits forever, `0` scans once and returns. Waiting is
/// `scheduler::yield_now`, never a busy `wfi`: other tasks (the shell's idle loop,
/// the network poll) must run so the keyboard and pointer keep working while a
/// program waits. The timeout is measured in kernel ticks, so a 1 ms request is
/// one tick (10 ms).
fn poll_wait(fds: usize, nfds: usize, timeout_ms: i64) -> isize {
    let deadline = if timeout_ms < 0 {
        None
    } else {
        let ticks = (timeout_ms as u64)
            .saturating_mul(crate::arch::riscv64::timer::TICK_HZ as u64)
            .div_ceil(1000);
        Some(crate::arch::riscv64::timer::ticks().saturating_add(ticks))
    };

    loop {
        // The keyboard and NIC have no routed interrupt (see the drivers
        // README): make pending events visible before deciding nothing is
        // ready. A listener's `POLLIN` and a stream's data both depend on this
        // poll, because the shell is parked in `wait_for_process` while the
        // program runs.
        crate::drivers::virtio_input::poll();
        crate::net::poll();
        let mut revents = [0i16; MAX_POLL_FDS];
        let mut ready = 0usize;
        for index in 0..nfds {
            let (fd, events) = match read_pollfd(fds, index) {
                Ok(entry) => entry,
                Err(error) => return error,
            };
            if fd < 0 {
                continue;
            }
            let value = poll_one(fd as usize, events);
            if value != 0 {
                revents[index] = value;
                ready += 1;
            }
        }
        if ready > 0 || timeout_ms == 0 {
            for index in 0..nfds {
                if let Err(error) = write_pollfd(fds, index, revents[index]) {
                    return error;
                }
            }
            return ready as isize;
        }
        if let Some(deadline) = deadline {
            if crate::arch::riscv64::timer::ticks() >= deadline {
                for index in 0..nfds {
                    if let Err(error) = write_pollfd(fds, index, 0) {
                        return error;
                    }
                }
                return 0;
            }
        }
        crate::task::scheduler::yield_now();
    }
}

/// Validate the `pollfd` array's address range, then run [`poll_wait`].
fn poll_common(fds: usize, nfds: usize, timeout_ms: i64) -> isize {
    if nfds > MAX_POLL_FDS {
        return EINVAL;
    }
    let Some(bytes) = nfds.checked_mul(POLLFD_SIZE) else {
        return fault();
    };
    if bytes > 0 {
        let Some(end) = fds.checked_add(bytes) else {
            return fault();
        };
        if !user_range_valid(fds, end, true) {
            return fault();
        }
    }
    let result = poll_wait(fds, nfds, timeout_ms);
    if result >= 0 {
        POLLS.fetch_add(1, Ordering::Relaxed);
    }
    result
}

/// `poll(fds, nfds, timeout_ms)`: wait for events on up to [`MAX_POLL_FDS`]
/// descriptors.
///
/// `timeout_ms` 0 is a single non-blocking scan; negative waits without a
/// deadline. A zero return is a timeout, not an error.
fn poll(fds: usize, nfds: usize, timeout_ms: i64) -> isize {
    poll_common(fds, nfds, timeout_ms)
}

/// `ppoll(fds, nfds, *Timespec, sigmask, sigsetsize)`.
///
/// The timeout is a `Timespec` rounded up to whole milliseconds; a null pointer
/// means "wait forever". The signal mask and its size are accepted and ignored —
/// this kernel has no signals, so there is nothing to unmask during the wait.
/// Every other rule is [`poll`]'s.
fn ppoll(fds: usize, nfds: usize, timeout: usize) -> isize {
    let timeout_ms = if timeout == 0 {
        -1
    } else {
        let mut raw = [0u8; TIMESPEC_SIZE];
        if let Err(error) = copy_from_user(&mut raw, timeout) {
            return error;
        }
        let seconds = i64::from_le_bytes(raw[0..8].try_into().unwrap_or([0; 8]));
        let nanos = i64::from_le_bytes(raw[8..16].try_into().unwrap_or([0; 8]));
        if seconds < 0 || nanos < 0 || nanos >= NANOS_PER_SEC as i64 {
            return EINVAL;
        }
        (seconds as i128)
            .saturating_mul(1000)
            .saturating_add((nanos as i128 + 999_999) / 1_000_000)
            .min(i64::MAX as i128) as i64
    };
    poll_common(fds, nfds, timeout_ms)
}

// ─── mmap / munmap / mprotect ─────────────────────────────────────────────

/// Turn the ABI's `PROT_*` bits into PTE permissions.
///
/// `R` is always set: a RISC-V leaf without `R` is write-only (reserved as a
/// normal mapping), and every user buffer helper validates `R`. `W` and `X` may
/// not be combined — pagh's W^X policy — so a program cannot ask for a writable
/// and executable page; allocate twice if a JIT needs that. `PROT_NONE` is not
/// expressible either (a leaf with no R/W/X is not a leaf to the walker), so it
/// is `-EINVAL` rather than a mapping that would fault on first touch.
fn prot_bits(prot: usize) -> Result<u64, isize> {
    const ALLOWED: usize = PROT_READ | PROT_WRITE | PROT_EXEC;
    if prot == 0 || prot & !ALLOWED != 0 {
        return Err(EINVAL);
    }
    let write = prot & PROT_WRITE != 0;
    let execute = prot & PROT_EXEC != 0;
    if write && execute {
        return Err(EINVAL);
    }
    let mut bits = PTE_V | PTE_R | PTE_U | PTE_A | PTE_D;
    if write {
        bits |= PTE_W;
    }
    if execute {
        bits |= PTE_X;
    }
    Ok(bits)
}

/// Whole pages needed for `length` bytes, or `None` on overflow.
fn pages_for(length: usize) -> Option<usize> {
    length
        .checked_add(FRAME_SIZE - 1)
        .map(|rounded| rounded / FRAME_SIZE)
}

/// `mmap(addr, length, prot, flags, fd, offset)`: anonymous private memory.
///
/// Only `MAP_PRIVATE | MAP_ANONYMOUS` is implemented. `MAP_SHARED`, `MAP_FIXED`
/// and a real `fd`/`offset` are `-ENOTSUP`/`-EINVAL` rather than silently
/// ignored — a caller asking for shared or file-backed memory must know it did
/// not get it. `addr` is a hint: it is used when page-aligned, inside the
/// [`crate::user::USER_MMAP_BASE`]..[`crate::user::USER_MMAP_TOP`] window and
/// entirely free; otherwise the kernel picks the first free run.
///
/// Frames are allocated and zeroed now. A failed allocation (or a full region
/// table) rolls every page of this call back, so the answer is either a complete
/// mapping or an unchanged ledger.
fn mmap(addr: usize, length: usize, prot: usize, flags: usize, fd: usize, offset: usize) -> isize {
    if length == 0 {
        return EINVAL;
    }
    if flags & MAP_SHARED != 0 || flags & MAP_FIXED != 0 {
        return ENOTSUP;
    }
    if flags != MAP_PRIVATE | MAP_ANONYMOUS {
        return EINVAL;
    }
    if fd as isize != -1 || offset != 0 {
        return EINVAL;
    }
    let bits = match prot_bits(prot) {
        Ok(bits) => bits,
        Err(error) => return error,
    };
    let Some(pages) = pages_for(length) else {
        return ENOMEM;
    };
    if pages > USER_MMAP_PAGES {
        return ENOMEM;
    }
    let hint = if addr % FRAME_SIZE == 0 { addr } else { 0 };
    match crate::user::mmap(hint, pages, bits) {
        Ok(base) => {
            MMAPS.fetch_add(1, Ordering::Relaxed);
            base as isize
        }
        Err(MmapError::Invalid) => EINVAL,
        Err(MmapError::NoMemory) => ENOMEM,
    }
}

/// `munmap(addr, length)`: unmap whole pages, rounding `length` up.
///
/// `addr` must be page-aligned and `length` non-zero. The range must be part of
/// mappings made by this run's `mmap`; freeing the image, the stack or a hole is
/// `-EINVAL`, and a partial unmap splits the bookkeeping extent so teardown frees
/// exactly what is left.
fn munmap(addr: usize, length: usize) -> isize {
    if length == 0 || addr % FRAME_SIZE != 0 {
        return EINVAL;
    }
    let Some(pages) = pages_for(length) else {
        return EINVAL;
    };
    match crate::user::munmap(addr, pages) {
        Ok(()) => {
            MUNMAPS.fetch_add(1, Ordering::Relaxed);
            0
        }
        Err(MmapError::Invalid) => EINVAL,
        Err(MmapError::NoMemory) => ENOMEM,
    }
}

/// `mprotect(addr, length, prot)`: rewrite R/W/X on mapped user pages.
///
/// The same `PROT_*` validation as [`mmap`] applies (W^X rejected, `R` implied,
/// `PROT_NONE` refused). Every page is checked to be a live user leaf before the
/// first PTE is touched, so an unmapped page or a bad range changes nothing.
fn mprotect(addr: usize, length: usize, prot: usize) -> isize {
    if length == 0 || addr % FRAME_SIZE != 0 {
        return EINVAL;
    }
    let bits = match prot_bits(prot) {
        Ok(bits) => bits,
        Err(error) => return error,
    };
    let Some(pages) = pages_for(length) else {
        return EINVAL;
    };
    match crate::user::mprotect(addr, pages, bits) {
        Ok(()) => {
            MPROTECTS.fetch_add(1, Ordering::Relaxed);
            0
        }
        Err(MmapError::Invalid) => EINVAL,
        Err(MmapError::NoMemory) => ENOMEM,
    }
}

// ─── sockets (AF_INET; one TCP client and userland UDP) ───────────────────

/// The ABI's native `sockaddr_in`: `u16 family` (little-endian, `AF_INET = 2`),
/// `u16 port` (network byte order), `[u8; 4] address`, then zero padding to 16.
const SOCKADDR_IN_SIZE: usize = 16;
const AF_INET: u16 = 2;
const SOCK_STREAM: u32 = 1;
const SOCK_DGRAM: u32 = 2;
/// `socket()` type bit: the descriptor starts in non-blocking mode. It applies
/// to a UDP `recvfrom` and to a stream listener's `accept`; TCP `recv` is
/// already non-blocking by contract.
const SOCK_NONBLOCK: usize = 0x800;
/// Sentinel in `FdEntry::Socket::id`: the descriptor has no TCP connection yet.
const SOCKET_NONE: u8 = u8::MAX;
/// `recvfrom` flag: do not wait for a datagram (`-EAGAIN` instead).
const MSG_DONTWAIT: usize = 0x40;
/// How long a blocking UDP `recvfrom` polls before answering `-EAGAIN`. The
/// NIC is polled by software, so a syscall cannot sleep on a wait queue; this
/// is the same bounded-poll shape as `connect`/`send`, long enough for an ARP
/// resolution and a host round trip on QEMU's user network.
const UDP_RECV_TICKS: u64 = 1000;

/// Payload bytes copied between user memory and the TCP stack per step. One
/// TCP MSS is 1000 bytes; a full segment per syscall step keeps `send` from
/// waiting for an ACK per 256-byte console-sized chunk.
const SOCKET_CHUNK: usize = 1000;

// `setsockopt` / `getsockopt` names (the subset this kernel accepts).
const SOL_SOCKET: usize = 1;
const SO_REUSEADDR: usize = 2;
const SO_TYPE: usize = 3;
const SO_ERROR: usize = 4;
const SO_KEEPALIVE: usize = 9;
const SO_RCVTIMEO: usize = 20;
const SO_SNDTIMEO: usize = 21;
const IPPROTO_TCP: usize = 6;
const TCP_NODELAY: usize = 1;

/// Run `body` with supervisor interrupts enabled, then restore the exact entry
/// value.
///
/// A syscall arrives with `sstatus.SIE = 0` (hardware cleared it), and this
/// stack's waits are tick-bounded: with the clock frozen, `tcp::wait_for` would
/// fall back to its iteration cap and the scheduler could not preempt a program
/// parked in `connect`/`send`. Enabling interrupts for the duration is the same
/// deliberate exception `nanosleep` makes, with no lock held: the TCP state lock
/// is taken only for short sections inside the stack, and no other task touches
/// the network while a user program runs (the shell is in `wait_for_run_end`, idle
/// is in `wfi`). The trap epilogue restores `sstatus` from the frame anyway, so
/// this cannot leak into U-mode. A timer interrupt here may switch this task out
/// and back; the syscall simply continues, exactly like `nanosleep`.
fn with_network_interrupts<T>(body: impl FnOnce() -> T) -> T {
    let was = csr::read_sstatus();
    core::sync::atomic::compiler_fence(Ordering::SeqCst);
    csr::write_sstatus(was | csr::SSTATUS_SIE);
    core::sync::atomic::compiler_fence(Ordering::SeqCst);
    let result = body();
    core::sync::atomic::compiler_fence(Ordering::SeqCst);
    csr::write_sstatus(was);
    core::sync::atomic::compiler_fence(Ordering::SeqCst);
    result
}

/// Read the 16-byte native `sockaddr_in` from user memory, returning the family
/// and the parsed `(address, port)`. The caller validates the family.
fn read_sockaddr_in(pointer: usize) -> Result<(u16, [u8; 4], u16), isize> {
    let mut raw = [0u8; SOCKADDR_IN_SIZE];
    copy_from_user(&mut raw, pointer)?;
    let family = u16::from_le_bytes([raw[0], raw[1]]);
    let port = u16::from_be_bytes([raw[2], raw[3]]);
    Ok((family, [raw[4], raw[5], raw[6], raw[7]], port))
}

/// Write the native `sockaddr_in` to user memory (used to fill `recvfrom`'s
/// source address). Padding is left zero.
fn write_sockaddr_in(pointer: usize, address: [u8; 4], port: u16) -> Result<(), isize> {
    let mut raw = [0u8; SOCKADDR_IN_SIZE];
    raw[0..2].copy_from_slice(&AF_INET.to_le_bytes());
    raw[2..4].copy_from_slice(&port.to_be_bytes());
    raw[4..8].copy_from_slice(&address);
    copy_to_user(pointer, &raw)
}

fn read_u32_user(pointer: usize) -> Result<u32, isize> {
    let mut raw = [0u8; 4];
    copy_from_user(&mut raw, pointer)?;
    Ok(u32::from_le_bytes(raw))
}

fn write_u32_user(pointer: usize, value: u32) -> Result<(), isize> {
    copy_to_user(pointer, &value.to_le_bytes())
}

/// `socket(domain, type, protocol)`.
///
/// `AF_INET` with protocol 0 supports `SOCK_STREAM` (one active-open client
/// descriptor at a time; a second `socket()` while one exists is `-ENOSPC`, but
/// a listener and accepted connections have their own descriptors) and
/// `SOCK_DGRAM` (a UDP socket, initially unbound; any number may be open).
/// `SOCK_NONBLOCK` is accepted for both: for a datagram socket it makes
/// `recvfrom` answer `-EAGAIN`, for a stream socket it makes a later
/// `listen`/`accept` answer `-EAGAIN`. Everything else is `-EAFNOSUPPORT` — the
/// ABI documents one answer instead of inventing per-field errnos.
fn socket(domain: usize, kind: usize, protocol: usize) -> isize {
    if domain != AF_INET as usize || protocol != 0 {
        return EAFNOSUPPORT;
    }
    let nonblocking = kind & SOCK_NONBLOCK != 0;
    let base = (kind & !SOCK_NONBLOCK) as u32;
    let result = match base {
        SOCK_STREAM => install_socket(nonblocking),
        SOCK_DGRAM => install_udp(nonblocking),
        _ => return EAFNOSUPPORT,
    };
    match result {
        Ok(fd) => {
            SOCKETS.fetch_add(1, Ordering::Relaxed);
            fd as isize
        }
        Err(error) => error,
    }
}

/// Map a TCP bind/listen refusal to the ABI's errno.
fn tcp_bind_errno(error: crate::net::tcp::BindError) -> isize {
    match error {
        crate::net::tcp::BindError::AddressInUse => EADDRINUSE,
        crate::net::tcp::BindError::NoPorts => ENOSPC,
        crate::net::tcp::BindError::NotBound => EINVAL,
    }
}

/// `bind(fd, *sockaddr_in, addrlen)`.
///
/// For a UDP socket `addrlen` must be exactly 16 and the family `AF_INET`; the
/// address must be `INADDR_ANY` (`0.0.0.0`) or the interface's current DHCP
/// lease. Port 0 asks for an ephemeral port; an already-bound socket is
/// `-EINVAL` and a port another socket owns is `-EADDRINUSE`.
///
/// For a stream socket the same address rules apply, but the port is reserved
/// for a later `listen(201)` instead of a datagram queue: one listener (one
/// pre-listen reservation) exists at a time, so a second `bind` while one is
/// live is `-EADDRINUSE`. Binding a connected socket or an accepted connection
/// is `-EINVAL`; a listening descriptor is already bound, so binding it again
/// is `-EINVAL` too.
fn bind(fd: usize, address: usize, length: usize) -> isize {
    if address == 0 {
        return fault();
    }
    if length != SOCKADDR_IN_SIZE {
        return EINVAL;
    }
    let (family, ip, port) = match read_sockaddr_in(address) {
        Ok(parsed) => parsed,
        Err(error) => return error,
    };
    if family != AF_INET {
        return EAFNOSUPPORT;
    }
    if ip != [0; 4] {
        match crate::net::config() {
            Some(config) if config.ip == ip => {}
            _ => return EADDRNOTAVAIL,
        }
    }
    match lookup(fd) {
        Ok(OpenFd::UdpSocket { port: current, .. }) => {
            if current != 0 {
                return EINVAL;
            }
            let chosen = match crate::net::udp_bind(port) {
                Ok(port) => port,
                Err(crate::net::UdpBindError::AddressInUse) => return EADDRINUSE,
                Err(crate::net::UdpBindError::NoSlots) => return ENOSPC,
            };
            if !set_udp_port(fd, chosen) {
                crate::net::udp_unbind(chosen);
                return EBADF;
            }
            0
        }
        Ok(OpenFd::Socket {
            connected, bound, ..
        }) => {
            if connected || bound != 0 {
                return EINVAL;
            }
            let chosen = match crate::net::tcp::bind_port(port) {
                Ok(port) => port,
                Err(error) => return tcp_bind_errno(error),
            };
            if !set_socket_bound(fd, chosen) {
                crate::net::tcp::release_binding(chosen);
                return EBADF;
            }
            0
        }
        Ok(OpenFd::Listener { .. }) | Ok(OpenFd::TcpConn { .. }) => EINVAL,
        Ok(OpenFd::Unsupported) => ENOTSUP,
        Ok(_) => ENOTSOCK,
        Err(error) => error,
    }
}

/// `listen(fd, backlog)`.
///
/// Only a stream descriptor that has been bound with `bind(200)` can listen.
/// `backlog` is clamped to `1..=4` (the fixed accept-queue depth); a negative
/// value arrives as a huge `usize` and clamps to 4 as well. A second `listen`
/// on the same listener updates the backlog and returns 0. A UDP descriptor,
/// an accepted connection or an unbound stream socket is `-ENOTSUP`/`-EINVAL`.
fn listen(fd: usize, backlog: usize) -> isize {
    match lookup(fd) {
        Ok(OpenFd::Socket {
            connected: false,
            bound,
            nonblocking,
            ..
        }) if bound != 0 => match crate::net::tcp::listen(bound, backlog) {
            Ok(()) => {
                if convert_to_listener(fd, bound, nonblocking) {
                    LISTENS.fetch_add(1, Ordering::Relaxed);
                    0
                } else {
                    crate::net::tcp::close_listener();
                    EBADF
                }
            }
            Err(error) => tcp_bind_errno(error),
        },
        Ok(OpenFd::Listener { port, .. }) => match crate::net::tcp::listen(port, backlog) {
            Ok(()) => 0,
            Err(error) => tcp_bind_errno(error),
        },
        Ok(OpenFd::UdpSocket { .. }) => ENOTSUP,
        Ok(OpenFd::Socket { .. }) | Ok(OpenFd::TcpConn { .. }) => EINVAL,
        Ok(OpenFd::Unsupported) => ENOTSUP,
        Ok(_) => ENOTSOCK,
        Err(error) => error,
    }
}

/// `accept(fd, *sockaddr_in, *addrlen)`.
///
/// Pops the oldest handshaken passive connection from the listener's queue and
/// installs it as a new descriptor. The optional peer address is written as the
/// same 16-byte native `sockaddr_in` `bind`/`connect` use, with the family,
/// big-endian port and four address bytes; `*addrlen` is set to 16.
///
/// A blocking listener polls the NIC for up to [`crate::net::tcp::ACCEPT_TICKS`]
/// (30 s) and answers `-EAGAIN` on expiry. A listener created with
/// `SOCK_NONBLOCK` polls once and answers `-EAGAIN` immediately. The address
/// buffers are validated before the wait, so a bad pointer is `-EFAULT` even
/// when the queue is empty.
fn accept(fd: usize, address: usize, length: usize) -> isize {
    let nonblocking = match lookup(fd) {
        Ok(OpenFd::Listener { nonblocking, .. }) => nonblocking,
        Ok(OpenFd::Socket { .. }) | Ok(OpenFd::TcpConn { .. }) => return EINVAL,
        Ok(OpenFd::UdpSocket { .. }) => return ENOTSUP,
        Ok(OpenFd::Unsupported) => return ENOTSUP,
        Ok(_) => return ENOTSOCK,
        Err(error) => return error,
    };
    if address != 0 {
        if length == 0 {
            return EINVAL;
        }
        let capacity = match read_u32_user(length) {
            Ok(value) => value,
            Err(error) => return error,
        };
        if capacity < SOCKADDR_IN_SIZE as u32 {
            return EINVAL;
        }
        let Some(address_end) = address.checked_add(SOCKADDR_IN_SIZE) else {
            return fault();
        };
        let Some(length_end) = length.checked_add(4) else {
            return fault();
        };
        if !user_range_valid(address, address_end, true)
            || !user_range_valid(length, length_end, true)
        {
            return fault();
        }
    }
    let accepted = if nonblocking {
        crate::net::tcp::accept_nonblocking()
    } else {
        with_network_interrupts(|| crate::net::tcp::accept(crate::net::tcp::ACCEPT_TICKS))
    };
    let (id, ip, remote_port) = match accepted {
        Ok(connection) => connection,
        Err(crate::net::tcp::Error::WouldBlock) => return EAGAIN,
        Err(crate::net::tcp::Error::NoLease) => return ENETUNREACH,
        Err(crate::net::tcp::Error::Reset) | Err(crate::net::tcp::Error::Closed) => return EAGAIN,
        Err(_) => return EAGAIN,
    };
    if address != 0 {
        if let Err(error) = write_sockaddr_in(address, ip, remote_port) {
            crate::net::tcp::close_id(id);
            return error;
        }
        if let Err(error) = write_u32_user(length, SOCKADDR_IN_SIZE as u32) {
            crate::net::tcp::close_id(id);
            return error;
        }
    }
    let installed = match install_tcp_conn(id) {
        Ok(fd) => fd,
        Err(error) => {
            crate::net::tcp::close_id(id);
            return error;
        }
    };
    ACCEPTS.fetch_add(1, Ordering::Relaxed);
    installed as isize
}

/// Map a connect failure to the ABI's errno. `Reset` is a peer RST during the
/// handshake, which for a client means "connection refused"; a frame that could
/// not be sent is "host unreachable" (no ARP answer or no lease route).
fn connect_errno(error: tcp::Error) -> isize {
    match error {
        tcp::Error::NoLease => ENETUNREACH,
        tcp::Error::Busy => EISCONN,
        tcp::Error::Timeout => ETIMEDOUT,
        tcp::Error::Reset | tcp::Error::Closed => ECONNREFUSED,
        tcp::Error::Send => EHOSTUNREACH,
        tcp::Error::WouldBlock => EAGAIN,
    }
}

/// Map a `send`/`recv` failure. `Closed` after a peer FIN is `-EPIPE` for a
/// write and 0 for a read (handled by the stack returning `Ok(0)`); a reset is
/// `-ECONNRESET`. A would-block is only possible on the non-blocking receive.
fn stream_errno(error: tcp::Error) -> isize {
    match error {
        tcp::Error::NoLease => ENETUNREACH,
        tcp::Error::Busy => EAGAIN,
        tcp::Error::Timeout => ETIMEDOUT,
        tcp::Error::Reset => ECONNRESET,
        tcp::Error::Closed => EPIPE,
        tcp::Error::Send => EHOSTUNREACH,
        tcp::Error::WouldBlock => EAGAIN,
    }
}

/// `connect(fd, *sockaddr_in, addrlen)`.
///
/// The sockaddr is validated before anything touches the network: `addrlen`
/// must be exactly 16, every byte must be in a user-readable page, the family
/// must be `AF_INET`, and the port is read big-endian. IPv4 literals only —
/// there is no DNS in this layer. The call polls the NIC until the three-way
/// handshake completes or the TCP client's own deadline expires.
fn connect(fd: usize, address: usize, length: usize) -> isize {
    let (connected, bound) = match lookup(fd) {
        Ok(OpenFd::Socket {
            connected, bound, ..
        }) => (connected, bound),
        Ok(OpenFd::Listener { .. }) | Ok(OpenFd::TcpConn { .. }) => return EISCONN,
        Ok(OpenFd::UdpSocket { .. }) => return ENOTSUP,
        Ok(OpenFd::Unsupported) => return ENOTSUP,
        Ok(_) => return ENOTSOCK,
        Err(error) => return error,
    };
    if bound != 0 {
        // `connect` on a bound-but-not-listening stream socket is not part of
        // this ABI's small surface: the port reservation exists for `listen`.
        return EINVAL;
    }
    if connected {
        return EISCONN;
    }
    if length != SOCKADDR_IN_SIZE || address == 0 {
        return if address == 0 { fault() } else { EINVAL };
    }
    let (family, ip, port) = match read_sockaddr_in(address) {
        Ok(parsed) => parsed,
        Err(error) => return error,
    };
    if family != AF_INET {
        return EAFNOSUPPORT;
    }
    match with_network_interrupts(|| tcp::connect(ip, port)) {
        Ok(()) => {
            if let Some(id) = tcp::active_id() {
                mark_socket_connected(fd, id);
            }
            CONNECTS.fetch_add(1, Ordering::Relaxed);
            0
        }
        Err(error) => connect_errno(error),
    }
}

/// `sendto(fd, buf, len, flags, dest, addrlen)` and the `send` shape.
///
/// On a connected stream socket `dest`/`addrlen` are optional; when present
/// they are validated and then ignored, because the kernel's single connection
/// is already bound to its peer, and `flags` must be 0. TCP's `send` is
/// synchronous (it waits for the ACK with a bounded poll), so a completed call
/// means the bytes left this machine.
///
/// On a UDP socket `dest` is required and `flags` may be 0 or `MSG_DONTWAIT`
/// (the send path never waits once ARP is resolved, so the flag is accepted and
/// ignored). An unbound socket is auto-bound to an ephemeral port on its first
/// send, exactly like BSD. A datagram larger than [`UDP_DATAGRAM_MAX`] is
/// `-EMSGSIZE`; there is no fragmentation or truncation.
fn send_socket(
    fd: usize,
    buf: usize,
    len: usize,
    flags: usize,
    dest: usize,
    addrlen: usize,
) -> isize {
    match lookup(fd) {
        Ok(OpenFd::UdpSocket { port, .. }) => send_udp(fd, port, buf, len, flags, dest, addrlen),
        Ok(OpenFd::Socket {
            id,
            connected: true,
            ..
        })
        | Ok(OpenFd::TcpConn { id }) => {
            if flags != 0 {
                return EINVAL;
            }
            if dest != 0 {
                if addrlen != SOCKADDR_IN_SIZE {
                    return EINVAL;
                }
                match read_sockaddr_in(dest) {
                    Ok((family, _, _)) if family == AF_INET => {}
                    Ok(_) => return EAFNOSUPPORT,
                    Err(error) => return error,
                }
            }
            if len == 0 {
                return 0;
            }
            write_socket(buf, len, id)
        }
        Ok(OpenFd::Socket { .. }) | Ok(OpenFd::Listener { .. }) => ENOTCONN,
        Ok(OpenFd::Unsupported) => ENOTSUP,
        // `send` on a TLS descriptor encrypts; a destination is refused for the
        // same reason `recv` refuses a source: the session already has a peer.
        Ok(OpenFd::Tls { session, .. }) => {
            if flags != 0 {
                return EINVAL;
            }
            if dest != 0 {
                return ENOTSUP;
            }
            tls_write_session(session, buf, len)
        }
        Ok(_) => ENOTSOCK,
        Err(error) => error,
    }
}

/// The UDP half of `sendto`. The destination is mandatory; a bad sockaddr is
/// rejected before any state changes, and the local port is allocated before
/// the first byte leaves.
fn send_udp(
    fd: usize,
    port: u16,
    buf: usize,
    len: usize,
    flags: usize,
    dest: usize,
    addrlen: usize,
) -> isize {
    if flags & !MSG_DONTWAIT != 0 {
        return EINVAL;
    }
    if dest == 0 {
        return EDESTADDRREQ;
    }
    if addrlen != SOCKADDR_IN_SIZE {
        return EINVAL;
    }
    let (family, ip, remote_port) = match read_sockaddr_in(dest) {
        Ok(parsed) => parsed,
        Err(error) => return error,
    };
    if family != AF_INET {
        return EAFNOSUPPORT;
    }
    if remote_port == 0 {
        return EINVAL;
    }
    if len > crate::net::UDP_DATAGRAM_MAX {
        return EMSGSIZE;
    }
    if len == 0 {
        return 0;
    }
    let mut payload = [0u8; crate::net::UDP_DATAGRAM_MAX];
    if let Err(error) = copy_from_user(&mut payload[..len], buf) {
        return error;
    }
    if crate::net::config().is_none() {
        return ENETUNREACH;
    }
    let local_port = if port != 0 {
        port
    } else {
        let chosen = match crate::net::udp_bind(0) {
            Ok(port) => port,
            Err(crate::net::UdpBindError::AddressInUse) => return EADDRINUSE,
            Err(crate::net::UdpBindError::NoSlots) => return ENOSPC,
        };
        if !set_udp_port(fd, chosen) {
            crate::net::udp_unbind(chosen);
            return EBADF;
        }
        chosen
    };
    if !with_network_interrupts(|| {
        crate::net::udp_send_from(local_port, ip, remote_port, &payload[..len])
    }) {
        return EHOSTUNREACH;
    }
    SOCKET_SENDS.fetch_add(1, Ordering::Relaxed);
    len as isize
}

/// `recvfrom(fd, buf, len, flags, src, addrlen)` and the `recv` shape.
///
/// Stream sockets keep their non-blocking contract: `flags` must be 0,
/// `-EAGAIN` means "poll again", and 0 means the peer closed.
///
/// UDP sockets block by default (polling the NIC with interrupts enabled, the
/// same bounded loop as TCP's `connect`/`send`) until a datagram arrives, the
/// ~10 s budget expires, or a signal is pending; `MSG_DONTWAIT` (or the
/// descriptor's `SOCK_NONBLOCK` bit) makes it a single poll and `-EAGAIN`.
fn recv_socket(
    fd: usize,
    buf: usize,
    len: usize,
    flags: usize,
    src: usize,
    addrlen: usize,
) -> isize {
    match lookup(fd) {
        Ok(OpenFd::UdpSocket { port, nonblocking }) => {
            recv_udp(port, nonblocking, buf, len, flags, src, addrlen)
        }
        Ok(OpenFd::Socket {
            id,
            connected: true,
            ..
        })
        | Ok(OpenFd::TcpConn { id }) => {
            if flags != 0 {
                return EINVAL;
            }
            if src != 0 {
                if addrlen == 0 {
                    return EINVAL;
                }
                let capacity = match read_u32_user(addrlen) {
                    Ok(value) => value,
                    Err(error) => return error,
                };
                if capacity < SOCKADDR_IN_SIZE as u32 {
                    return EINVAL;
                }
                let Some((ip, port)) = tcp::peer_id(id) else {
                    return ENOTCONN;
                };
                if let Err(error) = write_sockaddr_in(src, ip, port) {
                    return error;
                }
                if let Err(error) = write_u32_user(addrlen, SOCKADDR_IN_SIZE as u32) {
                    return error;
                }
            }
            if len == 0 {
                return 0;
            }
            read_socket(buf, len, id)
        }
        Ok(OpenFd::Socket { .. }) | Ok(OpenFd::Listener { .. }) => ENOTCONN,
        Ok(OpenFd::Unsupported) => ENOTSUP,
        // `recv` on a TLS descriptor decrypts one buffer of plaintext. The
        // optional source address is refused rather than filled with a guess:
        // `tls_read` reports bytes, not peers, and inventing one would be worse
        // than saying no.
        Ok(OpenFd::Tls { session, .. }) => {
            if flags != 0 {
                return EINVAL;
            }
            if src != 0 {
                return ENOTSUP;
            }
            tls_read_session(session, buf, len)
        }
        Ok(_) => ENOTSOCK,
        Err(error) => error,
    }
}

/// The UDP half of `recvfrom`: validate the destination buffers first, then
/// wait for one datagram (or poll once when non-blocking), copy at most `len`
/// bytes and fill the optional source address.
fn recv_udp(
    port: u16,
    nonblocking: bool,
    buf: usize,
    len: usize,
    flags: usize,
    src: usize,
    addrlen: usize,
) -> isize {
    if flags & !MSG_DONTWAIT != 0 {
        return EINVAL;
    }
    if len > 0 {
        let Some(end) = buf.checked_add(len) else {
            return fault();
        };
        if !user_range_valid(buf, end, true) {
            return fault();
        }
    }
    if src != 0 {
        if addrlen == 0 {
            return EINVAL;
        }
        let capacity = match read_u32_user(addrlen) {
            Ok(value) => value,
            Err(error) => return error,
        };
        if capacity < SOCKADDR_IN_SIZE as u32 {
            return EINVAL;
        }
        let Some(source_end) = src.checked_add(SOCKADDR_IN_SIZE) else {
            return fault();
        };
        let Some(length_end) = addrlen.checked_add(4) else {
            return fault();
        };
        if !user_range_valid(src, source_end, true) || !user_range_valid(addrlen, length_end, true)
        {
            return fault();
        }
    }
    if port == 0 {
        return EINVAL;
    }
    if len == 0 {
        return 0;
    }
    // Receive into one fixed kernel buffer rather than returning a 1 KiB
    // `Datagram` by value: the debug kernel has a 16 KiB task stack, and the
    // poll path already carries a frame buffer and a queue temporary.
    let mut buffer = [0u8; crate::net::UDP_DATAGRAM_MAX];
    let wanted = len.min(buffer.len());
    let received = if nonblocking || flags & MSG_DONTWAIT != 0 {
        crate::net::poll();
        crate::net::udp_take_into(port, &mut buffer[..wanted])
    } else {
        with_network_interrupts(|| {
            crate::net::udp_wait_into(port, &mut buffer[..wanted], UDP_RECV_TICKS)
        })
    };
    let Some((src_ip, src_port, data_len)) = received else {
        return EAGAIN;
    };
    let take = wanted.min(data_len);
    if take > 0 {
        if let Err(error) = copy_to_user(buf, &buffer[..take]) {
            return error;
        }
    }
    if src != 0 {
        if let Err(error) = write_sockaddr_in(src, src_ip, src_port) {
            return error;
        }
        if let Err(error) = write_u32_user(addrlen, SOCKADDR_IN_SIZE as u32) {
            return error;
        }
    }
    SOCKET_RECVS.fetch_add(1, Ordering::Relaxed);
    take as isize
}

/// The body of `read`/`recv`/`recvfrom` on a stream socket: drain at most one
/// buffer of payload, copying it page by page to the user.
fn read_socket(buf: usize, len: usize, id: u8) -> isize {
    let mut chunk = [0u8; SOCKET_CHUNK];
    let mut total = 0usize;
    while total < len {
        let want = (len - total).min(SOCKET_CHUNK);
        let received = match tcp::recv_nonblocking_id(id, &mut chunk[..want]) {
            Ok(0) => break,
            Ok(received) if received <= want => received,
            Ok(_) => return if total > 0 { total as isize } else { EIO },
            Err(tcp::Error::WouldBlock) => {
                return if total > 0 { total as isize } else { EAGAIN };
            }
            Err(error) => {
                return if total > 0 {
                    total as isize
                } else {
                    stream_errno(error)
                };
            }
        };
        if let Err(error) = copy_to_user(buf + total, &chunk[..received]) {
            return if total > 0 { total as isize } else { error };
        }
        total += received;
        if received < want {
            break;
        }
    }
    if total > 0 {
        SOCKET_RECVS.fetch_add(1, Ordering::Relaxed);
    }
    total as isize
}

/// The body of `write`/`send`/`sendto` on a stream socket: copy user bytes in
/// MSS-sized pieces and hand each to the synchronous TCP send. A failure after
/// some bytes left returns the short count, never a false success.
fn write_socket(buf: usize, len: usize, id: u8) -> isize {
    let mut chunk = [0u8; SOCKET_CHUNK];
    let mut total = 0usize;
    while total < len {
        let want = (len - total).min(SOCKET_CHUNK);
        if let Err(error) = copy_from_user(&mut chunk[..want], buf + total) {
            return if total > 0 { total as isize } else { error };
        }
        match with_network_interrupts(|| tcp::send_id(id, &chunk[..want])) {
            Ok(sent) if sent <= want => {
                total += sent;
                if sent < want {
                    break;
                }
            }
            Ok(_) => return if total > 0 { total as isize } else { EIO },
            Err(error) => {
                return if total > 0 {
                    total as isize
                } else {
                    stream_errno(error)
                };
            }
        }
    }
    if total > 0 {
        TLS_WRITES.fetch_add(1, Ordering::Relaxed);
        SOCKET_SENDS.fetch_add(1, Ordering::Relaxed);
    }
    total as isize
}

/// `shutdown(fd, how)`.
///
/// The stack has no half-close: `how` (0 = read, 1 = write, 2 = both) is
/// validated and then both directions are closed, exactly like `close` minus
/// the descriptor. The fd stays open so the ABI stays BSD-shaped: a later `read`
/// returns 0 (end of stream) and a later `write` is `-EPIPE`; `close` then
/// releases the slot.
fn shutdown(fd: usize, how: usize) -> isize {
    if how > 2 {
        return EINVAL;
    }
    let id = match lookup(fd) {
        Ok(OpenFd::Socket {
            id,
            connected: true,
            ..
        })
        | Ok(OpenFd::TcpConn { id }) => id,
        // A TLS session shuts down by sending `close_notify`: both directions
        // close, the descriptor stays open, a later `read` returns 0 and a later
        // `write` is `-EPIPE` — the same contract the socket path documents, with
        // the alert as the TLS half of it.
        Ok(OpenFd::Tls { session, .. }) => {
            return match with_network_interrupts(|| crate::net::tls::user_shutdown(session)) {
                Ok(()) => {
                    SHUTDOWNS.fetch_add(1, Ordering::Relaxed);
                    0
                }
                Err(error) => tls_errno(error),
            };
        }
        Ok(OpenFd::Socket { .. }) | Ok(OpenFd::Listener { .. }) => return ENOTCONN,
        Ok(OpenFd::UdpSocket { .. }) => return ENOTCONN,
        Ok(OpenFd::Unsupported) => return ENOTSUP,
        Ok(_) => return ENOTSOCK,
        Err(error) => return error,
    };
    with_network_interrupts(|| tcp::shutdown_id(id));
    SHUTDOWNS.fetch_add(1, Ordering::Relaxed);
    0
}

/// `setsockopt(fd, level, optname, optval, optlen)`.
///
/// This kernel has nothing to tune: the options a BSD client commonly sets
/// (`SO_REUSEADDR`, `SO_KEEPALIVE`, the two socket timeouts and
/// `TCP_NODELAY`) are validated and ignored. Anything else is `-ENOPROTOOPT`,
/// so a program can tell "accepted no-op" from "not implemented".
fn setsockopt(fd: usize, level: usize, name: usize, value: usize, length: usize) -> isize {
    match lookup(fd) {
        Ok(OpenFd::Socket { .. })
        | Ok(OpenFd::Listener { .. })
        | Ok(OpenFd::TcpConn { .. })
        | Ok(OpenFd::Tls { .. })
        | Ok(OpenFd::UdpSocket { .. }) => {}
        Ok(OpenFd::Unsupported) => return ENOTSUP,
        Ok(_) => return ENOTSOCK,
        Err(error) => return error,
    }
    match (level, name) {
        (SOL_SOCKET, SO_REUSEADDR)
        | (SOL_SOCKET, SO_KEEPALIVE)
        | (SOL_SOCKET, SO_RCVTIMEO)
        | (SOL_SOCKET, SO_SNDTIMEO)
        | (IPPROTO_TCP, TCP_NODELAY) => {}
        _ => return ENOPROTOOPT,
    }
    if length > 0 {
        if value == 0 {
            return fault();
        }
        // Validate the option buffer even though it is ignored: a bad pointer
        // is the caller's bug, not a silent no-op.
        let mut scratch = [0u8; 16];
        let take = length.min(scratch.len());
        if let Err(error) = copy_from_user(&mut scratch[..take], value) {
            return error;
        }
    }
    SOCKOPTS.fetch_add(1, Ordering::Relaxed);
    0
}

/// `getsockopt(fd, level, optname, optval, *optlen)`.
///
/// Answers the small fixed set with the values this stack actually has:
/// `SO_TYPE` is `SOCK_STREAM` or `SOCK_DGRAM`, `SO_ERROR` is 0 (connect reports
/// its error directly), and the two timeouts read back as zero (the kernel's
/// bounds are fixed). The caller's `optlen` is read first and the reply is
/// truncated to it, then `optlen` is updated to the bytes written.
fn getsockopt(fd: usize, level: usize, name: usize, value: usize, length: usize) -> isize {
    let socket_type = match lookup(fd) {
        Ok(OpenFd::Socket { .. })
        | Ok(OpenFd::Listener { .. })
        | Ok(OpenFd::TcpConn { .. })
        | Ok(OpenFd::Tls { .. }) => SOCK_STREAM,
        Ok(OpenFd::UdpSocket { .. }) => SOCK_DGRAM,
        Ok(OpenFd::Unsupported) => return ENOTSUP,
        Ok(_) => return ENOTSOCK,
        Err(error) => return error,
    };
    if value == 0 || length == 0 {
        return fault();
    }
    let capacity = match read_u32_user(length) {
        Ok(value) => value as usize,
        Err(error) => return error,
    };
    let mut reply = [0u8; 16];
    let size = match (level, name) {
        (SOL_SOCKET, SO_TYPE) => {
            reply[..4].copy_from_slice(&socket_type.to_le_bytes());
            4
        }
        (SOL_SOCKET, SO_ERROR) => 4,
        (SOL_SOCKET, SO_RCVTIMEO) | (SOL_SOCKET, SO_SNDTIMEO) => 16,
        (IPPROTO_TCP, TCP_NODELAY) => 4,
        _ => return ENOPROTOOPT,
    };
    let take = capacity.min(size);
    if take > 0 {
        if let Err(error) = copy_to_user(value, &reply[..take]) {
            return error;
        }
    }
    if let Err(error) = write_u32_user(length, take as u32) {
        return error;
    }
    SOCKOPTS.fetch_add(1, Ordering::Relaxed);
    0
}

// ─── userland TLS (1100–1103) ──────────────────────────────────────────────

/// Longest host name `tls_connect` accepts, including no terminator: a DNS name
/// is at most 253 bytes and the ABI's own path limit is 255, so one constant
/// covers both and keeps the copy a stack buffer.
const TLS_HOST_MAX: usize = 255;

/// Bytes copied from/to a user buffer per `tls_write`/`tls_read` chunk. One page
/// keeps the trap stack small; a larger request is answered with a short count
/// and the caller loops, exactly like the socket path's `SOCKET_CHUNK`.
const TLS_COPY_CHUNK: usize = 4096;

/// Map a session failure onto the ABI's errno set. Exhaustive on purpose: a new
/// [`crate::net::tls::SessionError`] variant must pick a number here, so the
/// mapping cannot drift silently.
///
/// `NoEntropy` is `-EAGAIN` ("try again later") because that is what Linux's
/// `getrandom` answers without a seed, and the seed is a boot-time property;
/// `NoClock` is `-ENOTSUP` because a machine with no RTC cannot do TLS at all
/// rather than "not yet". `Certificate` is `-EKEYREJECTED` — Linux's "the key
/// was rejected by service" — so a caller can tell "the server is not who it
/// claims" from "the handshake broke" (`-EPROTO`).
fn tls_errno(error: crate::net::tls::SessionError) -> isize {
    use crate::net::tls::SessionError;
    match error {
        SessionError::NoSession => EINVAL,
        SessionError::Busy => EAGAIN,
        SessionError::NoSpace => ENOSPC,
        SessionError::NoEntropy => EAGAIN,
        SessionError::NoClock => ENOTSUP,
        SessionError::Handshake | SessionError::Protocol => EPROTO,
        SessionError::Certificate => EKEYREJECTED,
        SessionError::Closed => EPIPE,
        SessionError::Reset => ECONNRESET,
        SessionError::Timeout => ETIMEDOUT,
        SessionError::Io => EIO,
    }
}

/// `tls_connect(tcp_fd, host, host_len, flags)` — syscall 1100.
///
/// Perform the fail-closed TLS 1.3 handshake on the stream socket `tcp_fd`
/// (a connected client socket or an accepted connection) and turn the
/// descriptor into a TLS session. `host` is **not** resolved and no connection
/// is opened: the caller connects the socket first, exactly as it would for
/// plain TCP, and passes the name the certificate must authorize (SNI + SAN).
///
/// What is verified, in order, before a single application byte can move:
/// the entropy pool has a seed, the RTC gives a wall clock, the certificate
/// chain reaches the committed CA bundle, the SAN authorizes `host`, and the
/// handshake's `CertificateVerify` proves possession of the leaf key — plus the
/// independent check that a certificate was verified during *this* handshake.
/// There is no flag that relaxes any of it: `flags` must be 0 and anything else
/// is `-EINVAL`, which is also what keeps a future "insecure" bit from slipping
/// in without a documented ABI change.
///
/// The descriptor must be a **connected stream**: an unconnected socket is
/// `-ENOTCONN`, a listener `-EISCONN`, a UDP socket `-ENOTSUP`, a non-socket
/// `-ENOTSOCK`, a bad number `-EBADF`. `host` must be 1..=[`TLS_HOST_MAX`] bytes
/// with no NUL or control byte (`-EINVAL`), in a user-readable page (`-EFAULT`).
///
/// On success the same fd number now carries the session: `read`/`write`/
/// `send`/`recv`/`poll` treat it as plaintext, `shutdown` sends `close_notify`,
/// and `tls_close`/`close` release it. On failure RFC 8446 requires the
/// connection to be abandoned, so the socket is torn down and the descriptor is
/// left as a tombstone (`-ENOTSUP` for I/O, `close` releases it): close it and
/// open a new socket to retry.
fn tls_connect(fd: usize, host: usize, host_len: usize, flags: usize) -> isize {
    if flags != 0 {
        return EINVAL;
    }
    if host_len == 0 || host_len > TLS_HOST_MAX {
        return EINVAL;
    }
    let Some(end) = host.checked_add(host_len) else {
        return fault();
    };
    if !user_range_valid(host, end, false) {
        return fault();
    }
    let mut buffer = [0u8; TLS_HOST_MAX];
    if let Err(error) = copy_from_user(&mut buffer[..host_len], host) {
        return error;
    }
    let Ok(name) = core::str::from_utf8(&buffer[..host_len]) else {
        return EINVAL;
    };
    // A host name is handed to the SNI extension and to the SAN matcher; a NUL
    // or a control byte in it could only mean the caller meant something else.
    if name
        .bytes()
        .any(|byte| byte == 0 || byte < 0x20 || byte == 0x7f)
    {
        return EINVAL;
    }

    let tcp_id = match lookup(fd) {
        Ok(OpenFd::Socket {
            id,
            connected: true,
            ..
        })
        | Ok(OpenFd::TcpConn { id }) => id,
        Ok(OpenFd::Socket { .. }) => return ENOTCONN,
        Ok(OpenFd::Listener { .. }) | Ok(OpenFd::Tls { .. }) => return EISCONN,
        Ok(OpenFd::UdpSocket { .. }) => return ENOTSUP,
        Ok(OpenFd::Unsupported) => return ENOTSUP,
        Ok(_) => return ENOTSOCK,
        Err(error) => return error,
    };

    match with_network_interrupts(|| crate::net::tls::user_open(tcp_id, name)) {
        Ok(session) => {
            let installed = with_fds(|table| {
                let slot = table.slots.get_mut(fd)?;
                // The old entry is just data (no `Drop`): its TCP connection is
                // now owned by the session, not closed by the replacement.
                *slot = Some(FdEntry::Tls {
                    session,
                    tcp: tcp_id,
                });
                Some(())
            })
            .flatten()
            .is_some();
            if !installed {
                // Unreachable in practice: the descriptor was looked up a moment
                // ago and this task owns it. Release everything rather than leak.
                crate::net::tls::user_close(session, false);
                with_network_interrupts(|| crate::net::tcp::close_id(tcp_id));
                return EBADF;
            }
            TLS_CONNECTS.fetch_add(1, Ordering::Relaxed);
            0
        }
        Err(error) => {
            // Fail closed: abandon the connection (RFC 8446 §6.1) and leave a
            // tombstone so the descriptor number still closes cleanly.
            let _ = with_fds(|table| {
                if let Some(slot) = table.slots.get_mut(fd) {
                    *slot = Some(FdEntry::Unsupported);
                }
            });
            with_network_interrupts(|| crate::net::tcp::close_id(tcp_id));
            tls_errno(error)
        }
    }
}

/// `tls_write(fd, buf, len)` — syscall 1101.
///
/// Encrypt `len` plaintext bytes from `buf` into TLS records and hand them to
/// the connection. Blocks (bounded by the session's idle budget) until the
/// record layer accepted them; the return value is the number of plaintext
/// bytes accepted, which is `len` unless the connection failed part-way.
///
/// `-EINVAL` when `fd` is not a TLS session, `-EFAULT` for a bad buffer (the
/// whole range is validated first, so a failure never means "some bytes went"),
/// `-EPIPE` after `tls_close`/`shutdown` or a received `close_notify`,
/// `-ECONNRESET` for a reset or a stream that ended without `close_notify`,
/// `-ETIMEDOUT` when the peer stops acknowledging, `-EAGAIN` when another thread
/// of this process is already inside a call on the same session.
fn tls_write(fd: usize, buf: usize, len: usize) -> isize {
    let session = match lookup(fd) {
        Ok(OpenFd::Tls { session, .. }) => session,
        // Not a TLS descriptor: there is no session to write on. The ABI names
        // one answer for every such descriptor rather than guessing which
        // socket-ish errno fits better.
        Ok(_) => return EINVAL,
        Err(error) => return error,
    };
    tls_write_session(session, buf, len)
}

/// The shared body of `tls_write` and of `write`/`send` on a TLS descriptor.
fn tls_write_session(session: u8, buf: usize, len: usize) -> isize {
    if len == 0 {
        return 0;
    }
    let Some(end) = buf.checked_add(len) else {
        return fault();
    };
    if !user_range_valid(buf, end, false) {
        return fault();
    }

    let mut chunk = [0u8; TLS_COPY_CHUNK];
    let mut total = 0usize;
    while total < len {
        let want = (len - total).min(TLS_COPY_CHUNK);
        if let Err(error) = copy_from_user(&mut chunk[..want], buf + total) {
            return if total > 0 { total as isize } else { error };
        }
        match with_network_interrupts(|| crate::net::tls::user_write(session, &chunk[..want])) {
            Ok(written) => {
                total += written;
                if written < want {
                    break;
                }
            }
            Err(error) => {
                return if total > 0 {
                    total as isize
                } else {
                    tls_errno(error)
                }
            }
        }
    }
    if total > 0 {
        SOCKET_SENDS.fetch_add(1, Ordering::Relaxed);
    }
    total as isize
}

/// `tls_read(fd, buf, len)` — syscall 1102.
///
/// Decrypt application data into `buf` and return the byte count. **Blocks**
/// (bounded by the session's idle budget) until at least one byte is available,
/// then returns up to [`TLS_COPY_CHUNK`] of what it has — a caller loops until
/// it sees `0`, which is end-of-stream (`close_notify` or the peer closing the
/// TCP connection cleanly).
///
/// `-EINVAL` when `fd` is not a TLS session, `-EFAULT` for a bad buffer,
/// `-ECONNRESET` for a reset or a truncated stream (a TCP FIN **without**
/// `close_notify` is *not* reported as 0 — that is the truncation RFC 8446
/// §6.1 warns about), `-ETIMEDOUT` when the idle budget expires, `-EAGAIN` for
/// a concurrent call by another thread of the same process.
fn tls_read(fd: usize, buf: usize, len: usize) -> isize {
    let session = match lookup(fd) {
        Ok(OpenFd::Tls { session, .. }) => session,
        Ok(_) => return EINVAL,
        Err(error) => return error,
    };
    tls_read_session(session, buf, len)
}

/// The shared body of `tls_read` and of `read`/`recv` on a TLS descriptor.
fn tls_read_session(session: u8, buf: usize, len: usize) -> isize {
    if len == 0 {
        return 0;
    }
    let Some(end) = buf.checked_add(len) else {
        return fault();
    };
    if !user_range_valid(buf, end, true) {
        return fault();
    }

    let mut chunk = [0u8; TLS_COPY_CHUNK];
    let want = len.min(TLS_COPY_CHUNK);
    match with_network_interrupts(|| crate::net::tls::user_read(session, &mut chunk[..want])) {
        Ok(0) => 0,
        Ok(read) => {
            if let Err(error) = copy_to_user(buf, &chunk[..read]) {
                return error;
            }
            TLS_READS.fetch_add(1, Ordering::Relaxed);
            SOCKET_RECVS.fetch_add(1, Ordering::Relaxed);
            read as isize
        }
        Err(error) => tls_errno(error),
    }
}

/// `tls_close(fd)` — syscall 1103.
///
/// Close a TLS session: send `close_notify` (best effort, bounded by the idle
/// budget), release the record buffers and the key schedule, close the
/// underlying TCP connection and release the descriptor. `0` means the session
/// is gone — it does not promise the peer received the alert; a failure to
/// deliver it is printed on the serial line and leaves the descriptor closed
/// all the same.
///
/// Unlike `close(2)` (which releases a TLS descriptor *without* the alert, since
/// `close` also runs at process teardown and may not wait for a peer), this is
/// the graceful close. `-EINVAL` when `fd` is not a TLS session; a
/// non-descriptor is `-EBADF`.
fn tls_close(fd: usize) -> isize {
    let entry = take_slot(fd);
    match entry {
        Some(FdEntry::Tls { session, tcp }) => {
            with_network_interrupts(|| crate::net::tls::user_close(session, true));
            with_network_interrupts(|| crate::net::tcp::close_id(tcp));
            TLS_CLOSES.fetch_add(1, Ordering::Relaxed);
            0
        }
        // Not a TLS descriptor: put it back untouched and say so. Taking it out
        // first and restoring it keeps this a single table operation.
        Some(entry) => {
            let restored = put_slot(fd, entry);
            debug_assert!(restored);
            EINVAL
        }
        None => EBADF,
    }
}

// ─── open / close ──────────────────────────────────────────────────────────

/// `openat(dirfd, path, flags, mode)`.
///
/// `dirfd` is ignored: the ABI takes absolute paths only, and `path` is checked
/// to start with `/`. `O_CREAT` creates through the parent directory's
/// `create_file`; `O_TRUNC` truncates to zero and is refused on a read-only
/// open; `O_APPEND` makes every write start at the current size.
///
/// `mode` is accepted and **ignored** on create: the RISC-V `VfsNode` trait has
/// no `set_mode`, so the filesystem's default permissions (ext2 0o644) stand.
/// That is the documented limitation, not a silently dropped promise.
fn openat(dirfd: usize, path_ptr: usize, flags: usize, mode: usize) -> isize {
    let _ = dirfd;
    let _ = mode;
    let access = flags & O_ACCMODE;
    if access == O_ACCMODE {
        return EINVAL;
    }
    let writable = access == O_WRONLY || access == O_RDWR;
    if flags & O_TRUNC != 0 && !writable {
        // POSIX leaves this undefined; refusing beats truncating a file the
        // caller cannot then write.
        return EINVAL;
    }

    let mut path_buf = [0u8; PATH_CAPACITY];
    let length = match read_user_path(path_ptr, &mut path_buf) {
        Ok(length) => length,
        Err(error) => return error,
    };
    let Ok(path) = core::str::from_utf8(&path_buf[..length]) else {
        // The VFS is UTF-8 by design (its names are `String`s); a byte path
        // that is not UTF-8 has no node it could name.
        return EINVAL;
    };
    if !path.starts_with('/') {
        return EINVAL;
    }

    // Set when this call created the entry: `(parent, leaf)` is what a later
    // failure must remove again.
    let mut created: Option<(Arc<dyn VfsNode>, String)> = None;
    let node = match vfs::walk(path, true) {
        Ok(node) => node,
        Err(VfsError::NotFound) if flags & O_CREAT != 0 => {
            let (parent_path, leaf) = match vfs::parent_and_leaf(path) {
                Ok(parts) => parts,
                Err(error) => return errno(error),
            };
            let parent = match vfs::walk(&parent_path, true) {
                Ok(parent) => parent,
                Err(error) => return errno(error),
            };
            if !parent.is_directory() {
                return ENOTDIR;
            }
            match parent.create_file(&leaf) {
                Ok(node) => {
                    created = Some((parent, leaf));
                    node
                }
                Err(error) => return errno(error),
            }
        }
        Err(error) => return errno(error),
    };

    let mut dir = None;
    if node.is_directory() {
        if access != O_RDONLY || flags & (O_CREAT | O_TRUNC) != 0 {
            return EISDIR;
        }
        // Snapshot the children now: the stream `getdents64` returns is the
        // directory as it was at open, and no VFS call is made while its
        // records are being encoded.
        match node.readdir() {
            Ok(entries) => dir = Some(DirSnapshot { entries, index: 0 }),
            Err(error) => return errno(error),
        }
    }

    // Install first, then truncate. A descriptor that cannot be installed (all
    // 16 slots open) must not leave a newly created file behind, and a failed
    // truncation must not leave a descriptor the program never saw — the second
    // is rolled back below, the first by removing the entry here.
    let truncate = !node.is_directory() && flags & O_TRUNC != 0;
    let fd = match install(Arc::clone(&node), flags, dir) {
        Ok(fd) => fd,
        Err(error) => {
            if let Some((parent, leaf)) = created.as_ref() {
                // Best effort: if this fails the filesystem is already unable
                // to remove what it just created, and the open error is the
                // more useful answer. No counter is bumped: this is rollback,
                // not a user's `unlinkat`.
                let _ = parent.remove(leaf);
            }
            return error;
        }
    };
    if truncate {
        if let Err(error) = node.truncate(0) {
            release_slot(fd);
            return match error {
                VfsError::NotSupported => ENOTSUP,
                error => errno(error),
            };
        }
    }
    OPENS.fetch_add(1, Ordering::Relaxed);
    fd as isize
}

/// Remove a descriptor without counting a `close`; used by [`openat`] to roll
/// back a descriptor whose truncation failed. `false` means the slot was empty.
fn release_slot(fd: usize) -> bool {
    let entry = take_slot(fd);
    match entry {
        Some(entry) => {
            drop_entry(entry);
            true
        }
        None => false,
    }
}

/// `close(fd)`: release the descriptor. Closing 0/1/2 is allowed and permanent
/// for this run (a later read/write is `-EBADF`); the runner resets the table
/// for the next program.
///
/// A socket descriptor also closes the kernel's TCP connection (FIN when the
/// handshake completed, otherwise a plain reset of the stack's state). The entry
/// is removed before the network call so the fd table lock is never held across
/// a segment send or a NIC poll.
fn close(fd: usize) -> isize {
    let entry = take_slot(fd);
    match entry {
        None => EBADF,
        Some(entry) => {
            drop_entry(entry);
            CLOSES.fetch_add(1, Ordering::Relaxed);
            0
        }
    }
}

// ─── lseek / fstat / getdents64 ────────────────────────────────────────────

/// `lseek(fd, offset, whence)`: 0 = SET, 1 = CUR, 2 = END.
///
/// Offsets are signed and 64-bit; a negative result is `-EINVAL`, an offset past
/// `isize::MAX` is `-EOVERFLOW` (the ABI returns the new offset in `a0`).
/// Seeking is refused on the console (not seekable) and on a directory (its
/// `getdents64` cursor is the snapshot index, not a byte offset).
fn lseek(fd: usize, offset: usize, whence: usize) -> isize {
    let delta = offset as isize as i128;
    match lookup(fd) {
        Ok(OpenFd::Unsupported) => ENOTSUP,
        Ok(OpenFd::ConsoleIn)
        | Ok(OpenFd::ConsoleOut)
        | Ok(OpenFd::Pipe { .. })
        | Ok(OpenFd::UdpSocket { .. })
        | Ok(OpenFd::Socket { .. })
        | Ok(OpenFd::Listener { .. })
        | Ok(OpenFd::TcpConn { .. })
        | Ok(OpenFd::Tls { .. }) => EINVAL,
        Ok(OpenFd::File { node, offset, .. }) => {
            if node.is_directory() {
                return EINVAL;
            }
            let base = match whence {
                0 => 0i128,
                1 => offset as i128,
                2 => node.size() as i128,
                _ => return EINVAL,
            };
            let new = base + delta;
            if new < 0 {
                return EINVAL;
            }
            if new > isize::MAX as i128 {
                return EOVERFLOW;
            }
            set_offset(fd, new as u64);
            SEEKS.fetch_add(1, Ordering::Relaxed);
            new as isize
        }
        Err(error) => error,
    }
}

/// `struct Stat` from the ABI: `u32 mode`, `u32 kind`, `u64 size`, `u64 ino`.
const STAT_SIZE: usize = 24;
const KIND_FILE: u32 = 0;
const KIND_DIR: u32 = 1;
const KIND_SYMLINK: u32 = 2;
const KIND_OTHER: u32 = 3;

/// `fstat(fd, *Stat)`: fill the native 24-byte structure from the `VfsNode`.
///
/// `mode` is the permission bits only. A `fchmodat` override recorded for this
/// run wins (see [`ModeTable`]); otherwise an ext2 node reports its real mode
/// and a synthetic node (the root, a mount wrapper) has none, so the documented
/// defaults are 0o755 for a directory, 0o777 for a symlink and 0o644 for a file.
/// `ino` is `VfsNode::fs_ino` or 0 for a synthetic node. The three console
/// descriptors answer `kind = OTHER`, size 0, ino 0, mode 0o644.
fn fstat(fd: usize, pointer: usize) -> isize {
    let (kind, mode, size, ino) = match lookup(fd) {
        Ok(OpenFd::ConsoleIn) | Ok(OpenFd::ConsoleOut) => (KIND_OTHER, 0o644u32, 0u64, 0u64),
        Ok(OpenFd::Unsupported) => return ENOTSUP,
        // A pipe reports `OTHER`, no inode and the bytes currently queued as
        // its size: there is no filesystem object behind it, and the ABI's
        // `Stat` is the only metadata query a pipe has.
        Ok(OpenFd::Pipe { id, .. }) => (
            KIND_OTHER,
            0o600u32,
            PIPES
                .lock()
                .pipes
                .get(id)
                .filter(|pipe| pipe.in_use)
                .map_or(0, |pipe| pipe.len as u64),
            0u64,
        ),
        Ok(OpenFd::Socket { .. })
        | Ok(OpenFd::Listener { .. })
        | Ok(OpenFd::TcpConn { .. })
        | Ok(OpenFd::Tls { .. })
        | Ok(OpenFd::UdpSocket { .. }) => (KIND_OTHER, 0o644u32, 0u64, 0u64),
        Ok(OpenFd::File { node, .. }) => {
            let is_dir = node.is_directory();
            let is_link = node.is_symlink();
            let kind = if is_dir {
                KIND_DIR
            } else if is_link {
                KIND_SYMLINK
            } else {
                KIND_FILE
            };
            let ino = node.fs_ino();
            let declared = (node.mode() & 0o7777) as u32;
            let default = if declared != 0 {
                declared
            } else if is_dir {
                0o755
            } else if is_link {
                0o777
            } else {
                0o644
            };
            let mode = mode_override(ino).map_or(default, u32::from);
            (kind, mode, node.size(), ino)
        }
        Err(error) => return error,
    };

    let mut stat = [0u8; STAT_SIZE];
    stat[0..4].copy_from_slice(&mode.to_le_bytes());
    stat[4..8].copy_from_slice(&kind.to_le_bytes());
    stat[8..16].copy_from_slice(&size.to_le_bytes());
    stat[16..24].copy_from_slice(&ino.to_le_bytes());
    if let Err(error) = copy_to_user(pointer, &stat) {
        return error;
    }
    FSTATS.fetch_add(1, Ordering::Relaxed);
    0
}

/// Longest `Dirent` this layer can encode: a 12-byte header (ino, kind,
/// name_len, reserved) plus a 255-byte name, padded so the next record starts on
/// an 8-byte boundary. ext2's own name limit is the same 255 bytes.
const DIRENT_MAX: usize = (12 + 255 + 7) & !7;

/// `getdents64(fd, buf, len)`: encode the directory snapshot as the ABI's
/// `Dirent` records, 8-byte aligned.
///
/// One record is `u64 ino`, `u8 kind`, `u8 name_len`, `u16 reserved`,
/// `name[name_len]`, then zero padding to an 8-byte boundary. The header is 12
/// bytes, so a 16-byte name occupies 28 bytes padded to 32 — the old
/// `8 + align(name)` shape truncated every name longer than 12 bytes. Records are
/// produced until the next whole record does not fit in the caller's remaining
/// buffer; no record is ever split. The cursor advances only for records that
/// were copied, so a short buffer does not lose an entry.
///
/// Limitation, stated rather than hidden: if not even one record fits, the
/// result is 0 — the same value as end-of-directory. The caller must use a
/// buffer large enough for the longest name (264 bytes); the ABI itself defines
/// the record this way.
fn getdents64(fd: usize, buf: usize, len: usize) -> isize {
    if len == 0 {
        return 0;
    }
    if buf.checked_add(len).is_none() {
        return fault();
    }
    let Some(slot) = crate::user::process::current_slot() else {
        return EBADF;
    };
    let mut tables = FDS.lock();
    let Some(Some(FdEntry::File(file))) = tables.rows[slot].slots.get_mut(fd) else {
        return EBADF;
    };
    let Some(dir) = file.dir.as_mut() else {
        return ENOTDIR;
    };

    let mut record = [0u8; DIRENT_MAX];
    let mut total = 0usize;
    while dir.index < dir.entries.len() {
        let entry = &dir.entries[dir.index];
        let name = entry.name().as_bytes();
        if name.len() > 255 {
            return if total > 0 {
                total as isize
            } else {
                ENAMETOOLONG
            };
        }
        let record_len = (12 + name.len() + 7) & !7;
        if record_len > len - total {
            break;
        }
        // `ino`, `kind`, `name_len`, `reserved`; the padding keeps the next
        // record 8-byte aligned.
        record[..record_len].fill(0);
        record[0..8].copy_from_slice(&entry.fs_ino().to_le_bytes());
        record[8] = if entry.is_directory() {
            KIND_DIR as u8
        } else if entry.is_symlink() {
            KIND_SYMLINK as u8
        } else {
            KIND_FILE as u8
        };
        record[9] = name.len() as u8;
        record[12..12 + name.len()].copy_from_slice(name);
        if let Err(error) = copy_to_user(buf + total, &record[..record_len]) {
            return if total > 0 { total as isize } else { error };
        }
        total += record_len;
        dir.index += 1;
        DIRENTS.fetch_add(1, Ordering::Relaxed);
    }
    total as isize
}

// ─── readlinkat / time / uname / brk ──────────────────────────────────────

/// `readlinkat(dirfd, path, buf, len)`: the target of a symbolic link, without a
/// terminating NUL, truncated to `len`.
///
/// `dirfd` is ignored (absolute paths only). The lookup is `vfs::lookup_path`,
/// which does *not* follow a final link — the whole point of the call — while
/// intermediate links are still resolved. A path that is not a link is
/// `-EINVAL`; a link whose target cannot be read is `-EIO`.
fn readlinkat(dirfd: usize, path_ptr: usize, buf: usize, len: usize) -> isize {
    let _ = dirfd;
    let mut path_buf = [0u8; PATH_CAPACITY];
    let length = match read_user_path(path_ptr, &mut path_buf) {
        Ok(length) => length,
        Err(error) => return error,
    };
    let Ok(path) = core::str::from_utf8(&path_buf[..length]) else {
        return EINVAL;
    };
    if !path.starts_with('/') {
        return EINVAL;
    }
    let node = match vfs::lookup_path(path) {
        Ok(node) => node,
        Err(error) => return errno(error),
    };
    if !node.is_symlink() {
        return EINVAL;
    }
    let Some(target) = node.read_link() else {
        return EIO;
    };
    let bytes = target.as_bytes();
    let take = bytes.len().min(len);
    if take > 0 {
        if let Err(error) = copy_to_user(buf, &bytes[..take]) {
            return error;
        }
    }
    READLINKS.fetch_add(1, Ordering::Relaxed);
    take as isize
}

// ─── path mutations: mkdirat / unlinkat / symlinkat / linkat / renameat2 /
//     fchmodat / ftruncate ─────────────────────────────────────────────────

/// Resolve the parent directory of `path` for the create/remove/rename family.
///
/// `vfs::parent_and_leaf` refuses `""`, `"."` and `".."` as the leaf (none of
/// them can be created or removed); the parent is walked with `follow_final =
/// true`, because a parent reached through a symbolic link is still a directory.
/// A parent that resolves to a non-directory is `-ENOTDIR`, the same answer the
/// filesystem would give but decided before any mutation is attempted.
fn parent_dir(path: &str) -> Result<(Arc<dyn VfsNode>, String), isize> {
    let (parent_path, leaf) = vfs::parent_and_leaf(path).map_err(errno)?;
    let parent = vfs::walk(&parent_path, true).map_err(errno)?;
    if !parent.is_directory() {
        return Err(ENOTDIR);
    }
    Ok((parent, leaf))
}

/// `mkdirat(dirfd, path, mode)`.
///
/// `dirfd` is ignored: the ABI takes absolute paths only. `mode` is accepted and
/// **ignored**, exactly like `openat`'s: the RISC-V [`VfsNode`] trait has no
/// `set_mode`, so ext2's default `0o755` directory mode stands. That is the
/// documented limitation ([`fchmodat`] records an observable override for the
/// current run, but nothing writes the on-disk inode).
fn mkdirat(dirfd: usize, path_ptr: usize, mode: usize) -> isize {
    let _ = (dirfd, mode);
    let mut path_buf = [0u8; PATH_CAPACITY];
    let path = match read_path(path_ptr, &mut path_buf) {
        Ok(path) => path,
        Err(error) => return error,
    };
    let (parent, leaf) = match parent_dir(path) {
        Ok(parts) => parts,
        Err(error) => return error,
    };
    match parent.create_dir(&leaf) {
        Ok(_) => {
            MKDIRS.fetch_add(1, Ordering::Relaxed);
            0
        }
        Err(error) => errno(error),
    }
}

/// `unlinkat(dirfd, path, flags)`.
///
/// `flags = 0` removes a file or a symbolic link (the final component is not
/// followed: `rm link` removes the link); a directory is `-EISDIR`.
/// `flags = AT_REMOVEDIR (0x200)` removes an *empty* directory: a file is
/// `-ENOTDIR` and a directory with entries is `-ENOTEMPTY`. Any other flag bit
/// is `-EINVAL`. The type is checked before the mutation because the underlying
/// ext2 unlink accepts an empty directory and a file alike — the flag is the
/// policy, not the filesystem.
fn unlinkat(dirfd: usize, path_ptr: usize, flags: usize) -> isize {
    let _ = dirfd;
    if flags & !AT_REMOVEDIR != 0 {
        return EINVAL;
    }
    let mut path_buf = [0u8; PATH_CAPACITY];
    let path = match read_path(path_ptr, &mut path_buf) {
        Ok(path) => path,
        Err(error) => return error,
    };
    let (parent, leaf) = match parent_dir(path) {
        Ok(parts) => parts,
        Err(error) => return error,
    };
    // `lookup_path` (not `walk`) is deliberate: the link itself is the entry to
    // remove, so following a final symlink would target the wrong node.
    let node = match vfs::lookup_path(path) {
        Ok(node) => node,
        Err(error) => return errno(error),
    };
    let wants_directory = flags & AT_REMOVEDIR != 0;
    if wants_directory && !node.is_directory() {
        return ENOTDIR;
    }
    if !wants_directory && node.is_directory() {
        return EISDIR;
    }
    match parent.remove(&leaf) {
        Ok(()) => {
            UNLINKS.fetch_add(1, Ordering::Relaxed);
            0
        }
        // ext2 reports a non-empty directory as `AlreadyExists` (its own
        // internal name for "the rmdir precondition failed"); the ABI has the
        // precise errno, so translate it here instead of leaking `-EEXIST`.
        Err(VfsError::AlreadyExists) if wants_directory => ENOTEMPTY,
        Err(error) => errno(error),
    }
}

/// `symlinkat(target, dirfd, path)`: create `path` as a symbolic link whose
/// target is the literal bytes of `target`.
///
/// `dirfd` is ignored. The target is a raw string, copied through
/// [`read_user_path`] and stored verbatim: it is never resolved here, so a
/// relative target stays relative to the directory containing the link (the
/// VFS resolves it at walk time). An empty target is `-EINVAL`; a target that
/// does not fit ext2's link layout is `-EINVAL` from the VFS.
fn symlinkat(target_ptr: usize, dirfd: usize, path_ptr: usize) -> isize {
    let _ = dirfd;
    let mut target_buf = [0u8; PATH_CAPACITY];
    let target_length = match read_user_path(target_ptr, &mut target_buf) {
        Ok(length) => length,
        Err(error) => return error,
    };
    if target_length == 0 {
        return EINVAL;
    }
    let mut path_buf = [0u8; PATH_CAPACITY];
    let path = match read_path(path_ptr, &mut path_buf) {
        Ok(path) => path,
        Err(error) => return error,
    };
    let (parent, leaf) = match parent_dir(path) {
        Ok(parts) => parts,
        Err(error) => return error,
    };
    match parent.create_symlink(&leaf, &target_buf[..target_length]) {
        Ok(_) => {
            SYMLINKS.fetch_add(1, Ordering::Relaxed);
            0
        }
        Err(error) => errno(error),
    }
}

/// `linkat(olddirfd, oldpath, newdirfd, newpath, flags)`: a hard link.
///
/// Both dirfds are ignored; `flags` must be 0 (the ABI defines no
/// `AT_SYMLINK_FOLLOW`), otherwise `-EINVAL`. The existing entry is resolved
/// with `lookup_path`, so a link to a symbolic link links the link itself; a
/// directory target is `-EISDIR` (ext2 has no directory hard links).
fn linkat(
    olddirfd: usize,
    old_path_ptr: usize,
    newdirfd: usize,
    new_path_ptr: usize,
    flags: usize,
) -> isize {
    let _ = (olddirfd, newdirfd);
    if flags != 0 {
        return EINVAL;
    }
    let mut old_buf = [0u8; PATH_CAPACITY];
    let old_path = match read_path(old_path_ptr, &mut old_buf) {
        Ok(path) => path,
        Err(error) => return error,
    };
    let mut new_buf = [0u8; PATH_CAPACITY];
    let new_path = match read_path(new_path_ptr, &mut new_buf) {
        Ok(path) => path,
        Err(error) => return error,
    };
    let target = match vfs::lookup_path(old_path) {
        Ok(node) => node,
        Err(error) => return errno(error),
    };
    if target.is_directory() {
        return EISDIR;
    }
    let (parent, leaf) = match parent_dir(new_path) {
        Ok(parts) => parts,
        Err(error) => return error,
    };
    match parent.link(&leaf, &target) {
        Ok(_) => {
            LINKS.fetch_add(1, Ordering::Relaxed);
            0
        }
        Err(error) => errno(error),
    }
}

/// `renameat2(olddirfd, oldpath, newdirfd, newpath, flags)`.
///
/// Both dirfds are ignored; `flags` must be 0 (no `RENAME_NOREPLACE`/
/// `RENAME_EXCHANGE` in this ABI), otherwise `-EINVAL`. Both paths are checked
/// first — the source must exist and the destination parent must be a
/// directory — and then [`vfs::rename`] does the move in one filesystem
/// transaction: either the tree shows the new name or it still shows the old
/// one, never both and never neither. A cross-filesystem rename is
/// `-ENOTSUP` (copying is not renaming).
fn renameat2(
    olddirfd: usize,
    old_path_ptr: usize,
    newdirfd: usize,
    new_path_ptr: usize,
    flags: usize,
) -> isize {
    let _ = (olddirfd, newdirfd);
    if flags != 0 {
        return EINVAL;
    }
    let mut old_buf = [0u8; PATH_CAPACITY];
    let old_path = match read_path(old_path_ptr, &mut old_buf) {
        Ok(path) => path,
        Err(error) => return error,
    };
    let mut new_buf = [0u8; PATH_CAPACITY];
    let new_path = match read_path(new_path_ptr, &mut new_buf) {
        Ok(path) => path,
        Err(error) => return error,
    };
    // Check both ends before the transaction: a missing source is `-ENOENT`
    // and a destination inside a file is `-ENOTDIR`, both without touching the
    // filesystem.
    if let Err(error) = vfs::lookup_path(old_path) {
        return errno(error);
    }
    if let Err(error) = parent_dir(new_path) {
        return error;
    }
    match vfs::rename(old_path, new_path) {
        Ok(()) => {
            RENAMES.fetch_add(1, Ordering::Relaxed);
            0
        }
        Err(error) => errno(error),
    }
}

/// `fchmodat(dirfd, path, mode, flags)`: record `mode & 0o7777` as this run's
/// permissions for the inode behind `path`.
///
/// `dirfd` is ignored; `flags` must be 0 (`AT_SYMLINK_NOFOLLOW` is not part of
/// this ABI, so asking for it is `-EINVAL` rather than a silent follow). The
/// path is resolved with `follow_final = true`, like `chmod`.
///
/// The VFS has no `set_mode`, so the mode is stored in [`ModeTable`] and
/// [`fstat`] reports it until the run ends. It is **not** written to the ext2
/// inode and no kernel access check consults it; a synthetic node (`ino == 0`,
/// the root and a bare mount wrapper) has no inode to key on and is
/// `-ENOTSUP`. The ABI document states the same limit.
fn fchmodat(dirfd: usize, path_ptr: usize, mode: usize, flags: usize) -> isize {
    let _ = dirfd;
    if flags != 0 {
        return EINVAL;
    }
    let mut path_buf = [0u8; PATH_CAPACITY];
    let path = match read_path(path_ptr, &mut path_buf) {
        Ok(path) => path,
        Err(error) => return error,
    };
    let node = match vfs::walk(path, true) {
        Ok(node) => node,
        Err(error) => return errno(error),
    };
    let ino = node.fs_ino();
    if ino == 0 {
        return ENOTSUP;
    }
    match mode_set(ino, (mode & 0o7777) as u16) {
        Ok(()) => {
            CHMODS.fetch_add(1, Ordering::Relaxed);
            0
        }
        Err(error) => error,
    }
}

/// `ftruncate(fd, length)`: resize the regular file open on `fd`.
///
/// The descriptor must be a writable regular file: 0/1/2 and any console
/// descriptor are `-EINVAL`, a directory is `-EISDIR`, and a descriptor opened
/// read-only is `-EBADF` (the same answer [`write`] gives). The resize goes to
/// [`VfsNode::truncate`]; ext2 commits it as one transaction, so an error (a
/// length past the filesystem's 4 GiB limit is `-EINVAL`) leaves the file as it
/// was and exposes no partial state. The open offset is untouched, as POSIX
/// requires.
fn ftruncate(fd: usize, length: usize) -> isize {
    if fd < 3 {
        return EINVAL;
    }
    match lookup(fd) {
        Ok(OpenFd::Unsupported) => ENOTSUP,
        Ok(OpenFd::ConsoleIn)
        | Ok(OpenFd::ConsoleOut)
        | Ok(OpenFd::Pipe { .. })
        | Ok(OpenFd::UdpSocket { .. })
        | Ok(OpenFd::Socket { .. })
        | Ok(OpenFd::Listener { .. })
        | Ok(OpenFd::TcpConn { .. })
        | Ok(OpenFd::Tls { .. }) => EINVAL,
        Ok(OpenFd::File { node, flags, .. }) => {
            if node.is_directory() {
                return EISDIR;
            }
            if flags & O_ACCMODE == O_RDONLY {
                return EBADF;
            }
            match node.truncate(length as u64) {
                Ok(()) => {
                    TRUNCATES.fetch_add(1, Ordering::Relaxed);
                    0
                }
                Err(error) => errno(error),
            }
        }
        Err(error) => error,
    }
}

/// `struct Timespec` from the ABI: `i64 seconds`, `i64 nanos`.
const TIMESPEC_SIZE: usize = 16;

/// Nanoseconds in one second, and nanoseconds per kernel tick's worth of
/// conversion. Kept as integers: the ABI's `Timespec` is exact, the kernel clock
/// is 100 Hz, and the rounding rule is documented on [`nanosleep`].
const NANOS_PER_SEC: u128 = 1_000_000_000;

/// `clock_gettime(clk, *Timespec)`.
///
/// Clock ids 0 (realtime/uptime) and 1 (monotonic) are both the kernel's tick
/// clock: this machine has no wall-clock source, and the ABI documents the two
/// as "monotonic wall-ish". The resolution is [`crate::arch::riscv64::timer::TICK_MS`],
/// so `nsec` is always a multiple of one millisecond. Any other id is `-EINVAL`.
fn clock_gettime(clock: usize, pointer: usize) -> isize {
    if clock > 1 {
        return EINVAL;
    }
    let milliseconds = crate::arch::riscv64::timer::uptime_ms();
    let seconds = (milliseconds / 1000) as i64;
    let nanos = ((milliseconds % 1000) * 1_000_000) as i64;
    let mut timespec = [0u8; TIMESPEC_SIZE];
    timespec[0..8].copy_from_slice(&seconds.to_le_bytes());
    timespec[8..16].copy_from_slice(&nanos.to_le_bytes());
    if let Err(error) = copy_to_user(pointer, &timespec) {
        return error;
    }
    CLOCKS.fetch_add(1, Ordering::Relaxed);
    0
}

/// `nanosleep(*Timespec, rem)`: wait for the requested duration, rounded *up*
/// to the next 10 ms tick; `rem` is accepted and ignored (the ABI says so, and
/// the wait never returns early).
///
/// The trap handler runs with `sstatus.SIE = 0`, so `timer::sleep_ticks` — which
/// waits for the tick counter to move and parks in `wfi` — would wait forever
/// without enabling interrupts around the wait. This function is the one
/// syscall that does that, deliberately and with no lock held: a timer
/// interrupt inside the wait is handled and may even switch this task out, and
/// the nested trap frame lives on this task's own kernel stack (the scheduler's
/// documented contract). `sstatus` is restored from the entry value afterwards,
/// so the `sret` still resumes U-mode with interrupts on.
fn nanosleep(pointer: usize, remaining: usize) -> isize {
    let _ = remaining;
    let mut timespec = [0u8; TIMESPEC_SIZE];
    if let Err(error) = copy_from_user(&mut timespec, pointer) {
        return error;
    }
    let seconds = i64::from_le_bytes(timespec[0..8].try_into().unwrap_or([0; 8]));
    let nanos = i64::from_le_bytes(timespec[8..16].try_into().unwrap_or([0; 8]));
    if seconds < 0 || nanos < 0 || nanos >= NANOS_PER_SEC as i64 {
        return EINVAL;
    }
    let total = (seconds as u128)
        .saturating_mul(NANOS_PER_SEC)
        .saturating_add(nanos as u128);
    let ticks = total
        .saturating_mul(crate::arch::riscv64::timer::TICK_HZ as u128)
        .div_ceil(NANOS_PER_SEC)
        .min(u64::MAX as u128) as u64;
    if ticks == 0 {
        return 0;
    }
    let was = csr::read_sstatus();
    csr::write_sstatus(was | csr::SSTATUS_SIE);
    // One tick at a time, so a signal that arrives mid-sleep is noticed within
    // one tick period instead of after the whole duration. `sleep_ticks(1)` is
    // the same wait loop the one-shot call used, with the same absolute tick
    // deadline semantics summed over the requested count.
    let mut left = ticks;
    let mut interrupted = false;
    while left > 0 {
        crate::arch::riscv64::timer::sleep_ticks(1);
        left -= 1;
        if crate::user::signal::interrupted() {
            interrupted = true;
            break;
        }
    }
    csr::write_sstatus(was);
    SLEEPS.fetch_add(1, Ordering::Relaxed);
    if interrupted {
        EINTR
    } else {
        0
    }
}

/// `struct Uname` from the ABI: five 16-byte fields.
const UNAME_SIZE: usize = 80;
const UNAME_FIELD: usize = 16;

/// Copy `text` into field `index` of an 80-byte `Uname`, truncated to 16 bytes
/// and never writing past the struct. `text` must be ASCII (the ABI's fields
/// are).
fn put_uname_field(buf: &mut [u8; UNAME_SIZE], index: usize, text: &[u8]) {
    let start = index * UNAME_FIELD;
    let take = text.len().min(UNAME_FIELD);
    buf[start..start + take].copy_from_slice(&text[..take]);
}

/// `uname(*Uname)`: system identity for libpagh.
///
/// The release string is the ABI baseline (`0.10.0`); `version` reports the
/// package version, so the two are not silently conflated. Both change in the
/// release commit that bumps `Cargo.toml`.
fn uname(pointer: usize) -> isize {
    let mut buffer = [0u8; UNAME_SIZE];
    put_uname_field(&mut buffer, 0, b"pagh");
    put_uname_field(&mut buffer, 1, b"pagh-riscv");
    put_uname_field(&mut buffer, 2, b"0.10.0");
    put_uname_field(&mut buffer, 3, env!("CARGO_PKG_VERSION").as_bytes());
    put_uname_field(&mut buffer, 4, b"riscv64");
    if let Err(error) = copy_to_user(pointer, &buffer) {
        return error;
    }
    UNAMES.fetch_add(1, Ordering::Relaxed);
    0
}

// ─── entropy ───────────────────────────────────────────────────────────────

/// `getrandom(buf, len, flags)`: fill a user buffer from the kernel entropy
/// pool ([`crate::entropy`]) — the same SHA-256 ratchet that seeds every TLS
/// session key, so userland and the kernel's own TLS client draw from one pool
/// and one 256-bit `/chosen/rng-seed`.
///
/// # Contract
///
/// * `flags` — the ABI defines **no** flags: anything but `0` is `-EINVAL`.
///   Linux's `GRND_NONBLOCK`/`GRND_RANDOM`/`GRND_INSECURE` are not implemented,
///   and refusing them is the fail-closed answer: an ignored `GRND_INSECURE`
///   would hand a caller bytes it believes are unpredictable.
/// * `-EAGAIN` and nothing written — the pool was never seeded (no
///   `/chosen/rng-seed` in the device tree). This is the same gate that makes a
///   TLS handshake refuse; there is no "fall back to the counters" path.
/// * a **short read** — at most [`MAX_GETRANDOM`] bytes are returned per call,
///   so a larger request gets that many bytes and a count below `len`. Linux
///   documents the same behaviour beyond 256 bytes; the caller loops.
/// * `-EFAULT` with nothing written — the whole requested range is validated
///   (user-readable *and* -writable) before the first byte is generated, so a
///   bad pointer can never leave a half-filled buffer behind.
/// * `len == 0` — `0`, and no counter moves.
///
/// Every byte comes from [`crate::entropy::fill`], which ratchets the pool
/// state after each block: two calls never return the same stream, and learning
/// a later block says nothing about earlier ones.
fn getrandom(buf: usize, len: usize, flags: usize) -> isize {
    if flags != 0 {
        return EINVAL;
    }
    if len == 0 {
        return 0;
    }
    let take = len.min(MAX_GETRANDOM);
    let Some(end) = buf.checked_add(take) else {
        return fault();
    };
    if !user_range_valid(buf, end, true) {
        return fault();
    }
    if !crate::entropy::is_available() {
        return EAGAIN;
    }

    let mut chunk = [0u8; COPY_CHUNK];
    let mut total = 0usize;
    while total < take {
        let want = (take - total).min(COPY_CHUNK);
        if !crate::entropy::fill(&mut chunk[..want]) {
            // The pool became unavailable between the check and the draw. The
            // bytes already delivered are real random bytes; report them as a
            // short read instead of pretending the call failed.
            return total as isize;
        }
        if let Err(error) = copy_to_user(buf + total, &chunk[..want]) {
            return if total > 0 { total as isize } else { error };
        }
        total += want;
    }

    GETRANDOMS.fetch_add(1, Ordering::Relaxed);
    RANDOM_BYTES.fetch_add(total as u64, Ordering::Relaxed);
    total as isize
}

// ─── path helper ───────────────────────────────────────────────────────────

/// Copy a NUL-terminated user path into `out`, returning its length without the
/// NUL.
///
/// The copy stops at the first NUL and is bounded by `out.len()`; a string that
/// does not terminate inside the buffer is `-ENAMETOOLONG`, a bad page
/// `-EFAULT`. Reading a whole page beyond the NUL is harmless (the page is
/// validated) and keeps this to one `SUM` window per page rather than per byte.
fn read_user_path(pointer: usize, out: &mut [u8; PATH_CAPACITY]) -> Result<usize, isize> {
    let mut length = 0usize;
    while length < out.len() {
        let Some(source) = pointer.checked_add(length) else {
            return Err(fault());
        };
        let take = (out.len() - length).min(FRAME_SIZE - (source & (FRAME_SIZE - 1)));
        copy_from_user(&mut out[length..length + take], source)?;
        if let Some(nul) = out[length..length + take]
            .iter()
            .position(|byte| *byte == 0)
        {
            return Ok(length + nul);
        }
        length += take;
    }
    Err(ENAMETOOLONG)
}

/// Read an absolute, UTF-8 path from a user pointer into `out`.
///
/// This is the one shape every path-taking syscall shares: [`read_user_path`]
/// copies up to the NUL (bounded, with `-EFAULT` on a bad page and
/// `-ENAMETOOLONG` past the buffer), then the bytes must be UTF-8 — the VFS's
/// names are `String`s, so a byte path that is not UTF-8 cannot name a node —
/// and absolute, because `dirfd` is ignored and there is no cwd in the kernel.
fn read_path<'a>(pointer: usize, out: &'a mut [u8; PATH_CAPACITY]) -> Result<&'a str, isize> {
    let length = read_user_path(pointer, out)?;
    let Ok(path) = core::str::from_utf8(&out[..length]) else {
        return Err(EINVAL);
    };
    if !path.starts_with('/') {
        return Err(EINVAL);
    }
    Ok(path)
}

// ─── user pointers ─────────────────────────────────────────────────────────

/// True when every page covering `[start, end)` is mapped for U-mode read (and
/// write, when `write` is set).
///
/// `end == start` is trivially true. `paging::flags` walks the same tables the
/// hardware will; a leaf without `U` (a kernel page, a device window) is not a
/// user buffer even if it is mapped supervisor-RW. `W` is checked for writes
/// because `SUM` only lifts the U-permission check — a store to a read-only
/// user page (`.text`, `.rodata`) would otherwise become an S-mode page fault
/// and halt the machine.
pub(crate) fn user_range_valid(start: usize, end: usize, write: bool) -> bool {
    if end < start {
        return false;
    }
    let mut page = start & !(FRAME_SIZE - 1);
    while page < end {
        let Some(flags) = paging::flags(page) else {
            return false;
        };
        let bits = flags.bits();
        if bits & (PTE_U | PTE_R) != PTE_U | PTE_R {
            return false;
        }
        if write && bits & PTE_W == 0 {
            return false;
        }
        page = match page.checked_add(FRAME_SIZE) {
            Some(next) => next,
            None => break, // the last representable page covers the rest of `end`
        };
    }
    true
}

/// Copy `dst.len()` bytes from the user address `src` into a kernel buffer.
///
/// Validates the whole range before setting `SUM`, and restores `sstatus` from
/// the value it entered with on every path — there is no early return between
/// the two CSR writes.
pub(crate) fn copy_from_user(dst: &mut [u8], src: usize) -> Result<(), isize> {
    if dst.is_empty() {
        return Ok(());
    }
    let Some(end) = src.checked_add(dst.len()) else {
        return Err(fault());
    };
    if !user_range_valid(src, end, false) {
        return Err(fault());
    }

    let was = csr::read_sstatus();
    core::sync::atomic::compiler_fence(Ordering::SeqCst);
    csr::write_sstatus(was | csr::SSTATUS_SUM);
    core::sync::atomic::compiler_fence(Ordering::SeqCst);

    let mut offset = 0usize;
    while offset < dst.len() {
        let address = src + offset;
        let take = (dst.len() - offset).min(FRAME_SIZE - (address & (FRAME_SIZE - 1)));
        // SAFETY: every page of `[src, end)` was validated above (U=1, R=1), so
        // the load cannot fault; `take` keeps the copy inside one page. `SUM`
        // is set for exactly this loop, and `dst` is a kernel buffer disjoint
        // from user memory.
        unsafe {
            core::ptr::copy_nonoverlapping(
                address as *const u8,
                dst.as_mut_ptr().add(offset),
                take,
            );
        }
        offset += take;
    }

    core::sync::atomic::compiler_fence(Ordering::SeqCst);
    csr::write_sstatus(was);
    core::sync::atomic::compiler_fence(Ordering::SeqCst);
    Ok(())
}

/// Copy `src` to the user address `dst`. The write counterpart of
/// [`copy_from_user`], with `W=1` added to the page validation.
pub(crate) fn copy_to_user(dst: usize, src: &[u8]) -> Result<(), isize> {
    if src.is_empty() {
        return Ok(());
    }
    let Some(end) = dst.checked_add(src.len()) else {
        return Err(fault());
    };
    if !user_range_valid(dst, end, true) {
        return Err(fault());
    }

    let was = csr::read_sstatus();
    core::sync::atomic::compiler_fence(Ordering::SeqCst);
    csr::write_sstatus(was | csr::SSTATUS_SUM);
    core::sync::atomic::compiler_fence(Ordering::SeqCst);

    let mut offset = 0usize;
    while offset < src.len() {
        let address = dst + offset;
        let take = (src.len() - offset).min(FRAME_SIZE - (address & (FRAME_SIZE - 1)));
        // SAFETY: every page of `[dst, end)` was validated for user read *and*
        // write above, so the store cannot fault; `take` stays inside one page.
        // `src` is a kernel slice disjoint from user memory.
        unsafe {
            core::ptr::copy_nonoverlapping(src.as_ptr().add(offset), address as *mut u8, take);
        }
        offset += take;
    }

    core::sync::atomic::compiler_fence(Ordering::SeqCst);
    csr::write_sstatus(was);
    core::sync::atomic::compiler_fence(Ordering::SeqCst);
    Ok(())
}

// ─── VFS errors → errno ────────────────────────────────────────────────────

/// The ABI's error mapping. The VFS names a handful of conditions the ABI's
/// list does not spell out; those get the closest honest errno and are listed
/// in `docs/` rather than invented as new numbers.
fn errno(error: VfsError) -> isize {
    match error {
        VfsError::NotFound => ENOENT,
        VfsError::NotSupported => ENOTSUP,
        VfsError::InvalidArgument => EINVAL,
        VfsError::IoError | VfsError::Corrupt => EIO,
        VfsError::AlreadyExists => EEXIST,
        VfsError::NotEmpty => ENOTEMPTY,
        VfsError::IsADirectory => EISDIR,
        VfsError::NotADirectory => ENOTDIR,
        VfsError::NoSpace => ENOSPC,
        VfsError::NameTooLong => ENAMETOOLONG,
    }
}
