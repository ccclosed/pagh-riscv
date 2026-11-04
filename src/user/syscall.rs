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

use core::sync::atomic::{AtomicU64, Ordering};

use alloc::sync::Arc;

use crate::arch::riscv64::csr;
use crate::arch::riscv64::mmu::{PTE_R, PTE_U, PTE_W};
use crate::arch::riscv64::traps::TrapFrame;
use crate::memory::{paging, FRAME_SIZE};
use crate::sync::Spinlock;
use crate::vfs::{self, VfsError, VfsNode};

// ─── syscall numbers (RISC-V Linux generic, per the ABI document) ──────────
const SYS_READ: usize = 63;
const SYS_WRITE: usize = 64;
const SYS_OPENAT: usize = 56;
const SYS_CLOSE: usize = 57;
const SYS_EXIT: usize = 93;
const SYS_SCHED_YIELD: usize = 124;
const SYS_GETPID: usize = 172;

// ─── errno values (negated in `a0`) ────────────────────────────────────────
const ENOENT: isize = -2;
const EIO: isize = -5;
const EBADF: isize = -9;
const EAGAIN: isize = -11;
const EFAULT: isize = -14;
const EEXIST: isize = -17;
const ENOTDIR: isize = -20;
const EISDIR: isize = -21;
const EINVAL: isize = -22;
const ENOSPC: isize = -28;
const ENAMETOOLONG: isize = -36;
const ENOSYS: isize = -38;
const ENOTSUP: isize = -95;

// ─── `openat` flags (the ABI's own small set) ──────────────────────────────
const O_ACCMODE: usize = 0x3;
const O_RDONLY: usize = 0;
const O_WRONLY: usize = 1;
const O_RDWR: usize = 2;
const O_CREAT: usize = 0x40;
const O_TRUNC: usize = 0x80;
const O_APPEND: usize = 0x100;

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
struct FileHandle {
    node: Arc<dyn VfsNode>,
    offset: u64,
    flags: usize,
}

enum FdEntry {
    /// fd 0: console input (UART ring, then the virtio keyboard).
    Stdin,
    /// fd 1/2: console output.
    Stdout,
    File(FileHandle),
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

/// Fill the table with the three console descriptors. Called by the runner
/// before a task starts; a second call discards whatever a previous run left.
pub(crate) fn reset_fds() {
    let mut table = FDS.lock();
    *table = FdTable::empty();
    table.slots[0] = Some(FdEntry::Stdin);
    table.slots[1] = Some(FdEntry::Stdout);
    table.slots[2] = Some(FdEntry::Stdout);
}

/// Drop every open node. Called by the runner after the task is gone, before the
/// image mappings are released; `Arc::drop` is the only side effect.
pub(crate) fn release_fds() {
    *FDS.lock() = FdTable::empty();
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
        None => Err(EBADF),
    }
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
fn install(node: Arc<dyn VfsNode>, flags: usize) -> Result<usize, isize> {
    let mut table = FDS.lock();
    for fd in 3..MAX_FDS {
        if table.slots[fd].is_none() {
            table.slots[fd] = Some(FdEntry::File(FileHandle {
                node,
                offset: 0,
                flags,
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
                Ok(node) => node,
                Err(error) => return errno(error),
            }
        }
        Err(error) => return errno(error),
    };

    if node.is_directory() {
        if access != O_RDONLY || flags & (O_CREAT | O_TRUNC) != 0 {
            return EISDIR;
        }
    } else if flags & O_TRUNC != 0 {
        match node.truncate(0) {
            Ok(()) => {}
            Err(VfsError::NotSupported) => return ENOTSUP,
            Err(error) => return errno(error),
        }
    }

    match install(node, flags) {
        Ok(fd) => {
            OPENS.fetch_add(1, Ordering::Relaxed);
            fd as isize
        }
        Err(error) => error,
    }
}

/// `close(fd)`: release the descriptor. Closing 0/1/2 is allowed and permanent
/// for this run (a later read/write is `-EBADF`); the runner resets the table
/// for the next program.
fn close(fd: usize) -> isize {
    let mut table = FDS.lock();
    match table.slots.get_mut(fd) {
        Some(slot) => {
            if slot.take().is_some() {
                CLOSES.fetch_add(1, Ordering::Relaxed);
                0
            } else {
                EBADF
            }
        }
        // `get_mut` on `fd >= 16` is `None`; an already-closed slot is handled
        // by the `take` above.
        None => EBADF,
    }
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
        VfsError::NotEmpty => EINVAL,
        VfsError::IsADirectory => EISDIR,
        VfsError::NotADirectory => ENOTDIR,
        VfsError::NoSpace => ENOSPC,
        VfsError::NameTooLong => ENAMETOOLONG,
    }
}
