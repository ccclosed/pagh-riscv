//! POSIX-style signals for the native user ABI.
//!
//! Scope: the six syscalls the ABI exposes (`kill`, `tgkill`, `rt_sigaction`,
//! `rt_sigprocmask`, `rt_sigreturn`, `sigaltstack`), per-process handlers,
//! per-thread blocked masks, pending sets and alternate stacks, a user-memory
//! signal frame, synchronous delivery for the faults that have a POSIX name
//! (`SIGSEGV`, `SIGILL`, `SIGBUS`, `SIGTRAP`), and the four job-control stops
//! with `SIGCONT`.
//!
//! # What a signal is here
//!
//! One process owns one handler table; the pending mask, the blocked mask and
//! the alternate signal stack are per thread (a `clone` child has its own).
//! `fork` copies the handlers, the caller's blocked mask and the alternate-stack
//! settings, never the pending bits; `execve` restores every handler to
//! `SIG_DFL`, disables the alternate stack and drops pending bits, while the
//! blocked mask survives, as POSIX specifies. `kill` with the caller's process
//! id targets its main thread (or the first live thread when the main thread is
//! gone); any other positive id must name a live thread of the caller's process.
//! `kill(0)`/`kill(-1)` target every live thread of the calling process.
//! Cross-process `kill` is not implemented: there is no permission model and no
//! process group in this kernel. Delivery is asynchronous from the sender's
//! point of view: the target
//! marks the signal pending, and the *target's* next kernel-to-user boundary
//! installs the handler. That boundary is the end of every syscall and the end
//! of every trap taken from U-mode, including the timer tick, so a running
//! thread is interrupted within one tick at worst. A thread blocked in `futex`
//! is woken by the sender (`scheduler::wake_task`), so its wait returns
//! `-EINTR` and the handler runs at the boundary right after it.
//!
//! # Signal frame
//!
//! The frame is user memory below the target's current `sp` (or below the top
//! of its alternate stack when `SA_ONSTACK` selects one), 16-byte aligned:
//!
//! ```text
//! offset   0  saved TrapFrame (264 B, the frame `trap_entry` built)
//! offset 264  blocked mask to restore (8 B)
//! offset 272  magic (8 B)
//! offset 280  restorer address (8 B)
//! offset 288  siginfo_t (128 B, layout below)
//! offset 416  previous signal-frame pointer (8 B, 0 outermost)
//! offset 424  signum (8 B)
//! offset 432  ucontext_t (288 B, layout below)
//! offset 720  frame flags (8 B, bit 0 = this frame switched to the alt stack)
//! offset 728  reserved (8 B, 0)
//! total      736 B (16-byte aligned)
//! ```
//!
//! `siginfo_t` (128 B, little-endian, pagh layout):
//!
//! | offset | field |
//! |---:|---|
//! | 0 | `si_signo` (i32) |
//! | 4 | `si_errno` (i32, 0) |
//! | 8 | `si_code` (i32: `SI_USER=0`, `SI_KERNEL=0x80`, `SI_TKILL=-6`, `SEGV_MAPERR=1`, `TRAP_BRKPT=1`, `BUS_ADRALN=1`) |
//! | 12 | padding |
//! | 16 | `si_pid` (u32) and `si_uid` (u32) for `kill`/`tgkill`; `si_addr` (u64) for a synchronous fault |
//! | 24 | `si_status` (i32, 0), meaningful only for a child-exit `SIGCHLD` (no producer yet) |
//! | 28 | padding to 128 B |
//!
//! `ucontext_t` (288 B, pagh layout; this is **not** Linux's `ucontext_t`):
//!
//! | offset | field |
//! |---:|---|
//! | 0 | `uc_flags` (u64, 0) |
//! | 8 | `uc_link` (u64): `&ucontext` of the previous frame, 0 outermost |
//! | 16 | `uc_sigmask` (u64): the blocked mask in force *inside* the handler |
//! | 24 | `mcontext`: `gregs[32]` (u64, x0..x31: x0=0, x1=ra, x2=sp, x3=gp, x4=tp, then t0..t6/s0..s11/a0..a7 in RISC-V numeric order), then `pc` (u64, the interrupted `sepc`) |
//!
//! The handler may read the `ucontext` snapshot; `rt_sigreturn` restores the
//! saved `TrapFrame`, so a write to `ucontext` is ignored, not a way to change
//! the resumed context (documented, not a promise of Linux's mutable context).
//!
//! Every page of `[frame, frame + 736)` is validated `U|R|W` before the copy, so
//! a bad stack is a signal death (and never a kernel fault). `rt_sigreturn`
//! validates the same range for reads, requires `sp` to equal the frame the
//! kernel last armed for this thread, checks the magic and the frame flags, and
//! then restores the saved frame with a sanitized `sstatus` (`SPP=0`, `SIE=0`,
//! `SUM=0`, `SPIE=1`), the pre-handler blocked mask and the alternate-stack
//! depth the frame recorded. The user cannot turn a forged frame into S-mode
//! execution.
//!
//! # Alternate signal stack
//!
//! `sigaltstack` installs a per-thread stack (`SS_DISABLE=2` disables it,
//! `SS_ONSTACK=1` is reported while a handler is running on it). The kernel
//! layout is `struct { ss_sp: u64, ss_flags: i32, pad: i32, ss_size: u64 }`
//! (24 B). `SA_ONSTACK` (`0x0800_0000`) makes delivery use the top of that
//! stack; the size must be at least `MINSIGSTKSZ` (4 KiB) and every page must be
//! mapped `U|R|W`. With no stack installed (or `SS_DISABLE`), `SA_ONSTACK` falls
//! back to the normal stack, as POSIX permits. A signal already running on the
//! alternate stack keeps its current stack for nested delivery (there is no
//! recursive switch to the same stack top). An installed stack that is unmapped
//! before delivery is a clean signal death ("bad alternate signal stack"), never
//! a kernel fault.
//!
//! # Handler ABI and flags
//!
//! Without `SA_SIGINFO` the handler is called as `handler(signo, *siginfo)` (the
//! pagh convention this ABI shipped with). With `SA_SIGINFO` (`0x4`) it is
//! `handler(signo, *siginfo, *ucontext)`. `SA_RESTORER` (`0x0400_0000`) and a
//! user-executable restorer are required for every real handler: there is no
//! vDSO trampoline. Accepted flag bits are exactly
//! `SA_RESTORER|SA_ONSTACK|SA_RESTART|SA_NODEFER|SA_RESETHAND|SA_SIGINFO`; any
//! other bit is `-EINVAL`.
//!
//! * `SA_NODEFER` (`0x4000_0000`): the delivered signal is not added to the
//!   handler's mask, so the handler can take it again (recursively through
//!   `kill`), until it chooses to stop.
//! * `SA_RESETHAND` (`0x8000_0000`): the disposition becomes `SIG_DFL` as the
//!   handler is entered. A second delivery then takes the default action.
//! * `SA_RESTART` (`0x1000_0000`): honoured for `futex` and `nanosleep` only.
//!   When such a wait returns `-EINTR` because of this signal, the kernel leaves
//!   `sepc` on the `ecall` and `a0` untouched, so `rt_sigreturn` resumes the
//!   syscall with its original arguments. The wait restarts with its **full**
//!   requested duration/timeout, not the remaining one (`rem` is not tracked);
//!   that is the documented minimal behaviour. Every other syscall returns
//!   `-EINTR` as before.
//!
//! # Default actions and job control
//!
//! `SIG_DFL` terminates the *run* for the POSIX-fatal signals (SIGTERM, SIGINT,
//! SIGQUIT, SIGABRT, SIGSEGV, SIGILL, SIGFPE, SIGBUS, SIGKILL and the rest of
//! the core family); SIGCHLD, SIGCONT, SIGURG and SIGWINCH are ignored. This run
//! is the process, so a fatal default action retires every thread, exactly like
//! `exit_group`/a fault; the runner does the one teardown of shared state.
//!
//! `SIGSTOP(19)`, `SIGTSTP(20)`, `SIGTTIN(21)` and `SIGTTOU(22)` with `SIG_DFL`
//! stop the target thread in the scheduler (`State::Stopped`: not scheduled,
//! keeps its kernel stack and signal state). A catchable stop signal with a
//! handler is delivered like any other signal; `SIG_IGN` ignores it.
//! `SIGCONT(18)` resumes a stopped thread and clears a still-pending stop bit
//! for it. `kill(0)`/`kill(-1)` address every thread of the run, so a stop sent
//! that way stops the whole run once each thread next reaches a boundary. There
//! is no tty and no process-group model: the shell cannot continue a run it
//! stopped, the only `SIGCONT` is one the program sends itself or gets from
//! another thread, and stopping every thread of an otherwise idle run parks it
//! until the shell is rebooted. That limit is deliberate and documented.
//!
//! # Deliberate limits (documented, not faked)
//!
//! * No `sigqueue`, no `sival_ptr`, no per-signal queueing: one pending bit per
//!   signal per thread, and `si_code`/`si_pid`/`si_uid` of the last sender win.
//! * No `SIGFPE` producer: RISC-V integer division has no trap, and this kernel
//!   has no floating point in U-mode; SIGFPE is deliverable only by
//!   `kill`/`tgkill`, with `SI_USER`/`SI_TKILL`.
//! * No child-status source: `SIGCHLD` can be sent explicitly, but with
//!   `SI_USER`, `si_pid` = sender and `si_status = 0`; `fork`/`wait` own the
//!   real child-exit fields in the field layout above.
//! * `read(0)` is non-blocking by contract and returns `-EAGAIN` as before.
//! * `SA_RESTART` is honoured only for `futex`/`nanosleep`, as described above.
//!
//! # Lock order
//!
//! `signal table -> (nothing)`. The table lock is never held across a user copy,
//! a VFS call or `scheduler::wake_task`/`scheduler::continue_task`; `RUN` may be
//! taken before the signal lock (the run id is read first), and the scheduler
//! lock is only taken inside `thread_group`/`group_threads`/`wake_task`/
//! `continue_task` with the signal lock released. Every table access masks
//! interrupts, because `interrupted()` is called from `nanosleep` with `SIE` on.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::arch::riscv64::csr;
use crate::arch::riscv64::mmu::{PTE_U, PTE_X};
use crate::arch::riscv64::traps::TrapFrame;
use crate::memory::{paging, FRAME_SIZE};
use crate::sync::Spinlock;
use crate::task::scheduler;
use crate::user::process::MAX_PROCESSES;

/// Signal numbers the ABI can name. Index 0 is never a valid signal.
pub(crate) const MAX_SIGNALS: usize = 32;

/// Valid bits of a signal mask: signal `n` is bit `n - 1`, so signals 1..=31.
pub(crate) const VALID_MASK: u64 = (1 << 31) - 1;

/// SIGKILL and SIGSTOP cannot be blocked or caught (POSIX).
const UNBLOCKABLE_MASK: u64 = (1 << (9 - 1)) | (1 << (19 - 1));

/// The four job-control stops. A pending bit for any of them is cleared when
/// SIGCONT resumes the thread, so a stop that raced with the continue cannot
/// re-stop it.
const JOB_STOP_MASK: u64 = (1 << (19 - 1)) | (1 << (20 - 1)) | (1 << (21 - 1)) | (1 << (22 - 1));

const SIGKILL: usize = 9;
const SIGSTOP: usize = 19;
const SIGCONT: usize = 18;

/// `struct sigaction` flags bit that says `sa_restorer` is meaningful.
pub(crate) const SA_RESTORER: usize = 0x0400_0000;
/// Deliver on the thread's alternate signal stack when one is installed.
pub(crate) const SA_ONSTACK: usize = 0x0800_0000;
/// Restart an interrupted `futex`/`nanosleep` wait (see the module docs).
pub(crate) const SA_RESTART: usize = 0x1000_0000;
/// Do not block the delivered signal in the handler.
pub(crate) const SA_NODEFER: usize = 0x4000_0000;
/// Reset the disposition to `SIG_DFL` when the handler is entered.
pub(crate) const SA_RESETHAND: usize = 0x8000_0000;
/// Call the handler as `handler(signo, *siginfo, *ucontext)`.
pub(crate) const SA_SIGINFO: usize = 0x0000_0004;

const SA_SUPPORTED: usize =
    SA_RESTORER | SA_ONSTACK | SA_RESTART | SA_NODEFER | SA_RESETHAND | SA_SIGINFO;

/// `SIG_DFL`: the default action.
pub(crate) const SIG_DFL: usize = 0;
/// `SIG_IGN`: discard the signal.
pub(crate) const SIG_IGN: usize = 1;

/// `struct sigaction` in pagh's ABI: handler, flags, mask, restorer.
///
/// This is deliberately pagh's order, not Linux's `handler/flags/restorer/mask`;
/// the native ABI document owns the layout and the fixture uses it directly.
const SIGACTION_SIZE: usize = 32;

/// `struct sigaltstack` in pagh's ABI: `ss_sp` (u64), `ss_flags` (i32), padding
/// (i32), `ss_size` (u64) — 24 bytes.
const SIGALTSTACK_SIZE: usize = 24;
/// `ss_flags`: the handler is running on this stack.
const SS_ONSTACK: i32 = 1;
/// `ss_flags`: no alternate stack is installed.
const SS_DISABLE: i32 = 2;
/// POSIX's minimum usable alternate stack (the ABI documents the exact 4 KiB).
const MINSIGSTKSZ: usize = 4096;

/// Maximum live threads a run can have (the scheduler's table minus the shell
/// and idle slots). A thread-signal entry is needed only while a thread uses
/// signals, so this is an upper bound, not a reservation.
const MAX_THREADS: usize = scheduler::MAX_TASKS;

/// Bytes of the signal frame written below the interrupted (or alternate) `sp`.
const SIGFRAME_SIZE: usize = 736;
const SIGFRAME_MAGIC: u64 = 0x7061_6768_7369_6700;
/// Frame flag: this delivery switched from the interrupted stack to the
/// thread's alternate signal stack; `rt_sigreturn` pops one alt-stack level.
const SIGFRAME_USED_ALTSTACK: u64 = 1;
const SIGFRAME_KNOWN_FLAGS: u64 = SIGFRAME_USED_ALTSTACK;

/// Bytes of the `siginfo_t` delivered in `a1`.
const SIGINFO_SIZE: usize = 128;
/// `SI_USER`: the signal came from `kill`/`tgkill`.
const SI_USER: i32 = 0;
/// `SI_KERNEL`: generated by the kernel with no more specific code (SIGILL: the
/// architecture merges illegal opcode, privileged instruction and illegal CSR).
const SI_KERNEL: i32 = 0x80;
/// `SI_TKILL`: the signal came from `tgkill`.
const SI_TKILL: i32 = -6;
/// `SEGV_MAPERR`: the synchronous page fault has no matching object (our map has
/// no per-object fault information, so this is the honest code).
const SEGV_MAPERR: i32 = 1;
/// `TRAP_BRKPT`: the RISC-V breakpoint exception.
const TRAP_BRKPT: i32 = 1;
/// `BUS_ADRALN`: a misaligned load/store (the only SIGBUS source here).
const BUS_ADRALN: i32 = 1;

/// Bytes of the user `ucontext_t` snapshot.
const UCONTEXT_SIZE: usize = 288;
/// Bytes of the `mcontext` inside it: 32 general registers plus `pc`.
const MCONTEXT_SIZE: usize = 264;

const ESRCH: isize = -3;
const EINVAL: isize = -22;
const ENOMEM: isize = -12;

/// One installed handler and its `rt_sigaction` fields.
#[derive(Clone, Copy)]
struct Action {
    handler: usize,
    flags: usize,
    mask: u64,
    restorer: usize,
}

impl Action {
    const fn default() -> Self {
        Action {
            handler: SIG_DFL,
            flags: 0,
            mask: 0,
            restorer: 0,
        }
    }
}

/// One thread's signal state. `run`/`tid` identify the owner; a slot is reused
/// only after `clear_run` (or a new `begin_run`) has reset the table.
#[derive(Clone, Copy)]
struct ThreadSignal {
    used: bool,
    run: u64,
    tid: u64,
    /// Signals sent and not yet delivered or discarded (bit `n - 1`).
    pending: u64,
    /// Signals masked for this thread (bit `n - 1`).
    blocked: u64,
    /// The signal frame the kernel armed most recently, or 0 when no handler is
    /// active. `rt_sigreturn` accepts only this address.
    frame: usize,
    /// Alternate stack installed by `sigaltstack` (`alt_disabled` when none).
    alt_sp: usize,
    alt_size: usize,
    alt_disabled: bool,
    /// How many active handler frames switched to the alternate stack. Non-zero
    /// is what `sigaltstack` reports as `SS_ONSTACK`, and what keeps a nested
    /// delivery from switching to the same stack top again.
    alt_active: u32,
    /// This thread's `SA_RESTART` request, armed by [`interrupted`] and consumed
    /// by `dispatch`. Per thread on purpose: `nanosleep` runs with interrupts
    /// enabled, so between arming and consuming it a tick can switch to another
    /// task, whose own `-EINTR` must not consume this request.
    restart: bool,
    /// Sender identity and `si_code` stored with each pending bit, so delivery
    /// can fill `siginfo` after the sender has gone. A second send for a signal
    /// that is already pending overwrites these (one pending entry per signal).
    pending_pid: [u32; MAX_SIGNALS],
    pending_uid: [u32; MAX_SIGNALS],
    pending_code: [i32; MAX_SIGNALS],
}

impl ThreadSignal {
    const fn empty() -> Self {
        ThreadSignal {
            used: false,
            run: 0,
            tid: 0,
            pending: 0,
            blocked: 0,
            frame: 0,
            alt_sp: 0,
            alt_size: 0,
            alt_disabled: true,
            alt_active: 0,
            restart: false,
            pending_pid: [0; MAX_SIGNALS],
            pending_uid: [0; MAX_SIGNALS],
            pending_code: [0; MAX_SIGNALS],
        }
    }

    fn new(run: u64, tid: u64) -> Self {
        ThreadSignal {
            used: true,
            run,
            tid,
            ..ThreadSignal::empty()
        }
    }
}

/// One process's installed handlers.
///
/// The handler table belongs to the process, not to a thread: `fork` copies it,
/// `execve` resets it, and a `clone` sibling shares it. The row is keyed by the
/// process-table slot and carries the pid, so a reused slot can never inherit a
/// stale generation.
struct ProcessSignals {
    used: bool,
    pid: u64,
    actions: [Action; MAX_SIGNALS],
}

impl ProcessSignals {
    const fn empty() -> Self {
        ProcessSignals {
            used: false,
            pid: 0,
            actions: [Action::default(); MAX_SIGNALS],
        }
    }
}

/// The per-process handler rows and the per-thread pending/blocked/altstack
/// state.
struct SignalTable {
    processes: [ProcessSignals; MAX_PROCESSES],
    threads: [ThreadSignal; MAX_THREADS],
}

impl SignalTable {
    const fn empty() -> Self {
        SignalTable {
            processes: [const { ProcessSignals::empty() }; MAX_PROCESSES],
            threads: [ThreadSignal::empty(); MAX_THREADS],
        }
    }

    fn process_index(&self, pid: u64) -> Option<usize> {
        self.processes
            .iter()
            .position(|entry| entry.used && entry.pid == pid)
    }

    fn thread_index(&self, tid: u64) -> Option<usize> {
        self.threads
            .iter()
            .position(|entry| entry.used && entry.tid == tid)
    }

    /// Find or create the entry for `tid`, refusing a tid that already belongs
    /// to a different run.
    fn ensure_thread(&mut self, run: u64, tid: u64) -> Option<usize> {
        if let Some(index) = self.thread_index(tid) {
            return (self.threads[index].run == run).then_some(index);
        }
        let index = self.threads.iter().position(|entry| !entry.used)?;
        self.threads[index] = ThreadSignal::new(run, tid);
        Some(index)
    }

    fn live_handlers(&self) -> usize {
        self.processes
            .iter()
            .filter(|process| process.used)
            .map(|process| {
                process
                    .actions
                    .iter()
                    .skip(1)
                    .filter(|action| action.handler != SIG_DFL)
                    .count()
            })
            .sum()
    }

    fn live_threads(&self) -> usize {
        self.threads.iter().filter(|entry| entry.used).count()
    }
}

static SIGNALS: Spinlock<SignalTable> = Spinlock::new(SignalTable::empty());

static SIGACTIONS: AtomicU64 = AtomicU64::new(0);
static SIGPROCMASKS: AtomicU64 = AtomicU64::new(0);
static KILLS: AtomicU64 = AtomicU64::new(0);
static SIGRETURNS: AtomicU64 = AtomicU64::new(0);
static DELIVERED: AtomicU64 = AtomicU64::new(0);
static IGNORED: AtomicU64 = AtomicU64::new(0);
static TERMINATED: AtomicU64 = AtomicU64::new(0);
static FAULT_SIGNALS: AtomicU64 = AtomicU64::new(0);
/// Signals that interrupted a `futex`/`nanosleep` wait (the `-EINTR` path).
static INTERRUPTS: AtomicU64 = AtomicU64::new(0);
static SIGALTSTACKS: AtomicU64 = AtomicU64::new(0);
static STOPS: AtomicU64 = AtomicU64::new(0);
static CONTINUES: AtomicU64 = AtomicU64::new(0);
static RESTARTS: AtomicU64 = AtomicU64::new(0);

/// Counters the selftest reads as evidence that the signal path ran.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub actions: u64,
    pub procmasks: u64,
    pub kills: u64,
    pub sigreturns: u64,
    pub delivered: u64,
    pub ignored: u64,
    pub terminated: u64,
    pub faults: u64,
    pub interrupts: u64,
    pub altstacks: u64,
    pub stops: u64,
    pub continues: u64,
    pub restarts: u64,
}

/// A snapshot of the signal counters.
pub fn stats() -> Stats {
    Stats {
        actions: SIGACTIONS.load(Ordering::Relaxed),
        procmasks: SIGPROCMASKS.load(Ordering::Relaxed),
        kills: KILLS.load(Ordering::Relaxed),
        sigreturns: SIGRETURNS.load(Ordering::Relaxed),
        delivered: DELIVERED.load(Ordering::Relaxed),
        ignored: IGNORED.load(Ordering::Relaxed),
        terminated: TERMINATED.load(Ordering::Relaxed),
        faults: FAULT_SIGNALS.load(Ordering::Relaxed),
        interrupts: INTERRUPTS.load(Ordering::Relaxed),
        altstacks: SIGALTSTACKS.load(Ordering::Relaxed),
        stops: STOPS.load(Ordering::Relaxed),
        continues: CONTINUES.load(Ordering::Relaxed),
        restarts: RESTARTS.load(Ordering::Relaxed),
    }
}

/// Handlers installed (the selftest requires 0 after a run is torn down).
pub(crate) fn live_handlers() -> usize {
    csr::without_interrupts(|| SIGNALS.lock().live_handlers())
}

/// Threads with a signal entry (the selftest requires 0 after a run).
pub(crate) fn live_threads() -> usize {
    csr::without_interrupts(|| SIGNALS.lock().live_threads())
}

/// Signals still pending somewhere (the selftest requires 0 after a run).
pub(crate) fn live_pending() -> usize {
    csr::without_interrupts(|| {
        SIGNALS
            .lock()
            .threads
            .iter()
            .filter(|entry| entry.used && entry.pending != 0)
            .count()
    })
}

/// Install a fresh handler row for a new process. Called before the process's
/// first task can issue an `ecall`.
pub(crate) fn begin_process(slot: usize, pid: u64) {
    csr::without_interrupts(|| {
        let mut table = SIGNALS.lock();
        if let Some(entry) = table.processes.get_mut(slot) {
            *entry = ProcessSignals {
                used: true,
                pid,
                actions: [Action::default(); MAX_SIGNALS],
            };
        }
    });
}

/// Copy `parent`'s handlers, the caller's blocked mask and its alternate-stack
/// settings into the child. Pending bits and an armed frame are deliberately
/// not copied: the child has not run yet, and a signal queued for the parent
/// must not be delivered twice.
pub(crate) fn fork_process(
    parent_pid: u64,
    child_pid: u64,
    parent_slot: usize,
    child_slot: usize,
    child_tid: u64,
) -> bool {
    csr::without_interrupts(|| {
        let mut table = SIGNALS.lock();
        let Some(parent) = table.processes.get(parent_slot) else {
            return false;
        };
        if !parent.used || parent.pid != parent_pid {
            return false;
        }
        let actions = parent.actions;
        if let Some(entry) = table.processes.get_mut(child_slot) {
            *entry = ProcessSignals {
                used: true,
                pid: child_pid,
                actions,
            };
        } else {
            return false;
        }
        // The child's main thread inherits the blocked mask and alternate-stack
        // configuration of the thread that called fork; pending signals and the
        // "already on the alternate stack" depth start empty.
        let inherited = table
            .threads
            .iter()
            .find(|entry| entry.used && entry.tid == scheduler::current_id())
            .map(|entry| {
                (
                    entry.blocked,
                    entry.alt_sp,
                    entry.alt_size,
                    entry.alt_disabled,
                )
            })
            .unwrap_or((0, 0, 0, true));
        let Some(index) = table.ensure_thread(child_pid, child_tid) else {
            return false;
        };
        let entry = &mut table.threads[index];
        entry.blocked = inherited.0;
        entry.alt_sp = inherited.1;
        entry.alt_size = inherited.2;
        entry.alt_disabled = inherited.3;
        entry.alt_active = 0;
        entry.pending = 0;
        entry.frame = 0;
        entry.restart = false;
        true
    })
}

/// `execve`: every handler returns to `SIG_DFL`, the alternate stack is
/// disabled, pending signals and armed frames are dropped, and the blocked mask
/// is kept (POSIX keeps the mask across `exec`).
pub(crate) fn exec_process(slot: usize, pid: u64) {
    csr::without_interrupts(|| {
        let mut table = SIGNALS.lock();
        if let Some(entry) = table.processes.get_mut(slot) {
            if entry.used && entry.pid == pid {
                entry.actions = [Action::default(); MAX_SIGNALS];
            }
        }
        for thread in table.threads.iter_mut() {
            if thread.used && thread.run == pid {
                thread.pending = 0;
                thread.frame = 0;
                thread.alt_sp = 0;
                thread.alt_size = 0;
                thread.alt_disabled = true;
                thread.alt_active = 0;
                thread.restart = false;
            }
        }
    });
}

/// Forget everything a finished process installed. Called by the runner or a
/// parent's `wait4` after the last thread is gone.
pub(crate) fn clear_process(slot: usize, pid: u64) {
    csr::without_interrupts(|| {
        let mut table = SIGNALS.lock();
        if let Some(entry) = table.processes.get_mut(slot) {
            if entry.used && entry.pid == pid {
                *entry = ProcessSignals::empty();
            }
        }
        for thread in table.threads.iter_mut() {
            if thread.used && thread.run == pid {
                *thread = ThreadSignal::empty();
            }
        }
    });
}

/// The signal name printed in the default-action diagnostic.
pub(crate) fn signal_name(signal: usize) -> &'static str {
    match signal {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        4 => "SIGILL",
        5 => "SIGTRAP",
        6 => "SIGABRT",
        7 => "SIGBUS",
        8 => "SIGFPE",
        9 => "SIGKILL",
        10 => "SIGUSR1",
        11 => "SIGSEGV",
        12 => "SIGUSR2",
        13 => "SIGPIPE",
        14 => "SIGALRM",
        15 => "SIGTERM",
        16 => "SIGSTKFLT",
        17 => "SIGCHLD",
        18 => "SIGCONT",
        19 => "SIGSTOP",
        20 => "SIGTSTP",
        21 => "SIGTTIN",
        22 => "SIGTTOU",
        23 => "SIGURG",
        24 => "SIGXCPU",
        25 => "SIGXFSZ",
        26 => "SIGVTALRM",
        27 => "SIGPROF",
        28 => "SIGWINCH",
        29 => "SIGIO",
        30 => "SIGPWR",
        31 => "SIGSYS",
        _ => "unknown signal",
    }
}

/// True when the default action for `signal` is "ignore" (POSIX's job-control
/// and child-status signals, none of which this kernel has state for).
fn default_ignored(signal: usize) -> bool {
    matches!(signal, 17 | 18 | 23 | 28)
}

/// True when `SIG_DFL` stops the target thread rather than ending the run.
fn default_stops(signal: usize) -> bool {
    matches!(signal, 19..=22)
}

/// Why a signal is being delivered; decides the `siginfo` payload.
#[derive(Clone, Copy)]
enum Source {
    /// `kill`/`tgkill`: `pid`/`uid` of the sender and `SI_USER`/`SI_TKILL`.
    User { pid: u32, uid: u32, code: i32 },
    /// A synchronous U-mode fault, with the faulting address as `si_addr`.
    Fault { address: usize, code: i32 },
}

impl Source {
    fn code(self) -> i32 {
        match self {
            Source::User { code, .. } | Source::Fault { code, .. } => code,
        }
    }

    fn pid(self) -> u32 {
        match self {
            Source::User { pid, .. } => pid,
            Source::Fault { .. } => 0,
        }
    }

    fn uid(self) -> u32 {
        match self {
            Source::User { uid, .. } => uid,
            Source::Fault { .. } => 0,
        }
    }
}

/// The `mcontext` snapshot: RISC-V `x0..x31` then the interrupted `pc`.
#[repr(C)]
#[derive(Clone, Copy)]
struct MContext {
    gregs: [u64; 32],
    pc: u64,
}

/// The `ucontext_t` snapshot. Read-only for the handler; `rt_sigreturn` restores
/// the frame's saved `TrapFrame`, not this copy.
#[repr(C)]
#[derive(Clone, Copy)]
struct UContext {
    flags: u64,
    link: u64,
    sigmask: u64,
    mcontext: MContext,
}

const _: () = assert!(core::mem::size_of::<MContext>() == MCONTEXT_SIZE);
const _: () = assert!(core::mem::size_of::<UContext>() == UCONTEXT_SIZE);

/// The user-memory signal frame (`SIGFRAME_SIZE` bytes, no padding).
#[repr(C)]
struct SignalFrame {
    saved: TrapFrame,
    mask: u64,
    magic: u64,
    restorer: u64,
    siginfo: [u8; SIGINFO_SIZE],
    prev_frame: u64,
    signum: u64,
    ucontext: UContext,
    flags: u64,
    reserved: u64,
}

const _: () = assert!(core::mem::size_of::<SignalFrame>() == SIGFRAME_SIZE);
const _: () = assert!(core::mem::offset_of!(SignalFrame, siginfo) == 288);
const _: () = assert!(core::mem::offset_of!(SignalFrame, ucontext) == 432);

/// The handler `pid` installed for `signal`, if the process still has a row.
fn action_of(table: &SignalTable, pid: u64, signal: usize) -> Option<Action> {
    table
        .process_index(pid)
        .map(|index| table.processes[index].actions[signal])
}

/// Run `f` with the signal table locked and interrupts masked.
///
/// Single-hart discipline: the lock protects against a trap handler, not another
/// hart, and a handler that takes it while a `nanosleep` caller holds it would
/// deadlock. Masking is therefore part of the lock, like `user::futex`.
fn with_table<R>(f: impl FnOnce(&mut SignalTable) -> R) -> R {
    csr::without_interrupts(|| {
        let mut table = SIGNALS.lock();
        f(&mut table)
    })
}

/// True for an address in a user page that U-mode may execute.
fn user_executable(address: usize) -> bool {
    if address == 0 {
        return false;
    }
    paging::flags(address & !(FRAME_SIZE - 1))
        .map(|flags| flags.bits() & (PTE_U | PTE_X) == PTE_U | PTE_X)
        .unwrap_or(false)
}

/// True when `frame` is a usable saved frame base for the signal back-chain:
/// either `0` ("no previous frame") or a whole [`SIGFRAME_SIZE`] signal frame
/// inside the caller's mapped user range, sixteen-byte aligned the way [`arm`]
/// writes it.
///
/// `prev_frame` is the one signal-frame field a handler can rewrite before
/// `rt_sigreturn`, and it used to cross from user memory into the thread's
/// signal state unchecked. The next delivery then computed
/// `prev_frame + offset_of!(SignalFrame, ucontext)` with an ordinary `+`: a
/// forged `usize::MAX` overflowed, the debug kernel panicked, and the hart was
/// parked (F-r3-rv-1). Both the install (`sigreturn`) and the use (`arm`) go
/// through this predicate; a forged chain is `-EINVAL`, never a machine kill.
fn valid_saved_frame(frame: usize) -> bool {
    if frame == 0 {
        return true;
    }
    if frame & 0xf != 0 {
        return false;
    }
    match frame.checked_add(SIGFRAME_SIZE) {
        Some(end) => crate::user::syscall::user_range_valid(frame, end, false),
        None => false,
    }
}

/// Decode the 32-byte `struct sigaction`.
fn decode_action(raw: &[u8; SIGACTION_SIZE]) -> Action {
    Action {
        handler: u64::from_le_bytes(raw[0..8].try_into().unwrap_or([0; 8])) as usize,
        flags: u64::from_le_bytes(raw[8..16].try_into().unwrap_or([0; 8])) as usize,
        mask: u64::from_le_bytes(raw[16..24].try_into().unwrap_or([0; 8])),
        restorer: u64::from_le_bytes(raw[24..32].try_into().unwrap_or([0; 8])) as usize,
    }
}

/// Encode the 32-byte `struct sigaction`.
fn encode_action(action: &Action) -> [u8; SIGACTION_SIZE] {
    let mut raw = [0u8; SIGACTION_SIZE];
    raw[0..8].copy_from_slice(&(action.handler as u64).to_le_bytes());
    raw[8..16].copy_from_slice(&(action.flags as u64).to_le_bytes());
    raw[16..24].copy_from_slice(&action.mask.to_le_bytes());
    raw[24..32].copy_from_slice(&(action.restorer as u64).to_le_bytes());
    raw
}

/// Decode the 24-byte `struct sigaltstack`.
fn decode_altstack(raw: &[u8; SIGALTSTACK_SIZE]) -> (usize, i32, usize) {
    let sp = u64::from_le_bytes(raw[0..8].try_into().unwrap_or([0; 8])) as usize;
    let flags = i32::from_le_bytes(raw[8..12].try_into().unwrap_or([0; 4]));
    let size = u64::from_le_bytes(raw[16..24].try_into().unwrap_or([0; 8])) as usize;
    (sp, flags, size)
}

/// Encode the 24-byte `struct sigaltstack` with `ss_flags` for this instant.
fn encode_altstack(sp: usize, flags: i32, size: usize) -> [u8; SIGALTSTACK_SIZE] {
    let mut raw = [0u8; SIGALTSTACK_SIZE];
    raw[0..8].copy_from_slice(&(sp as u64).to_le_bytes());
    raw[8..12].copy_from_slice(&flags.to_le_bytes());
    raw[16..24].copy_from_slice(&(size as u64).to_le_bytes());
    raw
}

/// `rt_sigaction(signum, *sigaction, *oldact, sigsetsize)`.
///
/// A user handler must live in a user executable page and must name a restorer
/// in one; `SA_RESTORER` is required, and the accepted optional bits are
/// `SA_ONSTACK`, `SA_RESTART`, `SA_NODEFER`, `SA_RESETHAND` and `SA_SIGINFO`.
/// `SIG_DFL`/`SIG_IGN` are accepted with no restorer. SIGKILL and SIGSTOP are
/// `-EINVAL` (not catchable and not ignorable); the rest of 1..=31 are fine.
pub(crate) fn rt_sigaction(signum: usize, act: usize, oldact: usize, sigsetsize: usize) -> isize {
    if sigsetsize != 8 || signum == 0 || signum >= MAX_SIGNALS {
        return EINVAL;
    }
    if signum == SIGKILL || signum == SIGSTOP {
        return EINVAL;
    }
    let Some(run) = crate::user::current_run_id() else {
        return EINVAL;
    };

    let new_action = if act != 0 {
        let mut raw = [0u8; SIGACTION_SIZE];
        if let Err(error) = crate::user::syscall::copy_from_user(&mut raw, act) {
            return error;
        }
        let mut action = decode_action(&raw);
        action.mask &= VALID_MASK & !UNBLOCKABLE_MASK;
        if action.handler > SIG_IGN {
            if action.flags & !SA_SUPPORTED != 0
                || action.flags & SA_RESTORER == 0
                || action.restorer == 0
                || !user_executable(action.handler)
                || !user_executable(action.restorer)
            {
                return EINVAL;
            }
        } else {
            // The restorer and the mask mean nothing for a default/ignored
            // action; normalise them so `oldact` cannot report stale nonsense.
            action.flags = 0;
            action.mask = 0;
            action.restorer = 0;
        }
        Some(action)
    } else {
        None
    };

    // Read the old action and publish it *before* installing the new one: if the
    // user pointer is bad, the call changes nothing.
    let old = with_table(|table| action_of(table, run, signum));
    let Some(old) = old else {
        return EINVAL;
    };
    if oldact != 0 {
        let raw = encode_action(&old);
        if let Err(error) = crate::user::syscall::copy_to_user(oldact, &raw) {
            return error;
        }
    }
    if let Some(action) = new_action {
        let installed = with_table(|table| {
            let Some(index) = table.process_index(run) else {
                return false;
            };
            table.processes[index].actions[signum] = action;
            true
        });
        if !installed {
            return EINVAL;
        }
        SIGACTIONS.fetch_add(1, Ordering::Relaxed);
    }
    0
}

/// `sigaltstack(*stack, *oldstack)`.
///
/// `stack` has the 24-byte pagh layout (`ss_sp`, `ss_flags`, padding,
/// `ss_size`). A new stack must be at least [`MINSIGSTKSZ`] bytes and every page
/// of it must be mapped `U|R|W`; the range is validated in full, so a later
/// delivery cannot fault the kernel. `SS_DISABLE` removes the stack (the address
/// and size are ignored), any other flag is `-EINVAL`. `oldstack` reports
/// `SS_DISABLE` or `SS_ONSTACK` as POSIX does, and is written before the new
/// stack is installed, so a bad `oldstack` pointer changes nothing.
pub(crate) fn sigaltstack(stack: usize, oldstack: usize) -> isize {
    let Some(run) = crate::user::current_run_id() else {
        return EINVAL;
    };
    let tid = scheduler::current_id();

    // Decode and validate the new stack first, so no side effect can happen on
    // an error. A `SS_DISABLE` request carries no address to validate.
    let requested = if stack != 0 {
        let mut raw = [0u8; SIGALTSTACK_SIZE];
        if let Err(error) = crate::user::syscall::copy_from_user(&mut raw, stack) {
            return error;
        }
        let (sp, flags, size) = decode_altstack(&raw);
        if flags != 0 && flags != SS_DISABLE {
            return EINVAL;
        }
        if flags == SS_DISABLE {
            Some((0usize, 0usize, true))
        } else {
            if size < MINSIGSTKSZ {
                return EINVAL;
            }
            let Some(end) = sp.checked_add(size) else {
                return EINVAL;
            };
            if sp == 0 || end < sp {
                return EINVAL;
            }
            if !crate::user::syscall::user_range_valid(sp, end, true) {
                return crate::user::syscall::fault();
            }
            Some((sp, size, false))
        }
    } else {
        None
    };

    // Snapshot the current stack for `oldstack` (and for the `SS_ONSTACK`
    // report) before installing anything.
    let current = with_table(|table| {
        if table.process_index(run).is_none() {
            return None;
        }
        let index = table.ensure_thread(run, tid)?;
        let entry = &table.threads[index];
        Some((
            entry.alt_sp,
            entry.alt_size,
            entry.alt_disabled,
            entry.alt_active,
        ))
    });
    let Some((old_sp, old_size, old_disabled, old_active)) = current else {
        return EINVAL;
    };

    if oldstack != 0 {
        let flags = if old_active > 0 {
            // An executing handler on the alternate stack is the stronger
            // fact, even if the handler also disabled the stack for the next
            // delivery: POSIX reports SS_ONSTACK here.
            SS_ONSTACK
        } else if old_disabled {
            SS_DISABLE
        } else {
            0
        };
        let raw = encode_altstack(if old_disabled { 0 } else { old_sp }, flags, old_size);
        if let Err(error) = crate::user::syscall::copy_to_user(oldstack, &raw) {
            return error;
        }
    }

    if let Some((sp, size, disabled)) = requested {
        let stored = with_table(|table| {
            if table.process_index(run).is_none() {
                return false;
            }
            let Some(index) = table.ensure_thread(run, tid) else {
                return false;
            };
            let entry = &mut table.threads[index];
            entry.alt_sp = sp;
            entry.alt_size = size;
            entry.alt_disabled = disabled;
            // `alt_active` is deliberately untouched: a handler already on the
            // old stack keeps running on it, and the outer sigreturn pops the
            // depth. A new delivery switches only when depth is zero.
            true
        });
        if !stored {
            return EINVAL;
        }
        SIGALTSTACKS.fetch_add(1, Ordering::Relaxed);
    }
    0
}

/// `rt_sigprocmask(how, *set, *oldset, sigsetsize)`.
///
/// The blocked mask is per thread. `SIG_BLOCK`/`SIG_UNBLOCK`/`SIG_SETMASK` are
/// 0/1/2; a bit for SIGKILL or SIGSTOP is silently dropped, as no mask can make
/// those signals wait. `oldset` is filled before the new mask is installed, so a
/// bad pointer changes nothing.
pub(crate) fn rt_sigprocmask(how: usize, set: usize, oldset: usize, sigsetsize: usize) -> isize {
    if sigsetsize != 8 || how > 2 {
        return EINVAL;
    }
    let Some(run) = crate::user::current_run_id() else {
        return EINVAL;
    };
    let tid = scheduler::current_id();

    let requested = if set != 0 {
        let mut raw = [0u8; 8];
        if let Err(error) = crate::user::syscall::copy_from_user(&mut raw, set) {
            return error;
        }
        Some(u64::from_le_bytes(raw))
    } else {
        None
    };

    let current = with_table(|table| {
        if table.process_index(run).is_none() {
            return None;
        }
        let index = table.ensure_thread(run, tid)?;
        Some(table.threads[index].blocked)
    });
    let Some(current) = current else {
        return EINVAL;
    };

    if oldset != 0 {
        let raw = current.to_le_bytes();
        if let Err(error) = crate::user::syscall::copy_to_user(oldset, &raw) {
            return error;
        }
    }

    if let Some(set) = requested {
        let set = set & VALID_MASK & !UNBLOCKABLE_MASK;
        let next = match how {
            0 => (current | set) & VALID_MASK & !UNBLOCKABLE_MASK,
            1 => (current & !set) & VALID_MASK & !UNBLOCKABLE_MASK,
            _ => set,
        };
        let stored = with_table(|table| {
            if table.process_index(run).is_none() {
                return false;
            }
            let Some(index) = table.ensure_thread(run, tid) else {
                return false;
            };
            table.threads[index].blocked = next;
            true
        });
        if !stored {
            return EINVAL;
        }
        SIGPROCMASKS.fetch_add(1, Ordering::Relaxed);
    }
    0
}

/// Validate a `kill`/`tgkill` signal argument.
///
/// `sig == 0` is the existence probe (`Ok(None)`); 1..=31 are real signals. All
/// 31 are accepted: the job-control stops are real scheduler state now, and the
/// remaining unmodelled signals simply take their POSIX default action.
fn validate_signal(sig: isize) -> Result<Option<usize>, isize> {
    if sig < 0 || sig > 31 {
        return Err(EINVAL);
    }
    if sig == 0 {
        return Ok(None);
    }
    Ok(Some(sig as usize))
}

/// Turn a pid argument into the target thread ids of `run`.
fn resolve_targets(run: u64, pid: isize, out: &mut [u64]) -> Result<usize, isize> {
    if pid > 0 {
        // `getpid` names the process, not its main thread: resolve that id to
        // the main thread when it is still live, else to any live thread of the
        // group. A thread id keeps the older thread-targeted rule. Cross-process
        // ids are `-ESRCH` (there is no permission model here).
        let requested = pid as u64;
        let target = if requested == crate::user::process_id() {
            let main = crate::user::process::peek(requested, |proc| proc.main_tid)
                .filter(|tid| scheduler::thread_group(*tid) == Some(run));
            match main {
                Some(tid) => Some(tid),
                None => {
                    let ids = scheduler::group_threads(run, out);
                    (ids > 0).then_some(out[0])
                }
            }
        } else {
            (scheduler::thread_group(requested) == Some(run)).then_some(requested)
        };
        match target {
            Some(tid) => {
                out[0] = tid;
                Ok(1)
            }
            None => Err(ESRCH),
        }
    } else if pid == 0 || pid == -1 {
        let count = scheduler::group_threads(run, out);
        if count == 0 {
            Err(ESRCH)
        } else {
            Ok(count)
        }
    } else {
        // Any other negative pid would address another process group; this
        // kernel has no process groups, so the honest answer is "no such
        // process".
        Err(ESRCH)
    }
}

/// Mark `signal` pending for one thread with its sender metadata and report
/// whether that made it deliverable (`pending & !blocked`). The caller wakes the
/// thread outside the signal lock.
fn mark_pending(run: u64, tid: u64, signal: usize, source: Source) -> Result<bool, isize> {
    let bit = 1u64 << (signal - 1);
    with_table(|table| {
        if table.process_index(run).is_none() {
            return Err(ESRCH);
        }
        let Some(index) = table.ensure_thread(run, tid) else {
            return Err(ENOMEM);
        };
        let entry = &mut table.threads[index];
        entry.pending |= bit;
        entry.pending_pid[signal - 1] = source.pid();
        entry.pending_uid[signal - 1] = source.uid();
        entry.pending_code[signal - 1] = source.code();
        Ok(entry.pending & !entry.blocked & VALID_MASK != 0)
    })
}

/// Clear a pending job-control stop bit. Called when SIGCONT resumes a thread:
/// a stop that raced with the continue must not stop it again immediately.
fn clear_stop_pending(run: u64, tid: u64) -> bool {
    with_table(|table| {
        if table.process_index(run).is_none() {
            return false;
        }
        match table.thread_index(tid) {
            Some(index) => {
                table.threads[index].pending &= !JOB_STOP_MASK;
                true
            }
            None => false,
        }
    })
}

/// Deliver one signal to one thread's pending set. Returns 0 on success.
///
/// `wake` is false for SIGCONT: a blocked `futex` waiter must not be woken by a
/// continue (only a stopped thread's scheduler state changes), and a pending
/// SIGCONT reaches a running thread at its next boundary anyway.
fn send_to(run: u64, tid: u64, signal: usize, source: Source, wake: bool) -> isize {
    match mark_pending(run, tid, signal, source) {
        Ok(deliverable) => {
            if deliverable && wake {
                // A thread blocked in `futex` returns `-EINTR`; a ready/running
                // thread sees the signal at its next kernel-to-user boundary.
                let _ = scheduler::wake_task(tid);
            }
            0
        }
        Err(error) => error,
    }
}

/// The common body of `kill`/`tgkill`: resolve targets, then send with `code`.
fn send_signal(run: u64, pid: isize, signal: usize, code: i32) -> isize {
    let mut targets = [0u64; MAX_THREADS];
    let count = match resolve_targets(run, pid, &mut targets) {
        Ok(count) => count,
        Err(error) => return error,
    };
    let source = Source::User {
        pid: crate::user::process_id() as u32,
        uid: 0,
        code,
    };
    for tid in targets[..count].iter().copied() {
        let cont = signal == SIGCONT;
        let result = send_to(run, tid, signal, source, !cont);
        if result != 0 {
            return result;
        }
        if cont {
            // A continue is a scheduler operation even when SIGCONT is ignored
            // or blocked: POSIX's SIGCONT resumes a stopped process regardless.
            clear_stop_pending(run, tid);
            if scheduler::continue_task(tid) {
                CONTINUES.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    KILLS.fetch_add(1, Ordering::Relaxed);
    0
}

/// `kill(pid, sig)`.
///
/// `pid > 0` names one thread of the run (the ABI's `getpid`/`gettid` values);
/// `pid == 0` and `pid == -1` name every live thread of the run. `sig == 0` is a
/// pure existence check. Unknown ids are `-ESRCH`, not a silent success.
pub(crate) fn kill(pid: isize, sig: isize) -> isize {
    let Some(run) = crate::user::current_run_id() else {
        return ESRCH;
    };
    let signal = match validate_signal(sig) {
        Ok(Some(signal)) => signal,
        Ok(None) => {
            return match resolve_targets(run, pid, &mut [0u64; MAX_THREADS]) {
                Ok(_) => 0,
                Err(error) => error,
            }
        }
        Err(error) => return error,
    };
    send_signal(run, pid, signal, SI_USER)
}

/// `tgkill(tgid, tid, sig)`: send to one thread, but only when `tgid` is this
/// run's process id. The `siginfo` code is `SI_TKILL`, as POSIX specifies.
pub(crate) fn tgkill(tgid: usize, tid: usize, sig: isize) -> isize {
    let Some(run) = crate::user::current_run_id() else {
        return ESRCH;
    };
    let process = crate::user::process_id();
    if tid == 0 || tgid as u64 != process {
        return ESRCH;
    }
    match scheduler::thread_group(tid as u64) {
        Some(group) if group == run => {}
        _ => return ESRCH,
    }
    let signal = match validate_signal(sig) {
        Ok(Some(signal)) => signal,
        Ok(None) => return 0,
        Err(error) => return error,
    };
    send_signal(run, tid as isize, signal, SI_TKILL)
}

/// True when the *current* thread has at least one deliverable signal (pending
/// and not blocked). A blocking syscall uses this to answer `-EINTR`; the handler
/// itself runs at the trap boundary after the syscall returns.
///
/// If the lowest deliverable signal has `SA_RESTART`, this also arms the
/// current thread's restart flag so `dispatch` can leave the `ecall` in place
/// and the syscall re-enters after `rt_sigreturn`. Only `futex`/`nanosleep`
/// call this from a position where they answer `-EINTR`, which scopes the flag;
/// it lives in `ThreadSignal` so a preemption between arming and consuming it
/// cannot hand the request to another task.
pub(crate) fn interrupted() -> bool {
    // `nanosleep` calls this with `SIE` on, so the `RUN` lock must be taken with
    // interrupts masked too: a tick inside `current_run_id` would re-enter the
    // trap handler and the signal/RUN locks would deadlock one hart against
    // itself. The nested `without_interrupts` in `with_table` is a no-op then.
    csr::without_interrupts(|| {
        let Some(run) = crate::user::current_run_id() else {
            return false;
        };
        let tid = scheduler::current_id();
        let deliverable = with_table(|table| {
            if table.process_index(run).is_none() {
                return None;
            }
            let index = table.thread_index(tid)?;
            let pending = table.threads[index].pending;
            let blocked = table.threads[index].blocked;
            let deliverable = pending & !blocked & VALID_MASK;
            if deliverable == 0 {
                return None;
            }
            let signal = deliverable.trailing_zeros() as usize + 1;
            if action_of(table, run, signal)?.flags & SA_RESTART != 0 {
                table.threads[index].restart = true;
            }
            Some(())
        });
        match deliverable {
            Some(()) => {
                INTERRUPTS.fetch_add(1, Ordering::Relaxed);
                true
            }
            None => false,
        }
    })
}

/// Consume the current thread's restart request armed by [`interrupted`].
/// Called once per returning syscall by `dispatch`; a `true` answer means the
/// caller must not write `a0` or advance `sepc`, so `sret` re-executes the
/// `ecall` after the handler's `rt_sigreturn`.
pub(crate) fn take_restart() -> bool {
    csr::without_interrupts(|| {
        let tid = scheduler::current_id();
        let restart = with_table(|table| {
            let Some(index) = table.thread_index(tid) else {
                return false;
            };
            let restart = table.threads[index].restart;
            table.threads[index].restart = false;
            restart
        });
        if restart {
            RESTARTS.fetch_add(1, Ordering::Relaxed);
        }
        restart
    })
}

/// Fill the 128-byte `siginfo_t`.
///
/// `si_pid`/`si_uid` describe the sender for `kill`/`tgkill`; `si_addr` is the
/// faulting virtual address for a synchronous fault. `si_status` is meaningful
/// only for a child-exit `SIGCHLD`, and this kernel does not generate that
/// signal: `fork`/`exit`/`wait4` do not post `SIGCHLD` yet, and an explicitly
/// sent SIGCHLD is `SI_USER` with `si_status = 0`. That is a documented gap, not
/// a claim of waitable child notification.
fn make_siginfo(signal: usize, source: Source) -> [u8; SIGINFO_SIZE] {
    let mut info = [0u8; SIGINFO_SIZE];
    info[0..4].copy_from_slice(&(signal as i32).to_le_bytes());
    info[4..8].copy_from_slice(&0i32.to_le_bytes());
    info[8..12].copy_from_slice(&source.code().to_le_bytes());
    match source {
        Source::User { pid, uid, .. } => {
            info[16..20].copy_from_slice(&pid.to_le_bytes());
            info[20..24].copy_from_slice(&uid.to_le_bytes());
        }
        Source::Fault { address, .. } => {
            info[16..24].copy_from_slice(&(address as u64).to_le_bytes());
        }
    }
    // `si_status`, the `SIGCHLD` union member, is 0 without a child-exit source.
    info[24..28].copy_from_slice(&0i32.to_le_bytes());
    info
}

/// Build the `ucontext` snapshot from the interrupted trap frame.
fn make_ucontext(frame: &TrapFrame, mask: u64, link: u64) -> UContext {
    let mut gregs = [0u64; 32];
    gregs[0] = 0; // x0 is hard-wired zero, not the saved slot.
    gregs[1] = frame.ra as u64;
    gregs[2] = frame.sp as u64;
    gregs[3] = frame.gp as u64;
    gregs[4] = frame.tp as u64;
    gregs[5] = frame.t0 as u64;
    gregs[6] = frame.t1 as u64;
    gregs[7] = frame.t2 as u64;
    gregs[8] = frame.s0 as u64;
    gregs[9] = frame.s1 as u64;
    gregs[10] = frame.a0 as u64;
    gregs[11] = frame.a1 as u64;
    gregs[12] = frame.a2 as u64;
    gregs[13] = frame.a3 as u64;
    gregs[14] = frame.a4 as u64;
    gregs[15] = frame.a5 as u64;
    gregs[16] = frame.a6 as u64;
    gregs[17] = frame.a7 as u64;
    gregs[18] = frame.s2 as u64;
    gregs[19] = frame.s3 as u64;
    gregs[20] = frame.s4 as u64;
    gregs[21] = frame.s5 as u64;
    gregs[22] = frame.s6 as u64;
    gregs[23] = frame.s7 as u64;
    gregs[24] = frame.s8 as u64;
    gregs[25] = frame.s9 as u64;
    gregs[26] = frame.s10 as u64;
    gregs[27] = frame.s11 as u64;
    gregs[28] = frame.t3 as u64;
    gregs[29] = frame.t4 as u64;
    gregs[30] = frame.t5 as u64;
    gregs[31] = frame.t6 as u64;
    UContext {
        flags: 0,
        link,
        sigmask: mask,
        mcontext: MContext {
            gregs,
            pc: frame.sepc as u64,
        },
    }
}

/// Install `signal` on the current thread by building the user frame and
/// rewriting the trap frame to enter `handler` with `ra = restorer`.
///
/// A bad stack is a run-level signal death: the kernel cannot deliver a frame it
/// cannot write, and dying is the only answer that cannot loop. The pending bit
/// has already been consumed by the caller.
fn arm(frame: &mut TrapFrame, run: u64, signal: usize, action: Action, source: Source) {
    let tid = scheduler::current_id();
    let state = with_table(|table| {
        if table.process_index(run).is_none() {
            return None;
        }
        let index = table.ensure_thread(run, tid)?;
        let entry = &table.threads[index];
        Some((
            entry.frame,
            entry.blocked,
            entry.alt_sp,
            entry.alt_size,
            entry.alt_disabled,
            entry.alt_active,
        ))
    });
    let Some((prev_frame, blocked, alt_sp, alt_size, alt_disabled, alt_active)) = state else {
        crate::user::on_signal_death(signal, frame.sepc, "signal state lost");
    };

    // The mask in force inside the handler. `SA_NODEFER` omits the delivered
    // signal itself; SIGKILL/SIGSTOP can never enter a mask.
    let mut next_blocked = (blocked | action.mask) & VALID_MASK & !UNBLOCKABLE_MASK;
    if action.flags & SA_NODEFER == 0 {
        next_blocked = (next_blocked | (1u64 << (signal - 1))) & VALID_MASK & !UNBLOCKABLE_MASK;
    }

    // Pick the stack: SA_ONSTACK with an installed stack and no handler already
    // on it; otherwise the interrupted stack (which, for a nested delivery, is
    // already the alternate stack). POSIX falls back rather than failing.
    let use_alt = action.flags & SA_ONSTACK != 0 && !alt_disabled && alt_active == 0;
    let used_alt;
    let frame_base;
    if use_alt {
        let Some(top) = alt_sp.checked_add(alt_size) else {
            crate::user::on_signal_death(signal, frame.sepc, "bad alternate signal stack");
        };
        let top = top & !0xf;
        let Some(base) = top.checked_sub(SIGFRAME_SIZE) else {
            crate::user::on_signal_death(signal, frame.sepc, "bad alternate signal stack");
        };
        let Some(end) = base.checked_add(SIGFRAME_SIZE) else {
            crate::user::on_signal_death(signal, frame.sepc, "bad alternate signal stack");
        };
        if base < alt_sp || !crate::user::syscall::user_range_valid(base, end, true) {
            crate::user::on_signal_death(signal, frame.sepc, "bad alternate signal stack");
        }
        frame_base = base;
        used_alt = true;
    } else {
        let aligned = frame.sp & !0xf;
        let Some(base) = aligned.checked_sub(SIGFRAME_SIZE) else {
            crate::user::on_signal_death(signal, frame.sepc, "bad signal stack");
        };
        let Some(end) = base.checked_add(SIGFRAME_SIZE) else {
            crate::user::on_signal_death(signal, frame.sepc, "bad signal stack");
        };
        if !crate::user::syscall::user_range_valid(base, end, true) {
            crate::user::on_signal_death(signal, frame.sepc, "bad signal stack");
        }
        frame_base = base;
        used_alt = false;
    }

    // `prev_frame` is kernel-written state, but `rt_sigreturn` is its only
    // user-reachable writer and installs only a frame [`valid_saved_frame`]
    // accepts. Re-check anyway, because the chain link is informational: an
    // impossible value starts a fresh chain instead of overflowing the
    // addition below (F-r3-rv-1).
    let prev_frame = if valid_saved_frame(prev_frame) {
        prev_frame
    } else {
        0
    };
    let link = if prev_frame == 0 {
        0
    } else {
        match prev_frame.checked_add(core::mem::offset_of!(SignalFrame, ucontext)) {
            Some(link) => link as u64,
            None => 0,
        }
    };
    let siginfo = make_siginfo(signal, source);
    let signal_frame = SignalFrame {
        // SAFETY: `TrapFrame` is `#[repr(C)]` and contains only `usize` fields,
        // so a bitwise copy is a valid duplicate of the interrupted context; the
        // original stays in `frame` until it is rewritten below.
        saved: unsafe { core::ptr::read(frame) },
        mask: blocked,
        magic: SIGFRAME_MAGIC,
        restorer: action.restorer as u64,
        siginfo,
        prev_frame: prev_frame as u64,
        signum: signal as u64,
        ucontext: make_ucontext(frame, next_blocked, link),
        flags: if used_alt { SIGFRAME_USED_ALTSTACK } else { 0 },
        reserved: 0,
    };
    // SAFETY: every field of `SignalFrame` is initialised above, the struct has
    // no padding beyond the byte arrays and 8-byte scalars (`repr(C)`), and the
    // slice borrows a live local for the duration of the copy.
    let bytes = unsafe {
        core::slice::from_raw_parts(
            (&signal_frame as *const SignalFrame).cast::<u8>(),
            SIGFRAME_SIZE,
        )
    };
    if crate::user::syscall::copy_to_user(frame_base, bytes).is_err() {
        let reason = if used_alt {
            "bad alternate signal stack"
        } else {
            "bad signal stack"
        };
        crate::user::on_signal_death(signal, frame.sepc, reason);
    }

    let armed = with_table(|table| {
        if table.process_index(run).is_none() {
            return false;
        }
        let Some(index) = table.ensure_thread(run, tid) else {
            return false;
        };
        let entry = &mut table.threads[index];
        entry.blocked = next_blocked;
        entry.frame = frame_base;
        if used_alt {
            entry.alt_active = entry.alt_active.saturating_add(1);
        }
        if action.flags & SA_RESETHAND != 0 {
            if let Some(process) = table.process_index(run) {
                table.processes[process].actions[signal] = Action::default();
            }
        }
        true
    });
    if !armed {
        crate::user::on_signal_death(signal, frame.sepc, "signal state lost");
    }

    // The handler is entered exactly as a function call from the interrupted
    // instruction would be: `ra` points at the user restorer, `sp` at the frame,
    // `a0` is the signal number, `a1` the siginfo inside that frame and, with
    // SA_SIGINFO, `a2` the ucontext after it.
    frame.ra = action.restorer;
    frame.sp = frame_base;
    frame.sepc = action.handler;
    frame.a0 = signal;
    frame.a1 = frame_base + core::mem::offset_of!(SignalFrame, siginfo);
    frame.a2 = if action.flags & SA_SIGINFO != 0 {
        frame_base + core::mem::offset_of!(SignalFrame, ucontext)
    } else {
        0
    };
    DELIVERED.fetch_add(1, Ordering::Relaxed);
}

/// Apply the default action of a signal with no handler.
///
/// The job-control stops park the *calling* thread in the scheduler: delivery
/// happens at a kernel-to-user boundary in the target's own context, so
/// `stop_current` is exactly the thread the signal names. It returns only after
/// another thread's SIGCONT makes this thread runnable again, and then the trap
/// returns to the interrupted user instruction.
fn default_action(frame: &TrapFrame, signal: usize) {
    if default_stops(signal) {
        STOPS.fetch_add(1, Ordering::Relaxed);
        scheduler::stop_current();
        return;
    }
    if default_ignored(signal) {
        IGNORED.fetch_add(1, Ordering::Relaxed);
        return;
    }
    TERMINATED.fetch_add(1, Ordering::Relaxed);
    crate::user::on_signal_death(signal, frame.sepc, signal_name(signal));
}

/// One delivery attempt for the current thread.
fn deliver_one(frame: &mut TrapFrame, run: u64, signal: usize, source: Source) -> bool {
    let action = with_table(|table| action_of(table, run, signal));
    let Some(action) = action else {
        return false;
    };
    if action.handler == SIG_DFL {
        default_action(frame, signal);
        return true;
    }
    if action.handler == SIG_IGN {
        IGNORED.fetch_add(1, Ordering::Relaxed);
        return true;
    }
    arm(frame, run, signal, action, source);
    true
}

/// Deliver at most one pending, unblocked signal to the current thread.
///
/// Called only on the kernel-to-user boundary: the end of a syscall and the end
/// of every trap taken from U-mode. One signal per boundary keeps a burst of
/// pending signals from nesting frames inside one trap; the next boundary (or
/// the handler's own `rt_sigreturn`) delivers the next.
pub(crate) fn deliver_pending(frame: &mut TrapFrame) {
    if frame.sstatus & csr::SSTATUS_SPP != 0 {
        return;
    }
    let Some(run) = crate::user::current_run_id() else {
        return;
    };
    let tid = scheduler::current_id();
    let item = with_table(|table| {
        if table.process_index(run).is_none() {
            return None;
        }
        let index = table.ensure_thread(run, tid)?;
        let entry = &mut table.threads[index];
        let deliverable = entry.pending & !entry.blocked & VALID_MASK;
        if deliverable == 0 {
            return None;
        }
        let signal = deliverable.trailing_zeros() as usize + 1;
        let source = Source::User {
            pid: entry.pending_pid[signal - 1],
            uid: entry.pending_uid[signal - 1],
            code: entry.pending_code[signal - 1],
        };
        // Acknowledge before touching user memory: a frame that cannot be
        // built kills the run, and a re-delivery would loop.
        entry.pending &= !(1u64 << (signal - 1));
        Some((signal, source))
    });
    let Some((signal, source)) = item else {
        return;
    };
    let _ = deliver_one(frame, run, signal, source);
}

/// Deliver SIGSEGV/SIGILL/SIGBUS/SIGTRAP for a synchronous U-mode fault when the
/// program installed a real handler; return `true` when the trap frame now
/// enters it.
///
/// With no handler (or `SIG_IGN`, which cannot make a faulting instruction
/// succeed) the caller keeps the pre-signal behaviour: `user::on_fault` kills the
/// run with the existing diagnostic. A blocked synchronous signal is still
/// delivered — POSIX leaves that behaviour undefined and a fault must not become
/// an unresolvable pending bit.
pub(crate) fn deliver_fault(
    frame: &mut TrapFrame,
    scause: usize,
    _sepc: usize,
    stval: usize,
) -> bool {
    use crate::arch::riscv64::traps::{
        CAUSE_BREAKPOINT, CAUSE_ILLEGAL_INSTRUCTION, CAUSE_INSTRUCTION_PAGE_FAULT,
        CAUSE_LOAD_ADDRESS_MISALIGNED, CAUSE_LOAD_PAGE_FAULT, CAUSE_STORE_ADDRESS_MISALIGNED,
        CAUSE_STORE_PAGE_FAULT,
    };
    let (signal, code) = match scause {
        CAUSE_ILLEGAL_INSTRUCTION => (4, SI_KERNEL), // SIGILL
        CAUSE_BREAKPOINT => (5, TRAP_BRKPT),         // SIGTRAP
        CAUSE_LOAD_ADDRESS_MISALIGNED | CAUSE_STORE_ADDRESS_MISALIGNED => (7, BUS_ADRALN), // SIGBUS
        CAUSE_INSTRUCTION_PAGE_FAULT | CAUSE_LOAD_PAGE_FAULT | CAUSE_STORE_PAGE_FAULT => {
            (11, SEGV_MAPERR) // SIGSEGV
        }
        _ => return false,
    };
    let Some(run) = crate::user::current_run_id() else {
        return false;
    };
    let action = with_table(|table| action_of(table, run, signal));
    let Some(action) = action else {
        return false;
    };
    if action.handler <= SIG_IGN {
        return false;
    }
    FAULT_SIGNALS.fetch_add(1, Ordering::Relaxed);
    arm(
        frame,
        run,
        signal,
        action,
        Source::Fault {
            address: stval,
            code,
        },
    );
    true
}

/// `rt_sigreturn`: restore the trap frame saved when the kernel delivered the
/// signal whose handler is returning.
///
/// `sp` must point at the frame the kernel armed for this thread (the restorer
/// runs with exactly that `sp`). The pointer range is validated `U|R` first, so
/// a bad frame is `-EFAULT`; an absent, mismatched or unknown-flag frame is
/// `-EINVAL`. On success the current trap frame is replaced wholesale and
/// `dispatch` returns without bumping `sepc`, so `sret` resumes the interrupted
/// instruction with its original `sp`, registers and blocked mask, and pops one
/// alternate-stack level if the frame recorded that it switched.
pub(crate) fn sigreturn(frame: &mut TrapFrame) -> Result<(), isize> {
    let Some(run) = crate::user::current_run_id() else {
        return Err(EINVAL);
    };
    let tid = scheduler::current_id();
    let sp = frame.sp;
    if sp == 0 || sp % 16 != 0 {
        return Err(crate::user::syscall::fault());
    }
    let Some(end) = sp.checked_add(SIGFRAME_SIZE) else {
        return Err(crate::user::syscall::fault());
    };
    if !crate::user::syscall::user_range_valid(sp, end, false) {
        return Err(crate::user::syscall::fault());
    }

    let active = with_table(|table| {
        if table.process_index(run).is_none() {
            return None;
        }
        let index = table.thread_index(tid)?;
        Some(table.threads[index].frame)
    });
    let Some(active) = active else {
        return Err(EINVAL);
    };
    if active == 0 || sp != active {
        return Err(EINVAL);
    }

    let mut raw = [0u8; SIGFRAME_SIZE];
    crate::user::syscall::copy_from_user(&mut raw, sp)?;
    // SAFETY: `raw` is a fully initialised byte array of exactly
    // `SIGFRAME_SIZE`; `SignalFrame` is `repr(C)`, and `read_unaligned` imposes
    // no alignment requirement on the source. Every bit pattern is a valid
    // `TrapFrame` (all `usize` fields), and the magic/signum/flags checks below
    // reject a frame this kernel did not write.
    let signal = unsafe { core::ptr::read_unaligned(raw.as_ptr().cast::<SignalFrame>()) };
    if signal.magic != SIGFRAME_MAGIC
        || signal.signum == 0
        || signal.signum >= MAX_SIGNALS as u64
        || signal.flags & !SIGFRAME_KNOWN_FLAGS != 0
    {
        return Err(EINVAL);
    }

    let restored_mask = signal.mask & VALID_MASK & !UNBLOCKABLE_MASK;
    let prev_frame = signal.prev_frame as usize;
    // The one user-writable pointer in the frame: a handler can rewrite it
    // before `rt_sigreturn`, and the next delivery adds an offset to it, so an
    // unchecked value was a debug-build overflow panic (F-r3-rv-1). Accept only
    // "no previous frame" or a complete frame the caller can still read;
    // anything else leaves the thread's state untouched and answers `-EINVAL`.
    if !valid_saved_frame(prev_frame) {
        return Err(EINVAL);
    }
    let used_alt = signal.flags & SIGFRAME_USED_ALTSTACK != 0;
    let restored = signal.saved;

    let restored_ok = with_table(|table| {
        if table.process_index(run).is_none() {
            return false;
        }
        let Some(index) = table.thread_index(tid) else {
            return false;
        };
        let entry = &mut table.threads[index];
        if entry.frame != sp {
            return false;
        }
        entry.blocked = restored_mask;
        entry.frame = prev_frame;
        if used_alt {
            entry.alt_active = entry.alt_active.saturating_sub(1);
        }
        true
    });
    if !restored_ok {
        return Err(EINVAL);
    }

    // `sstatus` is rebuilt, never copied from user memory: SPP=0 makes the
    // epilogue take the U-mode path, SIE=0 and SUM=0 are the kernel's normal
    // state, and SPIE=1 re-enables interrupts at `sret`. A forged frame cannot
    // request S-mode execution or clear `SUM` behind the copy helpers' backs.
    let mut restored = restored;
    restored.sstatus = csr::user_entry_status();
    *frame = restored;
    SIGRETURNS.fetch_add(1, Ordering::Relaxed);
    Ok(())
}
