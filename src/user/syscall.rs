//! The native system-call ABI (libpagh): the RISC-V syscall numbers from
//! `pagh-api/docs/abi.md`, a trap frame that carries the arguments, a per-run
//! file-descriptor table, and pointer validation that never trusts a U-mode
//! address.
//!
//! Dispatch runs in the trap handler, with `sstatus.SIE = 0` (hardware cleared
//! it on entry), so a syscall cannot be preempted by the timer halfway through a
//! user-buffer copy. The one voluntary switch this layer performs is
//! `sched_yield`; `nanosleep` deliberately re-enables interrupts while it waits
//! in `wfi` (see that function) and holds no lock across it.
//!
//! # The file-descriptor table
//!
//! One program runs at a time (`user::RUN`), so the table is one static with 16
//! slots, reset when a run starts and released when it ends: 0 = console input,
//! 1/2 = console output, 3.. = VFS nodes with an offset and the `openat` flags.
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
use crate::sync::Spinlock;
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
const SYS_SCHED_YIELD: usize = 124;
const SYS_GETPID: usize = 172;
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

// ─── errno values (negated in `a0`) ────────────────────────────────────────
const ENOENT: isize = -2;
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
const ENAMETOOLONG: isize = -36;
const EOVERFLOW: isize = -75;
const ENOSYS: isize = -38;
const ENOTEMPTY: isize = -39;
const ENOTSUP: isize = -95;

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
/// Pipes one run may hold open at once. Each is 4 KiB of static RAM; eight is
/// enough for a shell pipeline and bounded.
const MAX_PIPES: usize = 8;

/// File descriptors per run, including 0/1/2 (the ABI's limit).
pub(crate) const MAX_FDS: usize = 16;

/// Longest absolute path accepted by a path-taking syscall, including the NUL.
/// ext2 limits a name to 255 bytes; a longer *path* is refused with
/// `-ENAMETOOLONG` rather than truncated.
const PATH_CAPACITY: usize = 256;

/// Bytes copied from/to a user page per chunk. Small enough for the trap stack,
/// large enough that the UART, not the copy, is the cost.
const COPY_CHUNK: usize = 256;

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
        unknown: UNKNOWN.load(Ordering::Relaxed),
        faults: FAULTS.load(Ordering::Relaxed),
    }
}

/// Count a bad user pointer and hand back the ABI's answer.
fn fault() -> isize {
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

    /// Release every ring in place (no whole-table temporary).
    fn clear(&mut self) {
        for pipe in &mut self.pipes {
            pipe.reset();
        }
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
}

static FDS: Spinlock<FdTable> = Spinlock::new(FdTable::empty());

// ─── permission-mode overrides (fchmodat) ─────────────────────────────────

/// Distinct inodes one run may retarget with `fchmodat`. Sixteen covers a
/// program that chmods a handful of files; a seventeenth call is `-ENOSPC`.
const MAX_MODE_OVERRIDES: usize = 16;

#[derive(Clone, Copy)]
struct ModeOverride {
    ino: u64,
    mode: u16,
}

/// `fchmodat` has no VFS counterpart: [`VfsNode`] exposes `mode()` but no
/// `set_mode`, and ext2's writer has no permission-bit path behind the trait.
/// The syscall therefore records the new `0o7777` bits here, and [`fstat`]
/// reports them for the rest of the run — the same per-run shim the x86 tree
/// used before its filesystem grew a writable mode. The table is keyed by
/// [`VfsNode::fs_ino`] (so a hard link sees the change too), cleared with the
/// descriptor table, and deliberately not persisted: this is observable
/// metadata, not an on-disk permission change, and no access check consults it.
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

static MODES: Spinlock<ModeTable> = Spinlock::new(ModeTable::empty());

/// The override recorded for `ino`, if this run chmod'ed it. Synthetic nodes
/// (`ino == 0`) are never recorded.
fn mode_override(ino: u64) -> Option<u16> {
    if ino == 0 {
        return None;
    }
    MODES.lock().get(ino)
}

/// Fill the table with the three console descriptors. Called by the runner
/// before a task starts; a second call discards whatever a previous run left.
/// The `fchmodat` overrides are per run too, so they are cleared here.
pub(crate) fn reset_fds() {
    let mut table = FDS.lock();
    *table = FdTable::empty();
    table.slots[0] = Some(FdEntry::Stdin);
    table.slots[1] = Some(FdEntry::Stdout);
    table.slots[2] = Some(FdEntry::Stdout);
    drop(table);
    *MODES.lock() = ModeTable::empty();
    // A pipe whose descriptors a previous run forgot is dead storage, not state
    // the next program can reach: the table is per run, the rings with it.
    PIPES.lock().clear();
}

/// Drop every open node. Called by the runner after the task is gone, before the
/// image mappings are released; `Arc::drop` is the only side effect, and the
/// whole pipe table is discarded because no descriptor can outlive the run.
pub(crate) fn release_fds() {
    *FDS.lock() = FdTable::empty();
    *MODES.lock() = ModeTable::empty();
    PIPES.lock().clear();
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
}

fn lookup(fd: usize) -> Result<OpenFd, isize> {
    let table = FDS.lock();
    match table.slots.get(fd).and_then(|slot| slot.as_ref()) {
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
        None => Err(EBADF),
    }
}

/// First empty descriptor slot, including 0/1/2 once a program has closed them.
/// `dup` and `pipe2` use it, which is why they can hand back 0 after a program
/// closed its stdin — the ABI's "0/1/2 are console" describes the initial table,
/// not a permanent reservation.
fn alloc_fd() -> Option<usize> {
    let table = FDS.lock();
    table.slots.iter().position(|slot| slot.is_none())
}

/// Put an entry in `fd`, returning the previous occupant (released by the
/// caller). `None` means the slot was free or out of range.
fn take_slot(fd: usize) -> Option<FdEntry> {
    let mut table = FDS.lock();
    table.slots.get_mut(fd).and_then(|slot| slot.take())
}

/// Release one descriptor entry, dropping a pipe reference if it was one end.
fn drop_entry(entry: FdEntry) {
    if let FdEntry::Pipe { id, read } = entry {
        pipe_release(id, read);
    }
}

/// Install a pipe end in the first free slot.
fn install_pipe(id: usize, read: bool) -> Result<usize, isize> {
    let mut table = FDS.lock();
    let Some(fd) = table.slots.iter().position(|slot| slot.is_none()) else {
        return Err(ENOSPC);
    };
    table.slots[fd] = Some(FdEntry::Pipe { id, read });
    Ok(fd)
}

/// Publish the new offset of an open file. A descriptor closed in between (only
/// possible if the program itself did it, since one program runs at a time) is
/// simply not found and the offset is dropped with it.
fn set_offset(fd: usize, offset: u64) {
    let mut table = FDS.lock();
    if let Some(Some(FdEntry::File(file))) = table.slots.get_mut(fd) {
        file.offset = offset;
    }
}

/// Install a node in the first free slot at or above 3.
fn install(node: Arc<dyn VfsNode>, flags: usize, dir: Option<DirSnapshot>) -> Result<usize, isize> {
    let mut table = FDS.lock();
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
        SYS_CLOCK_GETTIME => clock_gettime(frame.a0, frame.a1),
        SYS_NANOSLEEP => nanosleep(frame.a0, frame.a1),
        SYS_UNAME => uname(frame.a0),
        SYS_BRK => match crate::user::brk(frame.a0) {
            Ok(current) => {
                BRKS.fetch_add(1, Ordering::Relaxed);
                current as isize
            }
            Err(()) => ENOMEM,
        },
        SYS_EXIT => crate::user::exit_current(frame.a0 as u32),
        SYS_SCHED_YIELD => {
            crate::task::scheduler::yield_now();
            YIELDS.fetch_add(1, Ordering::Relaxed);
            0
        }
        SYS_GETPID => {
            GETPIDS.fetch_add(1, Ordering::Relaxed);
            crate::task::scheduler::current_id() as isize
        }
        _ => {
            UNKNOWN.fetch_add(1, Ordering::Relaxed);
            ENOSYS
        }
    };
    frame.a0 = result as usize;
    // `sret` resumes at the saved `sepc`, which still points at the `ecall`
    // instruction. A returning syscall resumes at the instruction after it;
    // `exit` never reaches this line (it ends the task), so one increment
    // serves every call that returns.
    frame.sepc = frame.sepc.wrapping_add(4);
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
        Ok(OpenFd::Pipe { id, read }) => read_pipe(id, read, buf, len),
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
        Ok(OpenFd::Pipe { id, read }) => write_pipe(id, read, buf, len),
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
/// `flags` must be 0: `O_NONBLOCK` is already the only mode and `O_CLOEXEC` has
/// no meaning without `exec`, so a bit the ABI does not implement is `-EINVAL`
/// rather than silently dropped. The read end is installed first, the write end
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
    FDS.lock()
        .slots
        .get(fd)
        .and_then(|slot| slot.as_ref())
        .cloned()
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
    FDS.lock().slots[fd] = Some(entry);
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
    FDS.lock().slots[newfd] = Some(entry);
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
        // The keyboard has no routed interrupt (see the drivers README): make
        // pending key events visible before deciding nothing is ready.
        crate::drivers::virtio_input::poll();
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
    // The table lock is released before a pipe reference is dropped: the pipe
    // table is a second lock, and taking it after the descriptor table (never
    // before) is the one order every path here follows.
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
fn close(fd: usize) -> isize {
    if release_slot(fd) {
        CLOSES.fetch_add(1, Ordering::Relaxed);
        0
    } else {
        EBADF
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
        Ok(OpenFd::ConsoleIn) | Ok(OpenFd::ConsoleOut) | Ok(OpenFd::Pipe { .. }) => EINVAL,
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
    let mut table = FDS.lock();
    let Some(Some(FdEntry::File(file))) = table.slots.get_mut(fd) else {
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
    match MODES.lock().set(ino, (mode & 0o7777) as u16) {
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
        Ok(OpenFd::ConsoleIn) | Ok(OpenFd::ConsoleOut) | Ok(OpenFd::Pipe { .. }) => EINVAL,
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
    crate::arch::riscv64::timer::sleep_ticks(ticks);
    csr::write_sstatus(was);
    SLEEPS.fetch_add(1, Ordering::Relaxed);
    0
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
fn user_range_valid(start: usize, end: usize, write: bool) -> bool {
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
fn copy_from_user(dst: &mut [u8], src: usize) -> Result<(), isize> {
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
fn copy_to_user(dst: usize, src: &[u8]) -> Result<(), isize> {
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
