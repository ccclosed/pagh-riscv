//! User mode: a process model, ELF loading, U-mode execution, syscalls and
//! faults.
//!
//! # What a process is
//!
//! One [`Process`](process::Process) owns one Sv39 address space (a private root
//! table with a nonzero ASID), its image pages, a 64 KiB user stack, the `brk`
//! heap window, the `mmap` extents, one thread group, one descriptor table and
//! its signal state. `run_source` creates the first process; `fork` copies the
//! current one; `execve` replaces its image; `wait4` reaps a child. The shell
//! still starts one foreground program with `run` and waits for it, but a
//! forked child is a separate process that runs concurrently and can be waited
//! for.
//!
//! # The address space
//!
//! The kernel keeps its own root (ASID 0). [`paging::create_user_space`] makes a
//! process root by copying every *supervisor* mapping the kernel has — the RAM
//! identity map and the device windows — and no user page; the loader then maps
//! only this process's image, heap, `mmap` and stack pages into it. Two processes
//! therefore share no user frame, no table frame and no ASID, and U-mode in one
//! cannot reach the other's memory even though both roots contain the same
//! kernel map (that map has `U=0`). The scheduler installs a task's `satp`
//! before switching to it; `sscratch` and the trap-frame layout are unchanged,
//! so a trap from U-mode lands on the task's own kernel stack in the same root.
//!
//! A fork is a **deep copy** of the mapped user pages (image, heap, `mmap` and
//! stack), not copy-on-write. COW is out of scope for this MVP: both processes
//! own every frame they can touch from the first instruction, which is slower
//! and uses more memory than a COW fork, but it needs no write-fault path and
//! keeps the PMM ledger exact.
//!
//! # Run lifecycle
//!
//! [`run_image`]/[`run_file`] build a process, spawn the main thread, wait until
//! its `threads` count reaches zero and return its exit status. A process that
//! outlives the foreground — an orphan — is killed and reaped by the shell's
//! runner before `run` returns. There is no init process to adopt it, and
//! leaving it alive would let a stale user page table outlive the command that
//! created it; the ABI documents "orphans die with the foreground run" rather
//! than pretending to be POSIX.
//!
//! # Kernel stack, user stack, and `sscratch`
//!
//! A trap taken from U-mode arrives with the user's `sp`, which is not memory the
//! kernel may trust. `trap_entry` therefore keeps the task's kernel stack pointer
//! in `sscratch` while U-mode runs: the entry swaps `sp` with `sscratch` before
//! touching memory, so the 264-byte trap frame and every Rust frame below it live
//! on the task's own PMM stack. `sscratch` is 0 while S-mode runs, which is what
//! makes the same swap a no-op for kernel traps. [`enter_user`] installs the
//! kernel pointer, and the trap epilogue re-arms it on every return to U-mode.
//!
//! # Faults
//!
//! An exception whose saved `sstatus.SPP` is 0 ends the *process*, never the
//! machine ([`on_fault`]). It prints one diagnostic line, retires the other
//! threads with `scheduler::kill_group` and ends the caller; the runner or the
//! waiting parent then does the cleanup. A fault is process-fatal because a
//! thread that died holding a userspace lock would otherwise leave its siblings
//! waiting forever. S-mode exceptions keep the print-and-park contract in
//! `arch::riscv64::traps`.

pub mod elf;
pub mod futex;
pub mod process;
pub mod selftest;
pub mod signal;
pub mod syscall;

use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::arch::riscv64::mmu::{Flags, PTE_R, PTE_U};
use crate::arch::riscv64::traps::TrapFrame;
use crate::memory::paging;
use crate::memory::pmm;
use crate::memory::FRAME_SIZE;
use crate::sync::Spinlock;
use crate::task::scheduler;
use crate::vfs::{self, VfsError, VfsNode};

use process::Process;

/// Embedded hello fixture: `write(1, "hello from user mode\n")`, `sched_yield`,
/// `exit(42)`. Built by `userland/build.sh` and committed, so a clean checkout
/// runs without a disk or a cross-compiler.
pub const HELLO_ELF: &[u8] = include_bytes!("../../userland/hello.elf");

/// Embedded fault fixture: `getpid`, then a load from address 0. The kernel must
/// kill this task and keep running.
pub const FAULT_ELF: &[u8] = include_bytes!("../../userland/fault.elf");

/// Embedded timer-preemption fixture: spins in U-mode for 0.2 s (twenty tick
/// periods), then `getpid`/`exit(7)`. The selftest uses it to prove that a timer
/// interrupt taken while the hart is in U-mode is handled and returned from.
pub const SPIN_ELF: &[u8] = include_bytes!("../../userland/spin.elf");

/// Embedded entry-stack fixture: validates `argc`/`argv`/`envp` and the frame
/// layout against the ABI, prints one line and exits 0. See `userland/argv.rs`.
pub const ARGV_ELF: &[u8] = include_bytes!("../../userland/argv.elf");

/// Embedded native-ABI fixture: exercises the syscall table, the file and
/// directory mutations, sockets, threads, signals and the process family. It
/// exits 0 when everything passed, 77 when only the filesystem half was skipped
/// (no `/mnt`), and a distinct non-zero code otherwise.
pub const ABI_ELF: &[u8] = include_bytes!("../../userland/abi.elf");

/// Embedded `execve` target. The selftest writes it to `/mnt/exec-child`; the ABI
/// fixture execs that path with one argument and one environment variable and
/// the program exits 0 only when both arrived correctly.
pub const EXEC_CHILD_ELF: &[u8] = include_bytes!("../../userland/exec-child.elf");

pub use elf::{ElfError, ImageSource, SourceError, USER_STACK_BASE, USER_STACK_PAGES, USER_TOP};

/// Longest program name kept for the fault diagnostic; the shell's path may be
/// longer and is truncated rather than allocated.
const NAME_CAPACITY: usize = 32;

/// Most strings one initial stack may carry: `argv[0]`, the arguments and the
/// environment together. The shell's line is 128 bytes, so this is far above
/// anything it can produce; the limit exists because the frame builder uses a
/// fixed array and must refuse (not truncate) an oversized request.
const MAX_ENTRY_STRINGS: usize = 64;

/// Most bytes of string data (NULs included) one initial stack may carry.
///
/// The user stack is 64 KiB; leaving 48 KiB for the program's own frames is
/// plenty for a shell command line and keeps a caller that invokes
/// [`run_file`] from the kernel from filling the whole stack with pointers.
const MAX_ENTRY_BYTES: usize = 16 * 1024;

/// The user heap window (`brk`): 1 MiB at 2 GiB, one page at a time.
///
/// The window is inside the numeric user range (`0x1_0000..0x4000_0000`) but
/// above every image the loader accepts (1 MiB from `0x1_0000`) and below the
/// user stack, and no device window lives there (PLIC `0x0c00_0000`, UART
/// `0x1000_0000`, virtio below `0x1000_8000`). A crafted image that maps one of
/// these pages first gets `-ENOMEM` from `brk` instead of an overlap: `paging`
/// refuses an already-mapped leaf and the syscall rolls back.
pub const USER_HEAP_BASE: usize = 0x2000_0000;
/// Bytes of user heap the window reserves.
pub const USER_HEAP_SIZE: usize = 1 << 20;
/// One past the last heap byte (`brk` above this is `-ENOMEM`).
pub const USER_HEAP_TOP: usize = USER_HEAP_BASE + USER_HEAP_SIZE;
/// Pages in the heap window.
pub const USER_HEAP_PAGES: usize = USER_HEAP_SIZE / FRAME_SIZE;

/// The `mmap` window: 240 MiB above the `brk` heap, below the user stack.
///
/// The range is deliberately disjoint from every other user allocation the
/// runner makes: the image loader accepts pages below [`USER_STACK_BASE`], the
/// `brk` heap is the 1 MiB window at [`USER_HEAP_BASE`], and the stack is the
/// last 64 KiB before `0x4000_0000`. `mmap` still validates every candidate page
/// with `paging::flags`, so an image that happens to sit inside this window is
/// never overwritten — the window is a search range, not a reservation.
pub const USER_MMAP_BASE: usize = 0x3000_0000;
/// One past the last `mmap` byte (`0x3f00_0000`, 16 MiB below the stack base).
pub const USER_MMAP_TOP: usize = 0x3f00_0000;
/// Pages in the `mmap` window.
pub const USER_MMAP_PAGES: usize = (USER_MMAP_TOP - USER_MMAP_BASE) / FRAME_SIZE;

/// Most `mmap` regions one process may track.
///
/// A region is a contiguous extent returned by one `mmap`; a partial `munmap`
/// can split one into two. Eight extents cover ordinary allocator-shaped use
/// (one arena plus a handful of small mappings) while keeping `Process` small
/// enough for the trap stack. Running out is `-ENOMEM` *before* any page is
/// touched, never a half-applied `munmap`.
pub(crate) const MAX_MMAP_REGIONS: usize = 8;

/// One contiguous extent of anonymous private mappings owned by the process.
///
/// Teardown needs only the extent: every page is translated and freed one by
/// one, so no per-page frame array (the heap's shape) is required.
#[derive(Clone, Copy)]
pub(crate) struct MmapRegion {
    start: usize,
    pages: usize,
}

impl MmapRegion {
    pub(crate) const fn empty() -> Self {
        MmapRegion { start: 0, pages: 0 }
    }

    fn end(&self) -> usize {
        self.start + self.pages * FRAME_SIZE
    }
}

/// Why an `mmap`/`munmap`/`mprotect` call failed. The syscall layer maps these
/// to `-ENOMEM` and `-EINVAL`; keeping the enum here means the address-space
/// policy does not spell errno values of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MmapError {
    /// Bad range, nothing mapped, or no such tracked mapping.
    Invalid,
    /// The window or the frame pool has no room, or the region table is full.
    NoMemory,
}

/// How a user task ended.
///
/// The runner returns this to the shell, which prints it as the stable
/// `run: <name> exited with code N` / `run: <name> killed: <reason>` lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// `exit(2)` (syscall 93): the low 32 bits of `a0`.
    Code(u32),
    /// A U-mode exception. `reason` is the greppable name, `cause`/`sepc` the
    /// trap's own numbers (kept for the selftest and diagnostics).
    Fault {
        reason: &'static str,
        cause: usize,
        sepc: usize,
    },
}

impl fmt::Display for Exit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Exit::Code(code) => write!(f, "exited with code {code}"),
            Exit::Fault { reason, .. } => write!(f, "killed: {reason}"),
        }
    }
}

/// Why [`run_image`] could not start or finish a program.
///
/// A loader refusal is not a run: nothing was mapped by the time it is returned,
/// and the shell prints it as a command error. The scheduler variants are the
/// resource failures the launch path has (task slot, stack frames).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserError {
    /// The ELF was refused; `elf::load` has already rolled back its pages.
    Elf(ElfError),
    /// The 64 KiB user stack had no contiguous run in the PMM.
    StackFrames,
    /// A user-stack page could not be mapped; the stack run was rolled back.
    StackMap(paging::MapError),
    /// The process root table could not be built (frame pool empty).
    AddressSpace,
    /// The scheduler had no task slot or no frames for the kernel stack.
    TaskSlot,
    /// The process table has no free slot.
    ProcessSlot,
    /// The path could not be resolved through the VFS.
    Vfs(VfsError),
    /// The path names a directory, not an image file.
    IsADirectory,
    /// The file is empty or larger than [`elf::MAX_IMAGE_BYTES`].
    ImageSize(u64),
    /// The backing source refused a read while the image was loading.
    Source(SourceError),
    /// `argv`/`env` do not fit the fixed initial-stack budget: too many strings
    /// or too many bytes. Refused before anything is mapped.
    ArgumentsTooLong,
    /// The mapped user stack could not be written through `paging::translate`.
    /// Only a broken map or a race can produce this; it is an error, never a
    /// fault, because the runner is in S-mode and must survive it.
    StackFrame,
}

impl fmt::Display for UserError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UserError::Elf(error) => write!(f, "{error}"),
            UserError::StackFrames => f.write_str("no contiguous frames for the user stack"),
            UserError::StackMap(error) => write!(f, "mapping the user stack failed: {error}"),
            UserError::AddressSpace => f.write_str("no frames for a process address space"),
            UserError::TaskSlot => {
                f.write_str("the scheduler has no free slot or kernel stack for the task")
            }
            UserError::ProcessSlot => f.write_str("no free process-table slot"),
            UserError::Vfs(error) => f.write_str(error.as_str()),
            UserError::IsADirectory => f.write_str("is a directory"),
            UserError::ImageSize(size) => write!(
                f,
                "image size {} B is outside 1..={} B",
                size,
                elf::MAX_IMAGE_BYTES
            ),
            UserError::Source(error) => write!(f, "{error}"),
            UserError::ArgumentsTooLong => write!(
                f,
                "argv/env exceed {} strings or {} bytes",
                MAX_ENTRY_STRINGS, MAX_ENTRY_BYTES
            ),
            UserError::StackFrame => {
                f.write_str("the initial user stack frame could not be written")
            }
        }
    }
}

/// A fixed-capacity program name: stored in [`Process`] because the fault
/// handler runs in trap context and cannot borrow the shell's `String`.
#[derive(Clone, Copy)]
pub(crate) struct RunName {
    bytes: [u8; NAME_CAPACITY],
    len: u8,
}

impl RunName {
    pub(crate) const fn empty() -> Self {
        RunName {
            bytes: [0; NAME_CAPACITY],
            len: 0,
        }
    }

    pub(crate) fn new(name: &str) -> Self {
        let mut out = RunName::empty();
        let take = name.len().min(NAME_CAPACITY);
        out.bytes[..take].copy_from_slice(&name.as_bytes()[..take]);
        out.len = take as u8;
        out
    }

    pub(crate) fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.len as usize]).unwrap_or("?")
    }
}

/// Next process id. Monotonic for the whole boot; it is also the futex/signal
/// scope key and the scheduler group tag, so a stale entry from a killed process
/// can never match a future one.
static NEXT_PID: AtomicU64 = AtomicU64::new(1);

// ─── console capture for the ABI selftest ─────────────────────────────────
//
// The argv fixture has to prove one *exact* line, which is a property of the
// bytes `write(2)` handed to the console, not of an exit code. Capturing lives
// here rather than in `console` because only U-mode writes go through it: the
// kernel's own diagnostics stay out of the buffer by construction, and a normal
// boot pays one relaxed atomic load per user write.

/// Bytes the capture keeps; the argv line is well under 100.
const CAPTURE_CAPACITY: usize = 4096;

struct CaptureBuffer {
    len: usize,
    truncated: bool,
    bytes: [u8; CAPTURE_CAPACITY],
}

impl CaptureBuffer {
    const fn new() -> Self {
        CaptureBuffer {
            len: 0,
            truncated: false,
            bytes: [0; CAPTURE_CAPACITY],
        }
    }
}

/// A captured transcript, returned by [`capture_stop`] by value so the selftest
/// can compare it after the lock is gone.
pub struct Captured {
    pub len: usize,
    pub truncated: bool,
    pub bytes: [u8; CAPTURE_CAPACITY],
}

static CAPTURE_ON: AtomicBool = AtomicBool::new(false);
static CAPTURE: Spinlock<CaptureBuffer> = Spinlock::new(CaptureBuffer::new());

/// Start recording user-mode `write` output (the selftest is the only caller).
pub(crate) fn capture_start() {
    *CAPTURE.lock() = CaptureBuffer::new();
    CAPTURE_ON.store(true, Ordering::Release);
}

/// Stop recording and hand the bytes back. Bytes past the capacity are counted
/// as truncated, not stored: the selftest must fail a longer transcript, not
/// compare a prefix.
pub(crate) fn capture_stop() -> Captured {
    CAPTURE_ON.store(false, Ordering::Release);
    let buffer = CAPTURE.lock();
    Captured {
        len: buffer.len,
        truncated: buffer.truncated,
        bytes: buffer.bytes,
    }
}

/// Append bytes the `write` syscall just sent to the console. A no-op unless a
/// capture is running; never allocates and never blocks a trap for long.
pub(crate) fn capture_append(bytes: &[u8]) {
    if !CAPTURE_ON.load(Ordering::Acquire) {
        return;
    }
    let mut buffer = CAPTURE.lock();
    let start = buffer.len;
    let take = bytes.len().min(CAPTURE_CAPACITY - start);
    buffer.bytes[start..start + take].copy_from_slice(&bytes[..take]);
    buffer.len = start + take;
    if take < bytes.len() {
        buffer.truncated = true;
    }
}

/// A file in the VFS as an [`ImageSource`].
///
/// The loader asks only for the header, the program-header table and segment
/// bytes, so those reads go straight to `VfsNode::read`; there is no cache and
/// no whole-file copy. `size` is captured when the run starts and bounds every
/// read, so a file that shrinks mid-load is refused as a short source instead
/// of silently reading whatever now follows it.
struct FileSource {
    node: Arc<dyn VfsNode>,
    size: u64,
}

impl ImageSource for FileSource {
    fn len(&self) -> u64 {
        self.size
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, SourceError> {
        if buf.is_empty() || offset >= self.size {
            return Ok(0);
        }
        let want = core::cmp::min(buf.len() as u64, self.size - offset) as usize;
        match self.node.read(offset, &mut buf[..want]) {
            // The VFS contract is "up to buf.len()"; clamping keeps a broken node
            // from claiming bytes it never wrote.
            Ok(read) => Ok(read.min(want)),
            Err(error) => Err(SourceError::Io(error.as_str())),
        }
    }
}

/// Load `path` from the VFS and run it in U-mode until `exit` or a fault.
///
/// The path is resolved with [`vfs::walk`] (the final symlink is followed) and
/// the file is streamed by [`elf::load`]: no whole-image buffer, so any image
/// up to [`elf::MAX_IMAGE_BYTES`] (1 MiB) is accepted even though the kernel
/// heap hands out 64 KiB chunks. `path` is absolute; the shell resolves its
/// current directory before calling this.
pub fn run_file(path: &str) -> Result<Exit, UserError> {
    run_file_with_argv0(path, path, &[], &[])
}

/// [`run_file`] with arguments and an environment: `args` is `argv[1..]` and
/// `argv[0]` is `path` itself.
#[allow(dead_code)] // the path+args form is part of the kernel ABI, used by tests
pub fn run_file_with_args(path: &str, args: &[&str], env: &[&str]) -> Result<Exit, UserError> {
    run_file_with_argv0(path, path, args, env)
}

/// [`run_file`] with a separate `argv[0]`.
///
/// `open_path` is what the VFS resolves (the shell's cwd-joined path); `argv0`
/// is what the program sees in `argv[0]` (the path as the user typed it).
pub fn run_file_with_argv0(
    open_path: &str,
    argv0: &str,
    args: &[&str],
    env: &[&str],
) -> Result<Exit, UserError> {
    if 1 + args.len() + env.len() > MAX_ENTRY_STRINGS {
        return Err(UserError::ArgumentsTooLong);
    }
    let node = vfs::walk(open_path, true).map_err(UserError::Vfs)?;
    if node.is_directory() {
        return Err(UserError::IsADirectory);
    }
    let size = node.size();
    if size == 0 || size > elf::MAX_IMAGE_BYTES as u64 {
        return Err(UserError::ImageSize(size));
    }
    let source = FileSource { node, size };
    run_source(&source, argv0, args, env)
}

/// Load `bytes` and run it in U-mode until `exit` or a fault, then return how it
/// ended. The image and stack mappings are gone before this returns.
pub fn run_image(bytes: &[u8], name: &str) -> Result<Exit, UserError> {
    run_source(bytes, name, &[], &["PWD=/"])
}

/// [`run_image`] with explicit arguments (used by the ABI selftest to prove the
/// multi-argument stack without a filesystem). `args[0]` is `argv[0]`; the
/// environment is the caller's.
pub fn run_image_with_args(bytes: &[u8], args: &[&str], env: &[&str]) -> Result<Exit, UserError> {
    let Some((argv0, rest)) = args.split_first() else {
        return Err(UserError::ArgumentsTooLong);
    };
    run_source(bytes, argv0, rest, env)
}

/// The shared launch path for the shell and the embedded fixtures.
///
/// Builds a fresh address space and process, loads the image into *that* root,
/// maps the stack, builds the SysV entry frame, spawns the main thread and waits
/// for the process to finish. On every failure the pages, tables, fd row and
/// process slot are unwound; on success the process is reaped before it returns.
/// A forked child that outlives the foreground is killed and reaped by
/// [`cleanup_orphans`] before the result is returned.
fn run_source<S: ImageSource + ?Sized>(
    source: &S,
    argv0: &str,
    args: &[&str],
    env: &[&str],
) -> Result<Exit, UserError> {
    let mut argv_strings: Vec<&str> = Vec::new();
    if argv_strings.try_reserve(1 + args.len()).is_err() {
        return Err(UserError::ArgumentsTooLong);
    }
    argv_strings.push(argv0);
    argv_strings.extend_from_slice(args);
    build_entry_stack_plan(&argv_strings, env)?;

    let space = paging::create_user_space().map_err(|_| UserError::AddressSpace)?;
    let mut loaded = match elf::load_in(space.root, source, 0) {
        Ok(loaded) => loaded,
        Err(ElfError::Source(error)) => {
            paging::free_user_space(space.root);
            return Err(UserError::Source(error));
        }
        Err(error) => {
            paging::free_user_space(space.root);
            return Err(UserError::Elf(error));
        }
    };

    let Some(stack_base) = pmm::alloc_frames_contiguous(USER_STACK_PAGES) else {
        loaded.release();
        paging::free_user_space(space.root);
        return Err(UserError::StackFrames);
    };
    for page in 0..USER_STACK_PAGES {
        let va = USER_STACK_BASE + page * FRAME_SIZE;
        let pa = stack_base + page * FRAME_SIZE;
        if let Err(error) = paging::map_in(space.root, va, pa, Flags::user_rw()) {
            release_stack_in(space.root, stack_base);
            loaded.release();
            paging::free_user_space(space.root);
            return Err(UserError::StackMap(error));
        }
        // SAFETY: `pa` is a PMM frame (RAM, therefore identity-mapped supervisor
        // RW by `paging::init`) that this call just mapped as the user stack;
        // writing through the physical address, the same way the ELF loader
        // copies segment bytes, cannot fault and keeps `SUM` off. Zeroing stops
        // a U-mode program from reading a previous tenant of the frame.
        unsafe { core::ptr::write_bytes(pa as *mut u8, 0, FRAME_SIZE) };
    }

    let built = match build_entry_stack_in(space.root, &argv_strings, env) {
        Ok(frame) => frame,
        Err(error) => {
            release_stack_in(space.root, stack_base);
            loaded.release();
            paging::free_user_space(space.root);
            return Err(error);
        }
    };

    let pid = NEXT_PID.fetch_add(1, Ordering::Relaxed);
    let mut proc = Process::new(pid, 0, space.root, space.satp, space.asid);
    proc.name = RunName::new(argv0);
    proc.entry = loaded.entry;
    proc.sp = built.sp;
    proc.argc = built.argc;
    proc.argv = built.argv;
    proc.stack_base = stack_base;

    let slot = match process::publish(proc) {
        Ok(slot) => slot,
        Err(mut proc) => {
            release_process_memory(&mut proc, None);
            loaded.release();
            return Err(UserError::ProcessSlot);
        }
    };
    process::set_image(slot, loaded);
    syscall::reset_fds(slot);
    signal::begin_process(slot, pid);

    let spawned = crate::arch::riscv64::csr::without_interrupts(|| {
        let id = scheduler::spawn_in_group_satp(user_task, pid, space.satp);
        if let Some(id) = id {
            process::update(pid, |proc| {
                proc.main_tid = id;
                proc.ready = true;
            });
        }
        id
    });
    let Some(_) = spawned else {
        syscall::release_fds(slot);
        signal::clear_process(slot, pid);
        let _ = process::with_slot_mut(slot, |proc| release_process_memory(proc, Some(slot)));
        process::clear_slot(slot);
        return Err(UserError::TaskSlot);
    };

    wait_for_process(pid);
    // Every thread is `Exited`; each kernel stack is reaped on the next
    // scheduler entry. Reap explicitly so the caller's ledger does not depend on
    // when the next unrelated tick happens.
    let _ = scheduler::reap();

    let exit = process::peek(pid, |proc| proc.exit)
        .flatten()
        .unwrap_or(Exit::Fault {
            reason: "task ended without an exit status",
            cause: 0,
            sepc: 0,
        });
    // The process is gone, so its descriptors are too: a socket left open must
    // not keep the kernel's one connection alive into the next run, and the
    // `Arc<dyn VfsNode>` handles must not outlive the program. The address space
    // is released in place (a `Process` is several KiB and moving it through the
    // 16 KiB kernel stack overflowed it in an earlier version).
    syscall::release_fds(slot);
    signal::clear_process(slot, pid);
    futex::clear_run(pid);
    let _ = process::with_slot_mut(slot, |proc| release_process_memory(proc, Some(slot)));
    process::clear_slot(slot);

    // No init process: a child that outlived the foreground is not adopted, it
    // is killed and reaped here, before the shell sees its prompt again.
    cleanup_orphans();
    Ok(exit)
}

/// The three values `enter_user` needs besides the entry point.
struct EntryStack {
    /// User `sp` pointing at `argc`.
    sp: usize,
    argc: usize,
    /// User address of `argv[0]`.
    argv: usize,
}

/// Validate `argv`/`env` sizes before a single page is touched.
fn build_entry_stack_plan(argv: &[&str], env: &[&str]) -> Result<(), UserError> {
    if argv.len() + env.len() > MAX_ENTRY_STRINGS {
        return Err(UserError::ArgumentsTooLong);
    }
    let mut string_bytes = 0usize;
    for text in argv.iter().copied().chain(env.iter().copied()) {
        // An embedded NUL would make the in-memory vector of strings disagree
        // with the pointer table; refuse instead of silently truncating.
        if text.as_bytes().contains(&0) {
            return Err(UserError::ArgumentsTooLong);
        }
        string_bytes = string_bytes
            .checked_add(text.len() + 1)
            .ok_or(UserError::ArgumentsTooLong)?;
    }
    if string_bytes > MAX_ENTRY_BYTES {
        return Err(UserError::ArgumentsTooLong);
    }
    Ok(())
}

/// Build the SysV-style initial stack frame the ABI promises:
///
/// ```text
/// sp -> [argc][argv0..argvN-1][NULL][env0..envK-1][NULL][argv strings][env strings]
/// ```
///
/// The strings grow down from [`USER_TOP`]; the frame size is known from the
/// string lengths, so `sp` is computed up front and each string's pointer is
/// written into the table as the string itself is copied. The table start is
/// aligned down to 16 bytes, which is the entry `sp` alignment RISC-V requires.
///
/// Every write goes through [`paging::translate_in`] to the physical frame of
/// `root` (the loader's pattern): `SUM` stays clear, because this is S-mode
/// writing through the identity map, not the kernel borrowing a user pointer.
fn build_entry_stack_in(root: usize, argv: &[&str], env: &[&str]) -> Result<EntryStack, UserError> {
    build_entry_stack_plan(argv, env)?;
    let argc = argv.len();
    let envc = env.len();
    let mut string_bytes = 0usize;
    for text in argv.iter().copied().chain(env.iter().copied()) {
        string_bytes += text.len() + 1;
    }
    // `[argc]` + `argv` + `NULL` + `envp` + `NULL`.
    let words = 1 + argc + 1 + envc + 1;
    let frame_bytes = words
        .checked_mul(core::mem::size_of::<u64>())
        .ok_or(UserError::ArgumentsTooLong)?;
    let sp = USER_TOP
        .checked_sub(string_bytes)
        .and_then(|strings_start| strings_start.checked_sub(frame_bytes))
        .ok_or(UserError::ArgumentsTooLong)?
        & !0xf;
    if sp < USER_STACK_BASE {
        return Err(UserError::ArgumentsTooLong);
    }

    write_user_u64_in(root, sp, argc as u64)?;
    let mut cursor = USER_TOP;
    let mut argv_slot = sp + 8;
    let env_base = sp + 8 + (argc + 1) * 8;
    let mut env_slot = env_base;
    let mut index = 0usize;
    for text in argv.iter().copied().chain(env.iter().copied()) {
        let start = cursor
            .checked_sub(text.len() + 1)
            .filter(|start| *start >= USER_STACK_BASE)
            .ok_or(UserError::ArgumentsTooLong)?;
        cursor = start;
        write_user_bytes_in(root, cursor, text.as_bytes())?;
        write_user_bytes_in(root, cursor + text.len(), &[0])?;
        if index < argc {
            write_user_u64_in(root, argv_slot, cursor as u64)?;
            argv_slot += 8;
        } else {
            write_user_u64_in(root, env_slot, cursor as u64)?;
            env_slot += 8;
        }
        index += 1;
    }
    write_user_u64_in(root, sp + 8 + argc * 8, 0)?;
    write_user_u64_in(root, env_slot, 0)?;
    Ok(EntryStack {
        sp,
        argc,
        argv: sp + 8,
    })
}

/// Copy `bytes` to the user virtual address `va` in `root`, translating page by
/// page and writing through the physical frame.
///
/// No `SUM`: the destination is a user page this kernel mapped in `root`, and
/// the physical frame is identity-mapped supervisor-RW, so an S-mode store
/// through `pa` is legitimate and cannot take a U-permission fault.
fn write_user_bytes_in(root: usize, va: usize, bytes: &[u8]) -> Result<(), UserError> {
    let mut offset = 0usize;
    while offset < bytes.len() {
        let address = va.checked_add(offset).ok_or(UserError::StackFrame)?;
        let page = address & !(FRAME_SIZE - 1);
        let Some(physical) = paging::translate_in(root, page) else {
            return Err(UserError::StackFrame);
        };
        let in_page = address & (FRAME_SIZE - 1);
        let take = (bytes.len() - offset).min(FRAME_SIZE - in_page);
        // SAFETY: `page` is a mapped user-stack page in `root`; `translate`
        // returned its frame, `in_page + take <= FRAME_SIZE`, and the frame is
        // identity-mapped supervisor-RW. `bytes` is a live kernel slice disjoint
        // from it; the store is how the initial stack becomes visible to U-mode.
        unsafe {
            core::ptr::copy_nonoverlapping(
                bytes.as_ptr().add(offset),
                (physical + in_page) as *mut u8,
                take,
            )
        };
        offset += take;
    }
    Ok(())
}

/// Little-endian 64-bit store into a user stack (see [`write_user_bytes_in`]).
fn write_user_u64_in(root: usize, va: usize, value: u64) -> Result<(), UserError> {
    write_user_bytes_in(root, va, &value.to_le_bytes())
}

// ─── address-space bookkeeping ────────────────────────────────────────────

/// `brk(addr)` for the running program.
///
/// `addr == 0` queries the current (page-aligned) break. Growing maps zeroed
/// PMM frames at [`USER_HEAP_BASE`] on demand; shrinking unmaps them and returns
/// the frames immediately. `addr` below the window is clamped to the base (the
/// program cannot shrink the heap past the start), `addr` past
/// [`USER_HEAP_TOP`] is `Err(())` → `-ENOMEM`, and a partial growth is rolled
/// back so a failed call changes neither the mapping nor the ledger.
pub(crate) fn brk(address: usize) -> Result<usize, ()> {
    let Some(pid) = process::current_pid() else {
        return Err(());
    };
    let Some(slot) = process::current_slot() else {
        return Err(());
    };
    process::update(pid, |run| {
        if address == 0 {
            return Ok(run.heap_end);
        }
        let target = address
            .checked_add(FRAME_SIZE - 1)
            .map(|value| value & !(FRAME_SIZE - 1))
            .ok_or(())?
            .max(USER_HEAP_BASE);
        if target > USER_HEAP_TOP {
            return Err(());
        }

        if target > run.heap_end {
            let start = run.heap_end;
            while run.heap_end < target {
                let Some(frame) = pmm::alloc_frame() else {
                    release_heap_to(run, slot, start);
                    return Err(());
                };
                if paging::map_in(run.root, run.heap_end, frame, Flags::user_rw()).is_err() {
                    pmm::free_frame(frame);
                    release_heap_to(run, slot, start);
                    return Err(());
                }
                // SAFETY: `frame` is a fresh PMM frame (RAM, identity-mapped
                // supervisor RW) that was just mapped into the user heap;
                // writing through the physical address cannot fault and keeps
                // `SUM` off. Zeroing stops a U-mode program from reading whatever
                // used the frame before it.
                unsafe { core::ptr::write_bytes(frame as *mut u8, 0, FRAME_SIZE) };
                process::set_heap_frame(slot, run.heap_count, frame);
                run.heap_count += 1;
                run.heap_end += FRAME_SIZE;
            }
        } else if target < run.heap_end {
            release_heap_to(run, slot, target);
        }
        Ok(run.heap_end)
    })
    .unwrap_or(Err(()))
}

/// Unmap and free heap pages above `target`, updating the recorded break.
fn release_heap_to(run: &mut Process, slot: usize, target: usize) {
    while run.heap_end > target {
        run.heap_end -= FRAME_SIZE;
        run.heap_count -= 1;
        let frame = process::heap_frame(slot, run.heap_count);
        process::set_heap_frame(slot, run.heap_count, 0);
        let _ = paging::unmap_in(run.root, run.heap_end);
        if frame != 0 {
            pmm::free_frame(frame);
        }
    }
}

/// Unmap and free every page the program mapped with `brk`.
fn release_heap(run: &mut Process, slot: usize) {
    release_heap_to(run, slot, USER_HEAP_BASE);
    process::clear_heap(slot);
}

/// True when `[start, start + pages * 4 KiB)` is inside the `mmap` window and
/// neither the region table nor the live page tables claim any page of it.
fn mmap_range_free(run: &Process, start: usize, pages: usize) -> bool {
    let Some(length) = pages.checked_mul(FRAME_SIZE) else {
        return false;
    };
    let Some(end) = start.checked_add(length) else {
        return false;
    };
    if start < USER_MMAP_BASE || end > USER_MMAP_TOP {
        return false;
    }
    if run.mmap_regions[..run.mmap_count]
        .iter()
        .any(|region| start < region.end() && region.start < end)
    {
        return false;
    }
    let mut page = start;
    while page < end {
        if paging::flags_in(run.root, page).is_some() {
            return false;
        }
        page += FRAME_SIZE;
    }
    true
}

/// Record one new region; adjacent extents are coalesced.
fn insert_mmap_region(run: &mut Process, start: usize, pages: usize) {
    run.mmap_regions[run.mmap_count] = MmapRegion { start, pages };
    run.mmap_count += 1;
    coalesce_mmap_regions(run);
}

/// Sort the region list and merge every touching pair.
fn coalesce_mmap_regions(run: &mut Process) {
    run.mmap_regions[..run.mmap_count].sort_unstable_by_key(|region| region.start);
    let mut write = 0usize;
    for read in 0..run.mmap_count {
        let region = run.mmap_regions[read];
        if write > 0 && run.mmap_regions[write - 1].end() == region.start {
            run.mmap_regions[write - 1].pages += region.pages;
        } else {
            run.mmap_regions[write] = region;
            write += 1;
        }
    }
    run.mmap_count = write;
}

/// True when every page of `[start, end)` belongs to a tracked `mmap` region.
fn mmap_range_covered(run: &Process, start: usize, end: usize) -> bool {
    let mut page = start;
    while page < end {
        let Some(region) = run.mmap_regions[..run.mmap_count]
            .iter()
            .find(|region| region.start <= page && page < region.end())
        else {
            return false;
        };
        page = region.end();
    }
    true
}

/// Unmap and free `[start, start + pages * 4 KiB)` in `root`.
fn release_mapping(root: usize, start: usize, pages: usize) {
    for page in 0..pages {
        if let Ok(frame) = paging::unmap_in(root, start + page * FRAME_SIZE) {
            pmm::free_frame(frame);
        }
    }
}

/// `mmap` for the running program: reserve a free window range and map zeroed
/// frames into it.
///
/// `addr` is a hint (0 = none); it is honoured only when page-aligned, inside
/// the window and completely free. Otherwise the first free run from
/// [`USER_MMAP_BASE`] wins. Frames are allocated eagerly and zeroed, because this
/// kernel has no demand-paging fault path.
pub(crate) fn mmap(addr: usize, pages: usize, bits: u64) -> Result<usize, MmapError> {
    if pages == 0 || pages > USER_MMAP_PAGES {
        return Err(MmapError::Invalid);
    }
    let length = pages * FRAME_SIZE;
    let flags = Flags::from_bits(bits);
    let Some(pid) = process::current_pid() else {
        return Err(MmapError::Invalid);
    };
    process::update(pid, |run| {
        if run.mmap_count == MAX_MMAP_REGIONS {
            return Err(MmapError::NoMemory);
        }

        let hinted = (addr != 0 && addr % FRAME_SIZE == 0 && mmap_range_free(run, addr, pages))
            .then_some(addr);
        let base = match hinted {
            Some(base) => base,
            None => {
                let mut cursor = USER_MMAP_BASE;
                loop {
                    let Some(end) = cursor.checked_add(length) else {
                        return Err(MmapError::NoMemory);
                    };
                    if end > USER_MMAP_TOP {
                        return Err(MmapError::NoMemory);
                    }
                    if mmap_range_free(run, cursor, pages) {
                        break cursor;
                    }
                    cursor += FRAME_SIZE;
                }
            }
        };

        let mut mapped = 0usize;
        while mapped < pages {
            let Some(frame) = pmm::alloc_frame() else {
                release_mapping(run.root, base, mapped);
                return Err(MmapError::NoMemory);
            };
            let va = base + mapped * FRAME_SIZE;
            if paging::map_in(run.root, va, frame, flags).is_err() {
                pmm::free_frame(frame);
                release_mapping(run.root, base, mapped);
                return Err(MmapError::NoMemory);
            }
            // SAFETY: `frame` is a fresh PMM frame (RAM, identity-mapped
            // supervisor RW) that was just mapped into the mmap window. Writing
            // through the physical address cannot fault and keeps `SUM` off;
            // zeroing means a user program never sees a previous tenant.
            unsafe { core::ptr::write_bytes(frame as *mut u8, 0, FRAME_SIZE) };
            mapped += 1;
        }
        insert_mmap_region(run, base, pages);
        Ok(base)
    })
    .unwrap_or(Err(MmapError::Invalid))
}

/// `munmap`: unmap whole pages and forget the range.
pub(crate) fn munmap(addr: usize, pages: usize) -> Result<(), MmapError> {
    if pages == 0 || addr % FRAME_SIZE != 0 {
        return Err(MmapError::Invalid);
    }
    let length = pages * FRAME_SIZE;
    let Some(end) = addr.checked_add(length) else {
        return Err(MmapError::Invalid);
    };
    if addr < USER_MMAP_BASE || end > USER_MMAP_TOP {
        return Err(MmapError::Invalid);
    }
    let Some(pid) = process::current_pid() else {
        return Err(MmapError::Invalid);
    };
    process::update(pid, |run| {
        if !mmap_range_covered(run, addr, end) {
            return Err(MmapError::Invalid);
        }

        let mut next = [MmapRegion::empty(); MAX_MMAP_REGIONS];
        let mut next_count = 0usize;
        for index in 0..run.mmap_count {
            let region = run.mmap_regions[index];
            let region_end = region.end();
            if region_end <= addr || region.start >= end {
                if next_count == MAX_MMAP_REGIONS {
                    return Err(MmapError::NoMemory);
                }
                next[next_count] = region;
                next_count += 1;
                continue;
            }
            if region.start < addr {
                if next_count == MAX_MMAP_REGIONS {
                    return Err(MmapError::NoMemory);
                }
                next[next_count] = MmapRegion {
                    start: region.start,
                    pages: (addr - region.start) / FRAME_SIZE,
                };
                next_count += 1;
            }
            if region_end > end {
                if next_count == MAX_MMAP_REGIONS {
                    return Err(MmapError::NoMemory);
                }
                next[next_count] = MmapRegion {
                    start: end,
                    pages: (region_end - end) / FRAME_SIZE,
                };
                next_count += 1;
            }
        }

        release_mapping(run.root, addr, pages);
        run.mmap_regions = next;
        run.mmap_count = next_count;
        coalesce_mmap_regions(run);
        Ok(())
    })
    .unwrap_or(Err(MmapError::Invalid))
}

/// `mprotect`: rewrite the permission bits of every mapped user page in the
/// range, keeping the frames in place.
pub(crate) fn mprotect(addr: usize, pages: usize, bits: u64) -> Result<(), MmapError> {
    if pages == 0 || addr % FRAME_SIZE != 0 {
        return Err(MmapError::Invalid);
    }
    let Some(length) = pages.checked_mul(FRAME_SIZE) else {
        return Err(MmapError::Invalid);
    };
    let Some(end) = addr.checked_add(length) else {
        return Err(MmapError::Invalid);
    };
    if addr < elf::USER_BASE || end > USER_TOP {
        return Err(MmapError::Invalid);
    }
    let Some(pid) = process::current_pid() else {
        return Err(MmapError::Invalid);
    };
    let root = process::peek(pid, |run| run.root).ok_or(MmapError::Invalid)?;

    let mut page = addr;
    while page < end {
        let Some(flags) = paging::flags_in(root, page) else {
            return Err(MmapError::Invalid);
        };
        if flags.bits() & (PTE_U | PTE_R) != PTE_U | PTE_R {
            return Err(MmapError::Invalid);
        }
        page += FRAME_SIZE;
    }

    let flags = Flags::from_bits(bits);
    page = addr;
    while page < end {
        paging::protect(page, flags).map_err(|_| MmapError::Invalid)?;
        page += FRAME_SIZE;
    }
    crate::arch::riscv64::mmu::fence_i();
    Ok(())
}

/// Unmap and free every page the program mapped with `mmap`.
fn release_mmap(run: &mut Process) {
    for index in 0..run.mmap_count {
        let region = run.mmap_regions[index];
        release_mapping(run.root, region.start, region.pages);
        run.mmap_regions[index] = MmapRegion::empty();
    }
    run.mmap_count = 0;
}

/// Unmap the user stack pages of `root` and return the contiguous run to the PMM.
fn release_stack_in(root: usize, base: usize) {
    for page in 0..USER_STACK_PAGES {
        let _ = paging::unmap_in(root, USER_STACK_BASE + page * FRAME_SIZE);
    }
    pmm::free_frames_contiguous(base, USER_STACK_PAGES);
}

/// Release everything a process owns in its address space.
///
/// Called after the last thread is gone and after the process has been taken out
/// of the table. The order matters: image/heap/mmap/stack leaves are unmapped
/// and their frames freed first, then the table structure is returned. Any
/// kernel mapping the root was cloned with is a *reference* to a kernel frame
/// and is deliberately not freed.
fn release_process_memory(proc: &mut Process, slot: Option<usize>) {
    if let Some(slot) = slot {
        if let Some(mut loaded) = process::take_image(slot) {
            loaded.release();
        }
        release_heap(proc, slot);
    }
    release_mmap(proc);
    if proc.stack_base != 0 {
        release_stack_in(proc.root, proc.stack_base);
        proc.stack_base = 0;
    }
    if proc.root != 0 {
        paging::free_user_space(proc.root);
        proc.root = 0;
    }
}

/// Block the calling (shell) task until `pid` has no live thread.
fn wait_for_process(pid: u64) {
    loop {
        if process::is_finished(pid) {
            return;
        }
        if process::slot_of(pid).is_none() {
            return;
        }
        scheduler::yield_now();
    }
}

/// Kill and reap every process still in the table.
///
/// The orphan policy: this kernel has no init, so a child that outlives the
/// foreground (or a grandchild whose parent was reaped by `wait4`) is not
/// adopted — it dies with the `run` that created it. The shell calls this after
/// the foreground process has been reaped, so the PMM ledger and the process
/// table are empty before the prompt returns.
fn cleanup_orphans() {
    let mut pids = [0u64; process::MAX_PROCESSES];
    loop {
        let count = process::ready_pids(&mut pids);
        if count == 0 {
            return;
        }
        for pid in pids[..count].iter().copied() {
            let _ = scheduler::kill_group(pid);
            process::force_finish(
                pid,
                Exit::Fault {
                    reason: "killed: orphaned by the foreground run",
                    cause: 0,
                    sepc: 0,
                },
                None,
            );
        }
        let _ = scheduler::reap();
        for pid in pids[..count].iter().copied() {
            let Some(slot) = process::slot_of(pid) else {
                continue;
            };
            syscall::release_fds(slot);
            signal::clear_process(slot, pid);
            futex::clear_run(pid);
            let _ = process::with_slot_mut(slot, |proc| release_process_memory(proc, Some(slot)));
            process::clear_slot(slot);
        }
    }
}

/// The kernel-thread body of a user task: read the launch request, then never
/// come back from U-mode.
fn user_task() {
    let request = process::current_pid()
        .and_then(|pid| process::peek(pid, |run| (run.entry, run.sp, run.argc, run.argv)));
    match request {
        Some((entry, sp, argc, argv)) => {
            // SAFETY: `entry` was validated by the ELF loader to lie inside an
            // executable `PT_LOAD` page, `sp`/`argc`/`argv` describe the initial
            // stack frame this run built, and this task is the only context that
            // reaches U-mode with these values. `enter_user` never returns.
            unsafe { enter_user(entry, sp, argc, argv) }
        }
        None => {
            crate::kprintln!("[user] task started without a launch request; ending it");
            scheduler::exit()
        }
    }
}

/// Leave S-mode for `entry` at user `sp`, with `a0 = argc` and `a1 = argv`.
///
/// # Safety
///
/// `entry`, `sp`, `argc` and `argv` must describe a loaded user image and the
/// initial stack frame [`build_entry_stack_in`] wrote, and this function must be
/// called from the task that owns the current kernel stack. It never returns.
unsafe fn enter_user(entry: usize, sp: usize, argc: usize, argv: usize) -> ! {
    let status = crate::arch::riscv64::csr::user_entry_status();
    // SAFETY: `sret` deliberately transfers control to U-mode at `entry`. Every
    // register operand is a value validated by the loader/frame builder;
    // interrupts are disabled before the CSRs that describe the return are set,
    // and `sret` re-enables them from `SPIE` only after the switch. The function
    // is `-> !` and `options(noreturn)` keeps the compiler from emitting a
    // return path that could execute with `sp` pointing at the user stack.
    unsafe {
        core::arch::asm!(
            "csrci sstatus, 2",   // SIE = 0: no trap between arming and `sret`
            "csrw sstatus, {status}", // SPP = 0, SPIE = 1, SUM = 0, SIE = 0
            "csrw sscratch, sp",  // kernel trap stack for the first U trap
            "csrw sepc, {entry}",
            "mv sp, {stack}",
            "sret",
            status = in(reg) status,
            entry = in(reg) entry,
            stack = in(reg) sp,
            in("a0") argc,
            in("a1") argv,
            options(noreturn)
        )
    }
}

/// The id of the current thread, as `gettid` reports it.
pub(crate) fn thread_id() -> u64 {
    scheduler::current_id()
}

/// The id of the process the current thread belongs to, if any.
pub(crate) fn current_run_id() -> Option<u64> {
    process::current_pid()
}

/// Count one more live thread of the current process before `clone` spawns it.
pub(crate) fn charge_thread() -> bool {
    let Some(pid) = process::current_pid() else {
        return false;
    };
    process::update(pid, |run| run.threads += 1).is_some()
}

/// Roll back [`charge_thread`] when the spawn it guarded failed.
pub(crate) fn uncharge_thread() {
    if let Some(pid) = process::current_pid() {
        process::update(pid, |run| run.threads = run.threads.saturating_sub(1));
    }
}

/// `getpid`: the process id (`0` on a stale call with no process).
pub(crate) fn process_id() -> u64 {
    process::current_pid().unwrap_or_else(scheduler::current_id)
}

/// `getppid`: the parent process id, `0` for a program the shell started.
pub(crate) fn parent_id() -> u64 {
    process::current_pid()
        .and_then(|pid| process::peek(pid, |run| run.ppid))
        .unwrap_or(0)
}

/// End the calling thread with `code` (syscall 93). Never returns to U-mode.
pub(crate) fn exit_thread(code: u32) -> ! {
    let id = scheduler::current_id();
    let pid = process::current_pid();
    let mut wake = 0u64;
    if let Some(pid) = pid {
        let mut parent = 0u64;
        process::update(pid, |run| {
            if run.main_tid == id {
                run.main_code = Some(code);
            }
            run.threads = run.threads.saturating_sub(1);
            if run.threads == 0 {
                run.exit = Some(Exit::Code(run.main_code.unwrap_or(code)));
                parent = run.ppid;
            }
        });
        if parent != 0 {
            wake = process::peek(parent, |run| run.wait_tid).unwrap_or(0);
        }
    }
    if wake != 0 {
        scheduler::wake_task(wake);
    }
    scheduler::exit()
}

/// End every thread of the calling process with `code` (syscall 94). Never
/// returns.
pub(crate) fn exit_group(code: u32) -> ! {
    let Some(pid) = process::current_pid() else {
        scheduler::exit()
    };
    let (final_code, parent) =
        process::peek(pid, |run| (run.main_code.unwrap_or(code), run.ppid)).unwrap_or((code, 0));
    let _ = scheduler::kill_group(pid);
    process::update(pid, |run| {
        run.threads = 0;
        run.exit = Some(Exit::Code(final_code));
    });
    notify_parent(parent);
    scheduler::exit()
}

/// Wake the parent thread blocked in `wait4`, if any.
fn notify_parent(parent: u64) {
    if parent == 0 {
        return;
    }
    let wait_tid = process::peek(parent, |run| run.wait_tid).unwrap_or(0);
    if wait_tid != 0 {
        scheduler::wake_task(wait_tid);
    }
}

/// A process's default action for a signal: print one line and end every thread.
pub(crate) fn on_signal_death(signal: usize, sepc: usize, reason: &'static str) -> ! {
    let mut name = RunName::new("user");
    let mut pid = 0u64;
    let mut parent = 0u64;
    if let Some(current) = process::current_pid() {
        pid = current;
        process::update(current, |run| {
            name = run.name;
            parent = run.ppid;
            run.exit = Some(Exit::Fault {
                reason,
                cause: 0,
                sepc,
            });
            run.signal = Some(signal);
        });
    }
    crate::kprintln!(
        "[user] {} killed: {} (signal={}, sepc={:#x})",
        name.as_str(),
        reason,
        signal,
        sepc
    );
    if pid != 0 {
        let _ = scheduler::kill_group(pid);
        process::update(pid, |run| run.threads = 0);
    }
    notify_parent(parent);
    scheduler::exit()
}

/// A U-mode exception: print one diagnostic line, record it as the process's
/// result and end *every* thread of that process. Never returns to U-mode.
pub fn on_fault(scause: usize, sepc: usize, stval: usize) -> ! {
    let reason = crate::arch::riscv64::traps::cause_name(scause);
    let mut name = RunName::new("user");
    let mut pid = 0u64;
    let mut parent = 0u64;
    if let Some(current) = process::current_pid() {
        pid = current;
        process::update(current, |run| {
            name = run.name;
            parent = run.ppid;
            run.exit = Some(Exit::Fault {
                reason,
                cause: scause,
                sepc,
            });
            run.signal = fault_signal(scause);
        });
    }
    crate::kprintln!(
        "[user] {} killed: {} (cause={}, stval={:#x}, sepc={:#x})",
        name.as_str(),
        reason,
        scause,
        stval,
        sepc
    );
    if pid != 0 {
        let _ = scheduler::kill_group(pid);
        process::update(pid, |run| run.threads = 0);
    }
    notify_parent(parent);
    scheduler::exit()
}

/// The POSIX signal number a synchronous fault maps to, if any.
fn fault_signal(cause: usize) -> Option<usize> {
    match cause {
        crate::arch::riscv64::traps::CAUSE_ILLEGAL_INSTRUCTION => Some(4), // SIGILL
        crate::arch::riscv64::traps::CAUSE_BREAKPOINT => Some(5),          // SIGTRAP
        crate::arch::riscv64::traps::CAUSE_INSTRUCTION_PAGE_FAULT
        | crate::arch::riscv64::traps::CAUSE_LOAD_PAGE_FAULT
        | crate::arch::riscv64::traps::CAUSE_STORE_PAGE_FAULT => Some(11), // SIGSEGV
        _ => None,
    }
}

// ─── process-family syscalls ───────────────────────────────────────────────

/// `fork`: duplicate the calling process.
///
/// The child gets its own Sv39 root and ASID, deep copies of every mapped user
/// page (image, heap, `mmap`, stack), a copy of the descriptor table (pipes keep
/// the shared ring with one more reference; a socket becomes an `Unsupported`
/// descriptor because the kernel owns exactly one TCP connection) and a copy of
/// the signal handlers plus the caller's blocked mask. Pending signals are not
/// inherited.
///
/// Both processes return from the same `ecall`: the parent with the child pid,
/// the child with `a0 = 0` and every other register — `sepc`, `sp`, `gp`, `tp`
/// and the callee-saved set — identical. The child's `sepc` already points past
/// the `ecall` because the parent's dispatcher is about to advance its own.
pub(crate) fn fork(frame: &TrapFrame) -> isize {
    const EAGAIN: isize = -11;
    const EINVAL: isize = -22;

    let Some(parent_pid) = process::current_pid() else {
        return EINVAL;
    };
    let Some(parent_slot) = process::current_slot() else {
        return EINVAL;
    };
    let Some(snapshot) = process::peek(parent_pid, |run| ForkSnapshot::capture(run, parent_slot))
    else {
        return EINVAL;
    };
    let child_pid = NEXT_PID.fetch_add(1, Ordering::Relaxed);

    // Reserve the slot before copying anything: the child object lives in the
    // static table, so a 1 MiB heap page list and a 1 MiB image page list never
    // travel through this syscall's 16 KiB kernel stack.
    let Some(child_slot) = process::reserve(child_pid, parent_pid) else {
        return EAGAIN;
    };
    if let Err(error) = build_fork_into(child_slot, &snapshot) {
        fork_cleanup(child_slot, child_pid);
        return error;
    }
    if !syscall::fork_fds(parent_slot, child_slot) {
        fork_cleanup(child_slot, child_pid);
        return EAGAIN;
    }
    let mut child_frame = *frame;
    child_frame.a0 = 0;
    child_frame.sepc = frame.sepc.wrapping_add(4);
    child_frame.sstatus = crate::arch::riscv64::csr::user_entry_status();
    let satp = process::peek(child_pid, |proc| proc.satp).unwrap_or(0);
    let Some(child_tid) = scheduler::spawn_user_frame(&child_frame, child_pid, satp) else {
        fork_cleanup(child_slot, child_pid);
        return EAGAIN;
    };
    if !signal::fork_process(parent_pid, child_pid, parent_slot, child_slot, child_tid) {
        let _ = scheduler::kill_group(child_pid);
        fork_cleanup(child_slot, child_pid);
        let _ = scheduler::reap();
        return EAGAIN;
    }
    process::update(child_pid, |proc| {
        proc.main_tid = child_tid;
        proc.ready = true;
    });
    child_pid as isize
}

/// Release every resource a failed fork may have installed, in place.
fn fork_cleanup(slot: usize, pid: u64) {
    syscall::release_fds(slot);
    signal::clear_process(slot, pid);
    futex::clear_run(pid);
    let _ = process::with_slot_mut(slot, |proc| release_process_memory(proc, Some(slot)));
    process::clear_slot(slot);
}

/// Everything `fork` needs from the parent, copied while the table is locked so
/// no page copy runs under it.
struct ForkSnapshot {
    /// Parent slot, which owns the image and heap page rows.
    parent_slot: usize,
    root: usize,
    entry: usize,
    sp: usize,
    argc: usize,
    argv: usize,
    name: RunName,
    image_count: usize,
    heap_end: usize,
    heap_count: usize,
    mmap_regions: [MmapRegion; MAX_MMAP_REGIONS],
    mmap_count: usize,
}

impl ForkSnapshot {
    fn capture(run: &Process, slot: usize) -> Self {
        let image_count = process::with_image(slot, |loaded| loaded.pages().len()).unwrap_or(0);
        ForkSnapshot {
            parent_slot: slot,
            root: run.root,
            entry: run.entry,
            sp: run.sp,
            argc: run.argc,
            argv: run.argv,
            name: run.name,
            image_count,
            heap_end: run.heap_end,
            heap_count: run.heap_count,
            mmap_regions: run.mmap_regions,
            mmap_count: run.mmap_count,
        }
    }
}

/// Fill a reserved child slot and copy every mapped user page into its root.
///
/// The child object is mutated through the table, never returned by value; on
/// error the caller runs [`fork_cleanup`].
fn build_fork_into(child_slot: usize, snapshot: &ForkSnapshot) -> Result<(), isize> {
    const ENOMEM: isize = -12;
    let space = paging::create_user_space().map_err(|_| ENOMEM)?;
    let mut pages = [0usize; elf::MAX_IMAGE_PAGES];
    for (index, page) in pages[..snapshot.image_count].iter_mut().enumerate() {
        let Some(va) = process::image_page(snapshot.parent_slot, index) else {
            paging::free_user_space(space.root);
            return Err(ENOMEM);
        };
        *page = va;
    }
    let Some(loaded) =
        elf::Loaded::from_pages(space.root, snapshot.entry, &pages[..snapshot.image_count])
    else {
        paging::free_user_space(space.root);
        return Err(ENOMEM);
    };
    process::with_slot_mut(child_slot, |child| {
        child.root = space.root;
        child.satp = space.satp;
        child.asid = space.asid;
        child.entry = snapshot.entry;
        child.sp = snapshot.sp;
        child.argc = snapshot.argc;
        child.argv = snapshot.argv;
        child.name = snapshot.name;
        child.heap_end = snapshot.heap_end;
        child.heap_count = snapshot.heap_count;
        child.mmap_regions = snapshot.mmap_regions;
        child.mmap_count = snapshot.mmap_count;
        child.stack_base = 0;
        child.threads = 1;
    });
    process::set_image(child_slot, loaded);

    for index in 0..snapshot.image_count {
        let Some(va) = process::image_page(snapshot.parent_slot, index) else {
            return Err(ENOMEM);
        };
        if copy_user_page(snapshot.root, space.root, va).is_err() {
            return Err(ENOMEM);
        }
    }

    let Some(stack_base) = pmm::alloc_frames_contiguous(USER_STACK_PAGES) else {
        return Err(ENOMEM);
    };
    if let Some(child) = process::with_slot_mut(child_slot, |child| child.stack_base = stack_base) {
        let _ = child;
    } else {
        pmm::free_frames_contiguous(stack_base, USER_STACK_PAGES);
        return Err(ENOMEM);
    }
    for page in 0..USER_STACK_PAGES {
        let va = USER_STACK_BASE + page * FRAME_SIZE;
        let frame = stack_base + page * FRAME_SIZE;
        if copy_page_into(snapshot.root, space.root, va, frame).is_err() {
            return Err(ENOMEM);
        }
    }

    for index in 0..snapshot.heap_count {
        let va = USER_HEAP_BASE + index * FRAME_SIZE;
        let Some(frame) = pmm::alloc_frame() else {
            return Err(ENOMEM);
        };
        if copy_page_into(snapshot.root, space.root, va, frame).is_err() {
            pmm::free_frame(frame);
            return Err(ENOMEM);
        }
        process::set_heap_frame(child_slot, index, frame);
    }

    for index in 0..snapshot.mmap_count {
        let region = snapshot.mmap_regions[index];
        let mut page = region.start;
        while page < region.end() {
            if copy_user_page(snapshot.root, space.root, page).is_err() {
                return Err(ENOMEM);
            }
            page += FRAME_SIZE;
        }
    }
    Ok(())
}

/// Copy one user page from `src_root` to a fresh frame in `dst_root`.
fn copy_user_page(src_root: usize, dst_root: usize, va: usize) -> Result<(), ()> {
    let Some(frame) = pmm::alloc_frame() else {
        return Err(());
    };
    if copy_page_into(src_root, dst_root, va, frame).is_err() {
        pmm::free_frame(frame);
        return Err(());
    }
    Ok(())
}

/// Copy the contents of the parent page at `va` into `frame` and map `frame` at
/// the same virtual address in `dst_root`, preserving the parent's permissions.
fn copy_page_into(src_root: usize, dst_root: usize, va: usize, frame: usize) -> Result<(), ()> {
    let Some(source) = paging::translate_in(src_root, va & !(FRAME_SIZE - 1)) else {
        return Err(());
    };
    let Some(flags) = paging::flags_in(src_root, va & !(FRAME_SIZE - 1)) else {
        return Err(());
    };
    if flags.bits() & PTE_U == 0 {
        return Err(());
    }
    // SAFETY: `source` is the physical frame backing a mapped user page of the
    // parent (identity-mapped supervisor RW) and `frame` is a fresh PMM frame
    // owned by this copy. Both are FRAME_SIZE bytes and disjoint.
    unsafe { core::ptr::copy_nonoverlapping(source as *const u8, frame as *mut u8, FRAME_SIZE) };
    if paging::map_in(dst_root, va & !(FRAME_SIZE - 1), frame, flags).is_err() {
        return Err(());
    }
    Ok(())
}

/// `execve(path, argv, envp)`.
///
/// Loads a static ELF from the VFS into a *fresh* address space, builds the
/// SysV entry stack from the caller's `argv`/`envp` arrays, then atomically
/// replaces the process image: the old user pages, heap, `mmap` extents and
/// stack are freed, the pid/parent/descriptor table survive, caught signal
/// handlers go back to `SIG_DFL`, pending signals are dropped and the blocked
/// mask is kept. Sibling threads are retired (a process has one image).
///
/// The success path rewrites `frame` and returns `Ok(())`; `dispatch` then must
/// **not** bump `sepc` or write `a0`. Every error returns an errno and changes
/// nothing.
pub(crate) fn execve(
    frame: &mut TrapFrame,
    path_ptr: usize,
    argv_ptr: usize,
    envp_ptr: usize,
) -> Result<(), isize> {
    const E2BIG: isize = -7;
    const ENOENT: isize = -2;
    const ENOEXEC: isize = -8;
    const EFAULT: isize = -14;
    const EISDIR: isize = -21;
    const EINVAL: isize = -22;
    const ENOMEM: isize = -12;
    const EIO: isize = -5;

    // Resolve the caller's slot before anything is allocated: every failure
    // below then has a place to release what it built, and a process that has
    // already been taken out of the table cannot leak a new address space.
    let Some(slot) = process::current_slot() else {
        return Err(EINVAL);
    };
    let Some(pid) = process::current_pid() else {
        return Err(EINVAL);
    };

    let mut path_buf = [0u8; 256];
    let path = read_user_cstring(path_ptr, &mut path_buf)?;
    if !path.starts_with('/') {
        return Err(ENOENT);
    }
    let argv = read_string_array(argv_ptr, MAX_ENTRY_BYTES)?;
    let env = read_string_array(envp_ptr, MAX_ENTRY_BYTES)?;
    if argv.len() + env.len() > MAX_ENTRY_STRINGS {
        return Err(E2BIG);
    }
    let total: usize = argv
        .iter()
        .chain(env.iter())
        .map(|text| text.len() + 1)
        .sum();
    if total > MAX_ENTRY_BYTES {
        return Err(E2BIG);
    }

    let node = vfs::walk(path, true).map_err(|error| match error {
        VfsError::NotFound => ENOENT,
        _ => EIO,
    })?;
    if node.is_directory() {
        return Err(EISDIR);
    }
    let size = node.size();
    if size == 0 {
        return Err(ENOEXEC);
    }
    if size > elf::MAX_IMAGE_BYTES as u64 {
        return Err(E2BIG);
    }
    let source = FileSource { node, size };

    let space = paging::create_user_space().map_err(|_| ENOMEM)?;
    let mut loaded = match elf::load_in(space.root, &source, 0) {
        Ok(loaded) => loaded,
        Err(ElfError::Source(_)) => {
            paging::free_user_space(space.root);
            return Err(EIO);
        }
        Err(ElfError::OutOfFrames) => {
            paging::free_user_space(space.root);
            return Err(ENOMEM);
        }
        Err(_) => {
            paging::free_user_space(space.root);
            return Err(ENOEXEC);
        }
    };

    let Some(stack_base) = pmm::alloc_frames_contiguous(USER_STACK_PAGES) else {
        loaded.release();
        paging::free_user_space(space.root);
        return Err(ENOMEM);
    };
    for page in 0..USER_STACK_PAGES {
        let va = USER_STACK_BASE + page * FRAME_SIZE;
        let pa = stack_base + page * FRAME_SIZE;
        if paging::map_in(space.root, va, pa, Flags::user_rw()).is_err() {
            release_stack_in(space.root, stack_base);
            loaded.release();
            paging::free_user_space(space.root);
            return Err(ENOMEM);
        }
        // SAFETY: as in `run_source`: a freshly mapped user-stack frame.
        unsafe { core::ptr::write_bytes(pa as *mut u8, 0, FRAME_SIZE) };
    }

    let argv_refs: Vec<&str> = match argv
        .iter()
        .map(|text| core::str::from_utf8(text).map_err(|_| EINVAL))
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(refs) => refs,
        Err(error) => {
            release_stack_in(space.root, stack_base);
            loaded.release();
            paging::free_user_space(space.root);
            return Err(error);
        }
    };
    let env_refs: Vec<&str> = match env
        .iter()
        .map(|text| core::str::from_utf8(text).map_err(|_| EINVAL))
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(refs) => refs,
        Err(error) => {
            release_stack_in(space.root, stack_base);
            loaded.release();
            paging::free_user_space(space.root);
            return Err(error);
        }
    };
    let built = match build_entry_stack_in(space.root, &argv_refs, &env_refs) {
        Ok(built) => built,
        Err(UserError::ArgumentsTooLong) => {
            release_stack_in(space.root, stack_base);
            loaded.release();
            paging::free_user_space(space.root);
            return Err(E2BIG);
        }
        Err(_) => {
            release_stack_in(space.root, stack_base);
            loaded.release();
            paging::free_user_space(space.root);
            return Err(EFAULT);
        }
    };

    let new_entry = loaded.entry;

    // Everything fallible is done. Switch the hart to the new root, retire the
    // siblings that still point at the old image, then replace the process's
    // address-space fields. The old pages are freed while the new root is
    // active; the freed root is never the one executing this code again.
    crate::arch::riscv64::mmu::activate_satp(space.satp);
    scheduler::set_current_satp(space.satp);
    let _ = scheduler::kill_group(pid);
    let replaced = process::with_slot_mut(slot, |proc| {
        release_process_memory(proc, Some(slot));
        proc.root = space.root;
        proc.satp = space.satp;
        proc.asid = space.asid;
        proc.stack_base = stack_base;
        proc.heap_end = USER_HEAP_BASE;
        proc.heap_count = 0;
        proc.mmap_count = 0;
        proc.mmap_regions = [MmapRegion::empty(); MAX_MMAP_REGIONS];
        proc.entry = new_entry;
        proc.sp = built.sp;
        proc.argc = built.argc;
        proc.argv = built.argv;
        proc.name = RunName::new(argv_refs.first().copied().unwrap_or(path));
        proc.threads = 1;
        proc.main_tid = scheduler::current_id();
        proc.main_code = None;
        proc.exit = None;
        proc.signal = None;
        // Any sibling killed above may have been the one blocked in `wait4`.
        // Leaving its tid here would make a later child-exit wake an unrelated
        // task, so the new image starts with nobody waiting.
        proc.wait_tid = 0;
    });
    if replaced.is_none() {
        crate::kprintln!("[user] execve lost its process slot; ending the task");
        scheduler::exit()
    }
    process::set_image(slot, loaded);
    signal::exec_process(slot, pid);
    futex::clear_run(pid);

    *frame = TrapFrame::zeroed();
    frame.sepc = new_entry;
    frame.sp = built.sp;
    frame.a0 = built.argc;
    frame.a1 = built.argv;
    frame.sstatus = crate::arch::riscv64::csr::user_entry_status();
    Ok(())
}

/// `wait4(pid, status, options, rusage)`.
pub(crate) fn wait4(pid_spec: isize, status_ptr: usize, options: usize, _rusage: usize) -> isize {
    const WNOHANG: usize = 1;
    const ECHILD: isize = -10;
    const EINTR: isize = -4;
    const EFAULT: isize = -14;
    const EINVAL: isize = -22;

    if options & !WNOHANG != 0 {
        return EINVAL;
    }
    if pid_spec < -1 {
        return EINVAL;
    }
    if status_ptr != 0 && !syscall::user_range_valid(status_ptr, status_ptr + 4, true) {
        return EFAULT;
    }
    let Some(parent) = process::current_pid() else {
        return ECHILD;
    };
    let want = if pid_spec > 0 {
        Some(pid_spec as u64)
    } else {
        None
    };

    loop {
        let mut children = 0usize;
        let mut zombie: Option<(usize, u64)> = None;
        process::with_table(|table| {
            for (slot, entry) in table.iter().enumerate() {
                let Some(proc) = entry.as_ref() else {
                    continue;
                };
                if !proc.ready || proc.ppid != parent {
                    continue;
                }
                if let Some(want) = want {
                    if proc.pid != want {
                        continue;
                    }
                }
                children += 1;
                if proc.threads == 0 {
                    zombie = Some((slot, proc.pid));
                    break;
                }
            }
        });
        if let Some((slot, child_pid)) = zombie {
            let child_signal = process::peek(child_pid, |proc| wait_status(proc)).unwrap_or(0);
            process::update(child_pid, |proc| {
                proc.ready = true;
            });
            syscall::release_fds(slot);
            signal::clear_process(slot, child_pid);
            futex::clear_run(child_pid);
            if process::with_slot_mut(slot, |proc| release_process_memory(proc, Some(slot)))
                .is_none()
            {
                continue;
            }
            process::clear_slot(slot);
            let _ = scheduler::reap();
            if status_ptr != 0 {
                if let Err(error) = syscall::copy_to_user(status_ptr, &child_signal.to_le_bytes()) {
                    return error;
                }
            }
            return child_pid as isize;
        }
        if children == 0 {
            return ECHILD;
        }
        if options & WNOHANG != 0 {
            return 0;
        }
        if crate::user::signal::interrupted() {
            return EINTR;
        }
        let tid = scheduler::current_id();
        process::update(parent, |proc| proc.wait_tid = tid);
        scheduler::block_current(None);
        process::update(parent, |proc| proc.wait_tid = 0);
        if crate::user::signal::interrupted() {
            return EINTR;
        }
    }
}

/// Encode a process's exit as a POSIX `wait` status.
///
/// A normal exit is `code << 8` (the low byte only, as Linux does); a signal or
/// fault death is the signal number in the low seven bits with no core-dump bit
/// and no exit code.
fn wait_status(child: &Process) -> u32 {
    if let Some(signal) = child.signal {
        return (signal as u32) & 0x7f;
    }
    match child.exit {
        Some(Exit::Code(code)) => (code & 0xff) << 8,
        Some(Exit::Fault { cause, .. }) => fault_signal(cause).unwrap_or(0) as u32,
        None => 0,
    }
}

/// Read a NUL-terminated user string into `out`, returning it without the NUL.
fn read_user_cstring<'a>(pointer: usize, out: &'a mut [u8]) -> Result<&'a str, isize> {
    const EFAULT: isize = -14;
    const EINVAL: isize = -22;
    if pointer == 0 {
        return Err(EFAULT);
    }
    let mut length = 0usize;
    loop {
        if length == out.len() {
            return Err(EINVAL);
        }
        let mut byte = [0u8; 1];
        let address = pointer.checked_add(length).ok_or(EFAULT)?;
        syscall::copy_from_user(&mut byte, address)?;
        if byte[0] == 0 {
            break;
        }
        out[length] = byte[0];
        length += 1;
    }
    core::str::from_utf8(&out[..length]).map_err(|_| EINVAL)
}

/// Read a user `char **` into owned byte strings.
fn read_string_array(pointer: usize, byte_budget: usize) -> Result<Vec<Vec<u8>>, isize> {
    const E2BIG: isize = -7;
    const EFAULT: isize = -14;
    if pointer == 0 {
        return Ok(Vec::new());
    }
    let mut strings = Vec::new();
    let mut bytes = 0usize;
    for index in 0..MAX_ENTRY_STRINGS {
        let word_address = pointer.checked_add(index * 8).ok_or(EFAULT)?;
        let mut raw = [0u8; 8];
        syscall::copy_from_user(&mut raw, word_address)?;
        let target = u64::from_le_bytes(raw) as usize;
        if target == 0 {
            return Ok(strings);
        }
        let mut text = Vec::new();
        let mut cursor = target;
        loop {
            let mut byte = [0u8; 1];
            syscall::copy_from_user(&mut byte, cursor)?;
            if byte[0] == 0 {
                break;
            }
            text.push(byte[0]);
            bytes += 1;
            if bytes > byte_budget || bytes > MAX_ENTRY_BYTES {
                return Err(E2BIG);
            }
            cursor = cursor.checked_add(1).ok_or(EFAULT)?;
        }
        strings.push(text);
    }
    Err(E2BIG)
}
