//! User mode (M6b): load a static RV64 ELF, run it in U-mode as a scheduler
//! task, serve its `ecall`s and survive its faults.
//!
//! # The shape of a run
//!
//! [`run_image`] (embedded fixtures) and [`run_file`] (`run <path>`) are the
//! whole user-visible operation, called from the shell:
//!
//! 1. [`elf::load`] validates the image and maps its `PT_LOAD` pages with `U=1`
//!    in the kernel's one Sv39 tree (see that module for the layout). The image
//!    is read through [`elf::ImageSource`], so a file is streamed: the loader
//!    asks for the header, the program-header table and one page of each
//!    segment at a time and never allocates a whole-image buffer.
//! 2. The 64 KiB user stack (`USER_STACK_BASE..USER_TOP`) is mapped from a
//!    contiguous PMM run.
//! 3. A normal scheduler task is spawned; its kernel-thread body calls
//!    [`enter_user`], which builds the return-to-U-mode state and executes
//!    `sret` (`sstatus.SPP = 0`, `SPIE = 1`, `sepc = entry`, `sp = stack_top`).
//! 4. The shell task waits in [`wait_for_exit`], yielding the CPU until the user
//!    task is `Exited`. While it waits, timer preemption and `sched_yield` work
//!    exactly as for any other task — U-mode is just the task's register state.
//! 5. The runner reaps the task's kernel stack and releases the image and user
//!    stack frames. The PMM ledger is back where it started; the `selftest`
//!    user suite measures that, not the function's promise.
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
//! # What this isolation is, and is not
//!
//! One address space, one ASID: user pages carry `U=1`, kernel pages do not, so
//! U-mode cannot read, write or execute the kernel. There is no separate `satp`,
//! no per-process translation namespace and no protection between two user
//! programs — processes run one at a time from [`run_image`], which refuses a
//! second concurrent run. That is the honest description of the milestone; a
//! per-process page table is a later change, not a flag.
//!
//! # Faults
//!
//! An exception whose saved `sstatus.SPP` is 0 ends the *task*, never the
//! machine ([`on_fault`]). It prints one diagnostic line and calls
//! `scheduler::exit`; the runner then does the cleanup below. S-mode exceptions
//! keep the older print-and-park contract in `arch::riscv64::traps`.

pub mod elf;
pub mod selftest;
pub mod syscall;

use core::fmt;
use core::sync::atomic::{AtomicBool, Ordering};

use alloc::sync::Arc;

use crate::arch::riscv64::mmu::Flags;
use crate::memory::paging;
use crate::memory::pmm;
use crate::memory::FRAME_SIZE;
use crate::sync::Spinlock;
use crate::task::scheduler::{self, State};
use crate::task::{self, TaskInfo, MAX_TASKS};
use crate::vfs::{self, VfsError, VfsNode};

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
/// two resource failures the launch path has (task slot, stack frames).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserError {
    /// A program is already running (the shell is a single runner by design).
    Busy,
    /// The ELF was refused; `elf::load` has already rolled back its pages.
    Elf(ElfError),
    /// The 64 KiB user stack had no contiguous run in the PMM.
    StackFrames,
    /// A user-stack page could not be mapped; the stack run was rolled back.
    StackMap(paging::MapError),
    /// The scheduler had no task slot or no frames for the kernel stack.
    TaskSlot,
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
            UserError::Busy => f.write_str("a user program is already running"),
            UserError::Elf(error) => write!(f, "{error}"),
            UserError::StackFrames => f.write_str("no contiguous frames for the user stack"),
            UserError::StackMap(error) => write!(f, "mapping the user stack failed: {error}"),
            UserError::TaskSlot => {
                f.write_str("the scheduler has no free slot or kernel stack for the task")
            }
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

/// A fixed-capacity program name: stored in [`RunState`] because the fault
/// handler runs in trap context and cannot borrow the shell's `String`.
#[derive(Clone, Copy)]
struct RunName {
    bytes: [u8; NAME_CAPACITY],
    len: u8,
}

impl RunName {
    const fn empty() -> Self {
        RunName {
            bytes: [0; NAME_CAPACITY],
            len: 0,
        }
    }

    fn new(name: &str) -> Self {
        let mut out = RunName::empty();
        let take = name.len().min(NAME_CAPACITY);
        out.bytes[..take].copy_from_slice(&name.as_bytes()[..take]);
        out.len = take as u8;
        out
    }

    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.len as usize]).unwrap_or("?")
    }
}

/// The one user program currently running, if any.
///
/// `run_image` claims this before mapping anything and releases it after the
/// task's frames are back in the PMM, so `Busy` is a real invariant rather than
/// advisory. The trap and syscall paths only ever *read and update* the entry:
/// the runner owns the mappings itself and does the cleanup after the task is
/// gone, which keeps page-table teardown out of trap context.
struct RunState {
    name: RunName,
    entry: usize,
    /// Initial user `sp`: 16-byte aligned, points at `argc`.
    sp: usize,
    /// `argc` and the user address of `argv[0]`, passed in `a0`/`a1`.
    argc: usize,
    argv: usize,
    exit: Option<Exit>,
    /// Page-aligned current `brk` (starts at [`USER_HEAP_BASE`]).
    heap_end: usize,
    /// Physical frames backing `USER_HEAP_BASE..heap_end`, in mapping order.
    /// Kept here so the runner can unmap/free them after the task is gone; the
    /// syscall layer is the only writer.
    heap_frames: [usize; USER_HEAP_PAGES],
    heap_count: usize,
}

static RUN: Spinlock<Option<RunState>> = Spinlock::new(None);

// ─── console capture for the ABI selftest ─────────────────────────────────
//
// The argv fixture has to prove one *exact* line, which is a property of the
// bytes `write(2)` handed to the console, not of an exit code. Capturing lives
// here rather than in `console` because only U-mode writes go through it: the
// kernel's own diagnostics stay out of the buffer by construction, and a normal
// boot pays one relaxed atomic load per user write.

/// Bytes the capture keeps; the argv line is well under 100.
const CAPTURE_CAPACITY: usize = 256;

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
///
/// This is the compatibility entry point: `argv[0]` is `path` and there are no
/// further arguments or environment strings. The shell uses
/// [`run_file_with_argv0`] so a relative `run demo.elf` keeps `argv[0]` exactly
/// as typed (the ABI contract), while [`run_file_with_args`] is the
/// path-plus-arguments form.
pub fn run_file(path: &str) -> Result<Exit, UserError> {
    run_file_with_argv0(path, path, &[], &[])
}

/// [`run_file`] with arguments and an environment: `argv` is `argv[1..]` and
/// `argv[0]` is `path` itself.
#[allow(dead_code)] // the ABI selftest uses the path+args form
pub fn run_file_with_args(path: &str, args: &[&str], env: &[&str]) -> Result<Exit, UserError> {
    run_file_with_argv0(path, path, args, env)
}

/// [`run_file`] with a separate `argv[0]`.
///
/// `open_path` is what the VFS resolves (the shell's cwd-joined path); `argv0`
/// is what the program sees in `argv[0]` (the path as the user typed it). The
/// split exists because the ABI says `argv[0]` is the typed path, and resolving
/// `demo.elf` against the cwd must not leak `/mnt/demo.elf` into the program's
/// view of its own name.
pub fn run_file_with_argv0(
    open_path: &str,
    argv0: &str,
    args: &[&str],
    env: &[&str],
) -> Result<Exit, UserError> {
    let mut argv = [""; MAX_ENTRY_STRINGS];
    let count = 1 + args.len();
    if count > MAX_ENTRY_STRINGS {
        return Err(UserError::ArgumentsTooLong);
    }
    argv[0] = argv0;
    argv[1..count].copy_from_slice(args);

    let node = vfs::walk(open_path, true).map_err(UserError::Vfs)?;
    if node.is_directory() {
        return Err(UserError::IsADirectory);
    }
    let size = node.size();
    if size == 0 || size > elf::MAX_IMAGE_BYTES as u64 {
        return Err(UserError::ImageSize(size));
    }
    let source = FileSource { node, size };
    run_source(&source, &argv[..count], env)
}

/// Load `bytes` and run it in U-mode until `exit` or a fault, then return how it
/// ended. The image and stack mappings are gone before this returns.
///
/// `name` is the label in the fault diagnostic and the shell's result line; it is
/// copied into a fixed buffer because the trap handler needs it while the caller
/// is suspended. The fixture gets `argc = 1`, `argv = [name]` and the minimal
/// environment `PWD=/` (fixtures have no cwd of their own).
pub fn run_image(bytes: &[u8], name: &str) -> Result<Exit, UserError> {
    run_source(bytes, &[name], &["PWD=/"])
}

/// [`run_image`] with explicit arguments (used by the ABI selftest to prove the
/// multi-argument stack without a filesystem). `argv[0]` is `args[0]`; the
/// environment is the caller's, because a fixture that checks `envp` needs to
/// know what to expect.
pub fn run_image_with_args(bytes: &[u8], args: &[&str], env: &[&str]) -> Result<Exit, UserError> {
    run_source(bytes, args, env)
}

/// The shared run path: claim `RUN`, load from `source`, map the stack, build
/// the SysV-style initial frame, spawn, wait, and unwind every allocation on
/// both success and failure.
///
/// `RUN` is claimed before the first mapping and released only after the image
/// pages, the initial stack and (when the program used `brk`) the user heap are
/// gone, which is what makes [`UserError::Busy`] a real one-run-at-a-time
/// guarantee rather than advice.
fn run_source<S: ImageSource + ?Sized>(
    source: &S,
    argv: &[&str],
    env: &[&str],
) -> Result<Exit, UserError> {
    if argv.is_empty() || argv.len() + env.len() > MAX_ENTRY_STRINGS {
        return Err(UserError::ArgumentsTooLong);
    }
    let mut string_bytes = 0usize;
    for text in argv.iter().chain(env.iter()) {
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

    {
        let mut slot = RUN.lock();
        if slot.is_some() {
            return Err(UserError::Busy);
        }
        *slot = Some(RunState {
            name: RunName::new(argv[0]),
            entry: 0,
            sp: USER_TOP,
            argc: argv.len(),
            argv: USER_TOP,
            exit: None,
            heap_end: USER_HEAP_BASE,
            heap_frames: [0; USER_HEAP_PAGES],
            heap_count: 0,
        });
    }

    let mut loaded = match elf::load(source, 0) {
        Ok(loaded) => loaded,
        Err(ElfError::Source(error)) => {
            *RUN.lock() = None;
            return Err(UserError::Source(error));
        }
        Err(error) => {
            *RUN.lock() = None;
            return Err(UserError::Elf(error));
        }
    };

    // The stack is one contiguous PMM run mapped page by page at the fixed user
    // range. A failure anywhere below unwinds the whole run: every page mapped
    // so far is unmapped and the frames go back as one allocation.
    let Some(stack_base) = pmm::alloc_frames_contiguous(USER_STACK_PAGES) else {
        loaded.release();
        *RUN.lock() = None;
        return Err(UserError::StackFrames);
    };
    for page in 0..USER_STACK_PAGES {
        let va = USER_STACK_BASE + page * FRAME_SIZE;
        let pa = stack_base + page * FRAME_SIZE;
        if let Err(error) = paging::map(va, pa, Flags::user_rw()) {
            for mapped in 0..page {
                let _ = paging::unmap(USER_STACK_BASE + mapped * FRAME_SIZE);
            }
            pmm::free_frames_contiguous(stack_base, USER_STACK_PAGES);
            loaded.release();
            *RUN.lock() = None;
            return Err(UserError::StackMap(error));
        }
        // SAFETY: `pa` is a PMM frame (RAM, therefore identity-mapped supervisor
        // RW by `paging::init`) that this call just mapped as the user stack;
        // writing through the physical address, the same way the ELF loader
        // copies segment bytes, cannot fault and keeps `SUM` off. Zeroing is
        // deliberate: the frames may have been a kernel task stack before, and
        // a U-mode program must not be able to read what was there.
        unsafe { core::ptr::write_bytes(pa as *mut u8, 0, FRAME_SIZE) };
    }

    let frame = match build_entry_stack(argv, env) {
        Ok(frame) => frame,
        Err(error) => {
            release_stack(stack_base);
            loaded.release();
            *RUN.lock() = None;
            return Err(error);
        }
    };

    {
        let mut slot = RUN.lock();
        if let Some(run) = slot.as_mut() {
            run.entry = loaded.entry;
            run.sp = frame.sp;
            run.argc = frame.argc;
            run.argv = frame.argv;
        }
    }

    let Some(id) = task::spawn(user_task) else {
        release_stack(stack_base);
        loaded.release();
        *RUN.lock() = None;
        return Err(UserError::TaskSlot);
    };

    wait_for_exit(id);
    // The task is `Exited`; its own PMM stack is reaped on this next scheduler
    // entry. Reap explicitly so the caller's ledger does not depend on when the
    // next unrelated tick happens.
    let _ = scheduler::reap();

    // The trap frame is gone with the task, so the user pages can go too. Heap
    // frames are owned by `RunState`, not by `Loaded`, so they are released
    // here explicitly — before `RunState` itself is dropped.
    release_stack(stack_base);
    loaded.release();
    let mut run = RUN.lock().take();
    if let Some(run) = run.as_mut() {
        release_heap(run);
    }
    let exit = run.and_then(|run| run.exit).unwrap_or(Exit::Fault {
        reason: "task ended without an exit status",
        cause: 0,
        sepc: 0,
    });
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

/// Build the SysV-style initial stack frame the ABI promises:
///
/// ```text
/// sp -> [argc][argv0..argvN-1][NULL][env0..envK-1][NULL][argv strings][env strings]
/// ```
///
/// The strings are placed first, growing down from [`USER_TOP`], so their
/// addresses are known before the pointer table is written. The table start is
/// then aligned down to 16 bytes, which is the entry `sp` alignment RISC-V
/// requires. Every write goes through `paging::translate` to the physical frame
/// (the loader's pattern): `SUM` stays clear, because this is S-mode writing
/// through the identity map, not the kernel borrowing a user pointer.
fn build_entry_stack(argv: &[&str], env: &[&str]) -> Result<EntryStack, UserError> {
    let argc = argv.len();
    let envc = env.len();
    let mut addresses = [0usize; MAX_ENTRY_STRINGS];
    let mut cursor = USER_TOP;
    let mut index = 0usize;
    for text in argv.iter().chain(env.iter()) {
        cursor = cursor
            .checked_sub(text.len() + 1)
            .filter(|start| *start >= USER_STACK_BASE)
            .ok_or(UserError::ArgumentsTooLong)?;
        addresses[index] = cursor;
        write_user_bytes(cursor, text.as_bytes())?;
        write_user_bytes(cursor + text.len(), &[0])?;
        index += 1;
    }
    // `[argc]` + `argv` + `NULL` + `envp` + `NULL`.
    let words = 1 + argc + 1 + envc + 1;
    let frame_bytes = words
        .checked_mul(core::mem::size_of::<u64>())
        .ok_or(UserError::ArgumentsTooLong)?;
    let sp = cursor
        .checked_sub(frame_bytes)
        .ok_or(UserError::ArgumentsTooLong)?
        & !0xf;
    if sp < USER_STACK_BASE {
        return Err(UserError::ArgumentsTooLong);
    }

    write_user_u64(sp, argc as u64)?;
    let mut slot = sp + 8;
    for address in &addresses[..argc] {
        write_user_u64(slot, *address as u64)?;
        slot += 8;
    }
    write_user_u64(slot, 0)?;
    slot += 8;
    for address in &addresses[argc..argc + envc] {
        write_user_u64(slot, *address as u64)?;
        slot += 8;
    }
    write_user_u64(slot, 0)?;
    Ok(EntryStack {
        sp,
        argc,
        argv: sp + 8,
    })
}

/// Copy `bytes` to the user virtual address `va`, translating page by page and
/// writing through the physical frame.
///
/// No `SUM`: the destination is a user page this kernel mapped in the same
/// Sv39 tree, and the physical frame is identity-mapped supervisor-RW, so an
/// S-mode store through `pa` is legitimate and cannot take a U-permission
/// fault. Returning an error instead of faulting is the house rule for every
/// mapping the runner writes itself.
fn write_user_bytes(va: usize, bytes: &[u8]) -> Result<(), UserError> {
    let mut offset = 0usize;
    while offset < bytes.len() {
        let address = va.checked_add(offset).ok_or(UserError::StackFrame)?;
        let page = address & !(FRAME_SIZE - 1);
        let Some(physical) = paging::translate(page) else {
            return Err(UserError::StackFrame);
        };
        let in_page = address & (FRAME_SIZE - 1);
        let take = (bytes.len() - offset).min(FRAME_SIZE - in_page);
        // SAFETY: `page` is a mapped user-stack page; `translate` returned its
        // frame, `in_page + take <= FRAME_SIZE`, and the frame is identity-mapped
        // supervisor-RW. `bytes` is a live kernel slice disjoint from it; the
        // store is how the initial stack becomes visible to U-mode.
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

/// Little-endian 64-bit store into the user stack (see [`write_user_bytes`]).
fn write_user_u64(va: usize, value: u64) -> Result<(), UserError> {
    write_user_bytes(va, &value.to_le_bytes())
}

/// Unmap and free every page the program mapped with `brk`.
///
/// Called by the runner after the task is gone and before `RunState` is dropped;
/// a second call (heap already shrunk to the base) has nothing to do.
fn release_heap(run: &mut RunState) {
    for index in 0..run.heap_count {
        let va = USER_HEAP_BASE + index * FRAME_SIZE;
        let _ = paging::unmap(va);
        if run.heap_frames[index] != 0 {
            pmm::free_frame(run.heap_frames[index]);
            run.heap_frames[index] = 0;
        }
    }
    run.heap_count = 0;
    run.heap_end = USER_HEAP_BASE;
}

/// Unmap the user stack pages and return the contiguous run to the PMM.
fn release_stack(base: usize) {
    for page in 0..USER_STACK_PAGES {
        let _ = paging::unmap(USER_STACK_BASE + page * FRAME_SIZE);
    }
    pmm::free_frames_contiguous(base, USER_STACK_PAGES);
}

/// Block the calling (shell) task until `id` is `Exited`.
///
/// `yield_now` is the whole wait primitive: the user task is the only runnable
/// work, so the shell comes back as soon as that task leaves the scheduler. A
/// user program that loops forever keeps the shell here, which is what running a
/// program means; it is not a scheduler hang.
fn wait_for_exit(id: u64) {
    loop {
        let mut infos = [TaskInfo::zeroed(); MAX_TASKS];
        let count = task::snapshot(&mut infos);
        let mut found = false;
        for info in &infos[..count] {
            if info.id == id {
                found = true;
                if info.state == State::Exited {
                    return;
                }
            }
        }
        if !found {
            // The table has no such thread: it has already been reaped and its
            // slot reused, or a failed spawn was cleaned up elsewhere. Either way
            // there is no task left to wait for.
            return;
        }
        scheduler::yield_now();
    }
}

/// The kernel-thread body of a user task: read the launch request, then never
/// come back from U-mode.
fn user_task() {
    let request = {
        let slot = RUN.lock();
        slot.as_ref()
            .map(|run| (run.entry, run.sp, run.argc, run.argv))
    };
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
/// This is the only `sret` that enters user code. It runs on the task's own
/// kernel stack and stores that stack pointer in `sscratch` first, so the first
/// trap from U-mode has somewhere safe to land (see the module docs). Interrupts
/// are masked across the `sscratch`/`sstatus` setup and the `sret` itself: a
/// timer tick in that window would otherwise look like a trap *from* U-mode and
/// would clear the re-armed `sscratch` before it is used.
///
/// The ABI ([`docs/abi.md`] in `pagh-api`) is: `a0 = argc`, `a1 = argv`,
/// `sp = initial_sp` pointing at the SysV frame. `a2` is not part of the
/// contract; `envp` is recovered by `libpagh`'s `_start` from `sp`
/// (`argv + argc + 1`), which is why it is not passed in a register.
///
/// # Safety
///
/// `entry`, `sp`, `argc` and `argv` must describe a loaded user image and the
/// initial stack frame [`build_entry_stack`] wrote (the only caller is
/// [`user_task`], fed by [`run_source`]), and this function must be called from
/// the task that owns the current kernel stack. It never returns.
unsafe fn enter_user(entry: usize, sp: usize, argc: usize, argv: usize) -> ! {
    // SAFETY: `sret` deliberately transfers control to U-mode at `entry`. Every
    // register operand is a value validated by the loader/frame builder;
    // interrupts are disabled before the CSRs that describe the return are set,
    // and `sret` re-enables them from `SPIE` only after the switch. The function
    // is `-> !` and `options(noreturn)` keeps the compiler from emitting a
    // return path that could execute with `sp` pointing at the user stack.
    // The return state is built here rather than with `csrci`/`csrsi`: SPP is
    // bit 8, SPIE is bit 5, and those immediates do not fit the 5-bit CSR
    // immediate field. A full read-modify-write of the value the `sret` will
    // consume is both simpler and self-documenting.
    //
    // `a0`/`a1` are declared as fixed-register inputs, so the compiler loads
    // `argc`/`argv` before the sequence starts and never allocates `entry`,
    // `sp` or `status` to them. `mv sp` comes after `csrw sscratch, sp`, which
    // is the kernel stack pointer the first trap will switch to.
    let mut status = crate::arch::riscv64::csr::read_sstatus();
    status &= !(crate::arch::riscv64::csr::SSTATUS_SIE
        | crate::arch::riscv64::csr::SSTATUS_SPP
        | crate::arch::riscv64::csr::SSTATUS_SUM);
    status |= crate::arch::riscv64::csr::SSTATUS_SPIE;
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

/// End the current user task with `code` (syscall 93). Never returns to U-mode.
pub(crate) fn exit_current(code: u32) -> ! {
    if let Some(run) = RUN.lock().as_mut() {
        run.exit = Some(Exit::Code(code));
    }
    scheduler::exit()
}

/// A U-mode exception: print one diagnostic line, record it as the run's result
/// and end the task. Never returns to U-mode.
///
/// The mapping cleanup is the runner's job (it knows the stack allocation and
/// the `Loaded` value); this path only has to leave the scheduler, which is what
/// lets the shell get its prompt back on the next schedule.
pub fn on_fault(scause: usize, sepc: usize, stval: usize) -> ! {
    let reason = crate::arch::riscv64::traps::cause_name(scause);
    let mut name = RunName::new("user");
    {
        let mut slot = RUN.lock();
        if let Some(run) = slot.as_mut() {
            name = run.name;
            run.exit = Some(Exit::Fault {
                reason,
                cause: scause,
                sepc,
            });
        }
    }
    crate::kprintln!(
        "[user] {} killed: {} (cause={}, stval={:#x}, sepc={:#x})",
        name.as_str(),
        reason,
        scause,
        stval,
        sepc
    );
    scheduler::exit()
}
