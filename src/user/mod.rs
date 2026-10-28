//! User mode (M6b): load a static RV64 ELF, run it in U-mode as a scheduler
//! task, serve its `ecall`s and survive its faults.
//!
//! # The shape of a run
//!
//! [`run_image`] is the whole user-visible operation, called from the shell:
//!
//! 1. [`elf::load`] validates the image and maps its `PT_LOAD` pages with `U=1`
//!    in the kernel's one Sv39 tree (see that module for the layout).
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

use crate::arch::riscv64::mmu::Flags;
use crate::memory::paging;
use crate::memory::pmm;
use crate::memory::FRAME_SIZE;
use crate::sync::Spinlock;
use crate::task::scheduler::{self, State};
use crate::task::{self, TaskInfo, MAX_TASKS};

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

pub use elf::{ElfError, USER_STACK_BASE, USER_STACK_PAGES, USER_TOP};

/// Longest program name kept for the fault diagnostic; the shell's path may be
/// longer and is truncated rather than allocated.
const NAME_CAPACITY: usize = 32;

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
    stack_top: usize,
    exit: Option<Exit>,
}

static RUN: Spinlock<Option<RunState>> = Spinlock::new(None);

/// Load `bytes` and run it in U-mode until `exit` or a fault, then return how it
/// ended. The image and stack mappings are gone before this returns.
///
/// `name` is the label in the fault diagnostic and the shell's result line; it is
/// copied into a fixed buffer because the trap handler needs it while the caller
/// is suspended.
pub fn run_image(bytes: &[u8], name: &str) -> Result<Exit, UserError> {
    {
        let mut slot = RUN.lock();
        if slot.is_some() {
            return Err(UserError::Busy);
        }
        *slot = Some(RunState {
            name: RunName::new(name),
            entry: 0,
            stack_top: USER_TOP,
            exit: None,
        });
    }

    let mut loaded = match elf::load(bytes, 0) {
        Ok(loaded) => loaded,
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
    }

    {
        let mut slot = RUN.lock();
        if let Some(run) = slot.as_mut() {
            run.entry = loaded.entry;
            run.stack_top = USER_TOP;
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

    // The trap frame is gone with the task, so the user pages can go too.
    release_stack(stack_base);
    loaded.release();

    let exit = RUN
        .lock()
        .take()
        .and_then(|run| run.exit)
        .unwrap_or(Exit::Fault {
            reason: "task ended without an exit status",
            cause: 0,
            sepc: 0,
        });
    Ok(exit)
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
        slot.as_ref().map(|run| (run.entry, run.stack_top))
    };
    match request {
        Some((entry, stack_top)) => {
            // SAFETY: `entry` was validated by the ELF loader to lie inside an
            // executable `PT_LOAD` page, `stack_top` is the top of the mapped
            // user stack, and this task is the only context that reaches U-mode
            // with these values. `enter_user` never returns.
            unsafe { enter_user(entry, stack_top) }
        }
        None => {
            crate::kprintln!("[user] task started without a launch request; ending it");
            scheduler::exit()
        }
    }
}

/// Leave S-mode for `entry` with `stack_top` as the user stack pointer.
///
/// This is the only `sret` that enters user code. It runs on the task's own
/// kernel stack and stores that stack pointer in `sscratch` first, so the first
/// trap from U-mode has somewhere safe to land (see the module docs). Interrupts
/// are masked across the `sscratch`/`sstatus` setup and the `sret` itself: a
/// timer tick in that window would otherwise look like a trap *from* U-mode and
/// would clear the re-armed `sscratch` before it is used.
///
/// # Safety
///
/// `entry` and `stack_top` must describe a loaded user image and its mapped
/// stack (the only caller is [`user_task`], fed by [`run_image`]), and this
/// function must be called from the task that owns the current kernel stack. It
/// never returns.
unsafe fn enter_user(entry: usize, stack_top: usize) -> ! {
    // SAFETY: `sret` deliberately transfers control to U-mode at `entry`. Every
    // register operand is a value validated by the loader; interrupts are
    // disabled before the CSRs that describe the return are set, and `sret`
    // re-enables them from `SPIE` only after the switch. The function is
    // `-> !` and `options(noreturn)` keeps the compiler from emitting a return
    // path that could execute with `sp` pointing at the user stack.
    // The return state is built here rather than with `csrci`/`csrsi`: SPP is
    // bit 8, SPIE is bit 5, and those immediates do not fit the 5-bit CSR
    // immediate field. A full read-modify-write of the value the `sret` will
    // consume is both simpler and self-documenting.
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
            stack = in(reg) stack_top,
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
