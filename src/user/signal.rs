//! POSIX-style signals for the native user ABI.
//!
//! Scope: the five syscalls the ABI exposes (`kill`, `tgkill`, `rt_sigaction`,
//! `rt_sigprocmask`, `rt_sigreturn`), per-run handlers, per-thread blocked masks
//! and pending sets, a user-memory signal frame, and synchronous delivery for
//! the faults that have a POSIX name (`SIGSEGV`, `SIGILL`, `SIGTRAP`).
//!
//! # What a signal is here
//!
//! One run is one process: handlers and the signal frame format are per run,
//! while the pending mask and the blocked mask are per thread (a `clone` child
//! has its own). `kill` with a thread id in the run targets that thread;
//! `kill(0)` and `kill(-1)` target every live thread of the run. Delivery is
//! asynchronous from the sender's point of view: the target marks the signal
//! pending, and the *target's* next kernel-to-user boundary installs the
//! handler. That boundary is the end of every syscall and the end of every
//! trap taken from U-mode, including the timer tick, so a running thread is
//! interrupted within one tick at worst. A thread blocked in `futex` is woken
//! by the sender (`scheduler::wake_task`), so its wait returns `-EINTR` and the
//! handler runs at the boundary right after it.
//!
//! # Signal frame
//!
//! The frame is user memory below the target's current `sp`, 16-byte aligned:
//!
//! ```text
//! offset   0  saved TrapFrame (264 B, the frame `trap_entry` built)
//! offset 264  blocked mask to restore (8 B)
//! offset 272  magic (8 B)
//! offset 280  restorer address (8 B)
//! offset 288  siginfo (128 B: signo, errno, code, then si_pid/si_uid or si_addr)
//! offset 416  previous frame pointer (8 B, 0 for the outermost handler)
//! offset 424  signum (8 B)
//! total     432 B (16-byte aligned)
//! ```
//!
//! Every page of `[frame, frame + 432)` is validated `U|R|W` before the copy, so
//! a bad stack is a signal death (and never a kernel fault). `rt_sigreturn`
//! validates the same range for reads, requires `sp` to equal the frame the
//! kernel last armed for this thread, checks the magic, and then restores the
//! saved frame with a sanitized `sstatus` (`SPP=0`, `SIE=0`, `SUM=0`,
//! `SPIE=1`). The user cannot turn a forged frame into S-mode execution.
//!
//! # Default actions
//!
//! `SIG_DFL` terminates the *run* for the POSIX-fatal signals (SIGTERM, SIGINT,
//! SIGQUIT, SIGABRT, SIGSEGV, SIGILL, SIGFPE, SIGBUS, SIGKILL and the rest of
//! the core family); SIGCHLD, SIGCONT, SIGURG and SIGWINCH are ignored. This
//! run is the process, so a fatal default action retires every thread, exactly
//! like `exit_group`/a fault; the runner does the one teardown of shared state.
//!
//! # Deliberate limits (documented, not faked)
//!
//! * No job control: SIGSTOP/SIGTSTP/SIGTTIN/SIGTTOU are refused by `kill` with
//!   `-ENOSYS`, and SIGKILL/SIGSTOP cannot be installed (`rt_sigaction` is
//!   `-EINVAL`, as POSIX requires for the two uncatchable signals).
//! * No alternate signal stack (`sigaltstack` is not a syscall here).
//! * No `SA_RESTART`, `SA_NODEFER`, `SA_RESETHAND` or `SA_SIGINFO` semantics:
//!   `rt_sigaction` accepts `0` or `SA_RESTORER`, and a user handler always gets
//!   `(signum, siginfo)` and runs with its own signal blocked. The restorer is
//!   required, because the kernel has no vDSO trampoline to return through.
//! * Blocking a syscall is interrupted (`-EINTR`) for `futex` and `nanosleep`;
//!   `read(0)` is non-blocking by contract and returns `-EAGAIN` as before.
//! * `siginfo` is the minimal 128-byte record above; `si_pid`/`si_uid` for a
//!   `kill` and `si_addr` for a fault are filled, nothing else.
//!
//! # Lock order
//!
//! `signal table -> (nothing)`. The table lock is never held across a user copy,
//! a VFS call or `scheduler::wake_task`; `RUN` may be taken before the signal
//! lock (the run id is read first), and the scheduler lock is only taken inside
//! `thread_group`/`group_threads`/`wake_task` with the signal lock released.
//! Every table access masks interrupts, because `interrupted()` is called from
//! `nanosleep` with `SIE` on.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::arch::riscv64::csr;
use crate::arch::riscv64::mmu::{PTE_U, PTE_X};
use crate::arch::riscv64::traps::TrapFrame;
use crate::memory::{paging, FRAME_SIZE};
use crate::sync::Spinlock;
use crate::task::scheduler;

/// Signal numbers the ABI can name. Index 0 is never a valid signal.
pub(crate) const MAX_SIGNALS: usize = 32;

/// Valid bits of a signal mask: signal `n` is bit `n - 1`, so signals 1..=31.
pub(crate) const VALID_MASK: u64 = (1 << 31) - 1;

/// SIGKILL and SIGSTOP cannot be blocked or caught (POSIX).
const UNBLOCKABLE_MASK: u64 = (1 << (9 - 1)) | (1 << (19 - 1));

const SIGKILL: usize = 9;
const SIGSTOP: usize = 19;

/// `struct sigaction` flags bit that says `sa_restorer` is meaningful.
pub(crate) const SA_RESTORER: usize = 0x0400_0000;
/// `SIG_DFL`: the default action.
pub(crate) const SIG_DFL: usize = 0;
/// `SIG_IGN`: discard the signal.
pub(crate) const SIG_IGN: usize = 1;

/// `struct sigaction` in pagh's ABI: handler, flags, mask, restorer.
///
/// This is deliberately pagh's order, not Linux's `handler/flags/restorer/mask`;
/// the native ABI document owns the layout and the fixture uses it directly.
const SIGACTION_SIZE: usize = 32;

/// Maximum live threads a run can have (the scheduler's table minus the shell
/// and idle slots). A thread-signal entry is needed only while a thread uses
/// signals, so this is an upper bound, not a reservation.
const MAX_THREADS: usize = scheduler::MAX_TASKS;

/// Bytes of the signal frame written below the interrupted `sp`.
const SIGFRAME_SIZE: usize = 432;
const SIGFRAME_MAGIC: u64 = 0x7061_6768_7369_6700;

/// Bytes of the minimal `siginfo_t` delivered in `a1`.
const SIGINFO_SIZE: usize = 128;
/// `SI_USER`: the signal came from `kill`/`tgkill`.
const SI_USER: i32 = 0;
/// `SEGV_MAPERR`: the synchronous fault has no matching object (our map has no
/// per-object fault information, so this is the honest code).
const SEGV_MAPERR: i32 = 1;

const ESRCH: isize = -3;
const EINVAL: isize = -22;
const ENOMEM: isize = -12;
const ENOSYS: isize = -38;

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
        }
    }

    fn new(run: u64, tid: u64) -> Self {
        ThreadSignal {
            used: true,
            run,
            tid,
            pending: 0,
            blocked: 0,
            frame: 0,
        }
    }
}

/// The per-run handlers and the per-thread pending/blocked state.
struct SignalTable {
    /// Run these handlers belong to; 0 when no program has installed any.
    run: u64,
    actions: [Action; MAX_SIGNALS],
    threads: [ThreadSignal; MAX_THREADS],
}

impl SignalTable {
    const fn empty() -> Self {
        SignalTable {
            run: 0,
            actions: [Action::default(); MAX_SIGNALS],
            threads: [ThreadSignal::empty(); MAX_THREADS],
        }
    }

    fn reset(&mut self, run: u64) {
        self.run = run;
        self.actions = [Action::default(); MAX_SIGNALS];
        self.threads = [ThreadSignal::empty(); MAX_THREADS];
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
        self.actions
            .iter()
            .skip(1)
            .filter(|action| action.handler != SIG_DFL)
            .count()
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
/// Signals that interrupted a `futex` wait (the `-EINTR` path).
static INTERRUPTS: AtomicU64 = AtomicU64::new(0);

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

/// Reset the per-run handler table and every thread entry. Called by
/// `run_source` before the first task can issue an `ecall`.
pub(crate) fn begin_run(run: u64) {
    csr::without_interrupts(|| SIGNALS.lock().reset(run));
}

/// Forget everything a finished run installed. Called by the runner after the
/// last thread is gone and before the next run can start.
pub(crate) fn clear_run(run: u64) {
    csr::without_interrupts(|| {
        let mut table = SIGNALS.lock();
        if table.run == run {
            table.reset(0);
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

/// Why a signal is being delivered; decides the `siginfo` payload.
#[derive(Clone, Copy)]
enum Source {
    /// `kill`/`tgkill`, `SI_USER`.
    User { pid: u64 },
    /// A synchronous U-mode fault, with the faulting address as `si_addr`.
    Fault { address: usize },
}

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
}

const _: () = assert!(core::mem::size_of::<SignalFrame>() == SIGFRAME_SIZE);

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

/// `rt_sigaction(signum, *sigaction, *oldact, sigsetsize)`.
///
/// A user handler must live in a user executable page and must name a restorer
/// in one; `SA_RESTORER` is the only accepted flag bit. `SIG_DFL`/`SIG_IGN` are
/// accepted with no restorer. SIGKILL and SIGSTOP are `-EINVAL` (not catchable
/// and not ignorable); the rest of 1..=31 are fine.
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
            if action.flags & !SA_RESTORER != 0
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
    let old = with_table(|table| {
        if table.run != run {
            None
        } else {
            Some(table.actions[signum])
        }
    });
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
            if table.run != run {
                return false;
            }
            table.actions[signum] = action;
            true
        });
        if !installed {
            return EINVAL;
        }
        SIGACTIONS.fetch_add(1, Ordering::Relaxed);
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
        if table.run != run {
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
            if table.run != run {
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
/// `sig == 0` is the existence probe (`Ok(None)`); 1..=31 are real signals. The
/// job-control stops are `-ENOSYS`: this kernel has no stopped state, so
/// pretending to stop a thread would be a lie.
fn validate_signal(sig: isize) -> Result<Option<usize>, isize> {
    if sig < 0 || sig > 31 {
        return Err(EINVAL);
    }
    if sig == 0 {
        return Ok(None);
    }
    let signal = sig as usize;
    if matches!(signal, SIGSTOP..=22) {
        return Err(ENOSYS);
    }
    Ok(Some(signal))
}

/// Turn a pid argument into the target thread ids of `run`.
fn resolve_targets(run: u64, pid: isize, out: &mut [u64]) -> Result<usize, isize> {
    if pid > 0 {
        match scheduler::thread_group(pid as u64) {
            Some(group) if group == run => {
                out[0] = pid as u64;
                Ok(1)
            }
            _ => Err(ESRCH),
        }
    } else if pid == 0 || pid == -1 {
        let count = scheduler::group_threads(run, out);
        if count == 0 {
            Err(ESRCH)
        } else {
            Ok(count)
        }
    } else {
        // Any other negative pid would address another process group; there is
        // exactly one run, so the honest answer is "no such process".
        Err(ESRCH)
    }
}

/// Mark `signal` pending for one thread and report whether that made it
/// deliverable (`pending & !blocked`). The caller wakes the thread outside the
/// signal lock.
fn mark_pending(run: u64, tid: u64, signal: usize) -> Result<bool, isize> {
    let bit = 1u64 << (signal - 1);
    with_table(|table| {
        if table.run != run {
            return Err(ESRCH);
        }
        let Some(index) = table.ensure_thread(run, tid) else {
            return Err(ENOMEM);
        };
        let entry = &mut table.threads[index];
        entry.pending |= bit;
        Ok(entry.pending & !entry.blocked & VALID_MASK != 0)
    })
}

/// Deliver one signal to one thread's pending set. Returns 0 on success.
fn send_to(run: u64, tid: u64, signal: usize) -> isize {
    match mark_pending(run, tid, signal) {
        Ok(deliverable) => {
            if deliverable {
                // A thread blocked in `futex` returns `-EINTR`; a ready/running
                // thread sees the signal at its next kernel-to-user boundary.
                let _ = scheduler::wake_task(tid);
            }
            0
        }
        Err(error) => error,
    }
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
        Ok(signal) => signal,
        Err(error) => return error,
    };
    let mut targets = [0u64; MAX_THREADS];
    let count = match resolve_targets(run, pid, &mut targets) {
        Ok(count) => count,
        Err(error) => return error,
    };
    if let Some(signal) = signal {
        for tid in targets[..count].iter().copied() {
            let result = send_to(run, tid, signal);
            if result != 0 {
                return result;
            }
        }
        KILLS.fetch_add(1, Ordering::Relaxed);
    }
    0
}

/// `tgkill(tgid, tid, sig)`: send to one thread, but only when `tgid` is this
/// run's process id.
pub(crate) fn tgkill(tgid: usize, tid: usize, sig: isize) -> isize {
    let Some(run) = crate::user::current_run_id() else {
        return ESRCH;
    };
    let process = crate::user::process_id();
    if tid == 0 || tgid as u64 != process {
        return ESRCH;
    }
    if let Some(group) = scheduler::thread_group(tid as u64) {
        if group != run {
            return ESRCH;
        }
    } else {
        return ESRCH;
    }
    kill(tid as isize, sig)
}

/// True when the *current* thread has at least one deliverable signal (pending
/// and not blocked). A blocking syscall uses this to answer `-EINTR`; the handler
/// itself runs at the trap boundary after the syscall returns.
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
            if table.run != run {
                return false;
            }
            let Some(index) = table.thread_index(tid) else {
                return false;
            };
            let entry = &table.threads[index];
            entry.pending & !entry.blocked & VALID_MASK != 0
        });
        if deliverable {
            INTERRUPTS.fetch_add(1, Ordering::Relaxed);
        }
        deliverable
    })
}

/// Fill the 128-byte minimal `siginfo_t`.
fn make_siginfo(signal: usize, source: Source) -> [u8; SIGINFO_SIZE] {
    let mut info = [0u8; SIGINFO_SIZE];
    info[0..4].copy_from_slice(&(signal as i32).to_le_bytes());
    info[4..8].copy_from_slice(&0i32.to_le_bytes());
    match source {
        Source::User { pid } => {
            info[8..12].copy_from_slice(&SI_USER.to_le_bytes());
            info[16..20].copy_from_slice(&(pid as u32).to_le_bytes());
            info[20..24].copy_from_slice(&0u32.to_le_bytes());
        }
        Source::Fault { address } => {
            info[8..12].copy_from_slice(&SEGV_MAPERR.to_le_bytes());
            info[16..24].copy_from_slice(&(address as u64).to_le_bytes());
        }
    }
    info
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
        if table.run != run {
            return None;
        }
        let index = table.ensure_thread(run, tid)?;
        let entry = &mut table.threads[index];
        Some((entry.frame, entry.blocked))
    });
    let Some((prev_frame, blocked)) = state else {
        crate::user::on_signal_death(signal, frame.sepc, "signal state lost");
    };

    let aligned = frame.sp & !0xf;
    let Some(frame_base) = aligned.checked_sub(SIGFRAME_SIZE) else {
        crate::user::on_signal_death(signal, frame.sepc, "bad signal stack");
    };
    let Some(frame_end) = frame_base.checked_add(SIGFRAME_SIZE) else {
        crate::user::on_signal_death(signal, frame.sepc, "bad signal stack");
    };
    if !crate::user::syscall::user_range_valid(frame_base, frame_end, true) {
        crate::user::on_signal_death(signal, frame.sepc, "bad signal stack");
    }

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
    };
    // SAFETY: every field of `SignalFrame` is initialised above, the struct has
    // no padding (all fields are 8-byte scalars or a byte array, `repr(C)`), and
    // the slice borrows a live local for the duration of the copy.
    let bytes = unsafe {
        core::slice::from_raw_parts(
            (&signal_frame as *const SignalFrame).cast::<u8>(),
            SIGFRAME_SIZE,
        )
    };
    if crate::user::syscall::copy_to_user(frame_base, bytes).is_err() {
        crate::user::on_signal_death(signal, frame.sepc, "bad signal stack");
    }

    let armed = with_table(|table| {
        if table.run != run {
            return false;
        }
        let Some(index) = table.ensure_thread(run, tid) else {
            return false;
        };
        let entry = &mut table.threads[index];
        entry.blocked =
            (entry.blocked | action.mask | (1u64 << (signal - 1))) & VALID_MASK & !UNBLOCKABLE_MASK;
        entry.frame = frame_base;
        true
    });
    if !armed {
        crate::user::on_signal_death(signal, frame.sepc, "signal state lost");
    }

    // The handler is entered exactly as a function call from the interrupted
    // instruction would be: `ra` points at the user restorer, `sp` at the frame,
    // `a0` is the signal number and `a1` the siginfo inside that frame.
    frame.ra = action.restorer;
    frame.sp = frame_base;
    frame.sepc = action.handler;
    frame.a0 = signal;
    frame.a1 = frame_base + core::mem::offset_of!(SignalFrame, siginfo);
    DELIVERED.fetch_add(1, Ordering::Relaxed);
}

/// Apply the default action of a signal with no handler.
fn default_action(frame: &TrapFrame, signal: usize) {
    if default_ignored(signal) {
        IGNORED.fetch_add(1, Ordering::Relaxed);
        return;
    }
    TERMINATED.fetch_add(1, Ordering::Relaxed);
    crate::user::on_signal_death(signal, frame.sepc, signal_name(signal));
}

/// One delivery attempt for the current thread.
fn deliver_one(frame: &mut TrapFrame, run: u64, signal: usize, source: Source) -> bool {
    let action = with_table(|table| {
        if table.run != run {
            None
        } else {
            Some(table.actions[signal])
        }
    });
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
    let signal = with_table(|table| {
        if table.run != run {
            return None;
        }
        let index = table.ensure_thread(run, tid)?;
        let entry = &mut table.threads[index];
        let deliverable = entry.pending & !entry.blocked & VALID_MASK;
        if deliverable == 0 {
            return None;
        }
        let signal = deliverable.trailing_zeros() as usize + 1;
        // Acknowledge before touching user memory: a frame that cannot be
        // built kills the run, and a re-delivery would loop.
        entry.pending &= !(1u64 << (signal - 1));
        Some(signal)
    });
    let Some(signal) = signal else {
        return;
    };
    let _ = deliver_one(
        frame,
        run,
        signal,
        Source::User {
            pid: crate::user::process_id(),
        },
    );
}

/// Deliver SIGSEGV/SIGILL/SIGTRAP for a synchronous U-mode fault when the program
/// installed a real handler; return `true` when the trap frame now enters it.
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
    let signal = match scause {
        crate::arch::riscv64::traps::CAUSE_ILLEGAL_INSTRUCTION => 4, // SIGILL
        crate::arch::riscv64::traps::CAUSE_BREAKPOINT => 5,          // SIGTRAP
        crate::arch::riscv64::traps::CAUSE_INSTRUCTION_PAGE_FAULT
        | crate::arch::riscv64::traps::CAUSE_LOAD_PAGE_FAULT
        | crate::arch::riscv64::traps::CAUSE_STORE_PAGE_FAULT => 11, // SIGSEGV
        _ => return false,
    };
    let Some(run) = crate::user::current_run_id() else {
        return false;
    };
    let action = with_table(|table| {
        if table.run != run {
            None
        } else {
            Some(table.actions[signal])
        }
    });
    let Some(action) = action else {
        return false;
    };
    if action.handler <= SIG_IGN {
        return false;
    }
    FAULT_SIGNALS.fetch_add(1, Ordering::Relaxed);
    arm(frame, run, signal, action, Source::Fault { address: stval });
    true
}

/// `rt_sigreturn`: restore the trap frame saved when the kernel delivered the
/// signal whose handler is returning.
///
/// `sp` must point at the frame the kernel armed for this thread (the restorer
/// runs with exactly that `sp`). The pointer range is validated `U|R` first, so
/// a bad frame is `-EFAULT`; an absent or mismatched frame is `-EINVAL`. On
/// success the current trap frame is replaced wholesale and `dispatch` returns
/// without bumping `sepc`, so `sret` resumes the interrupted instruction with its
/// original `sp`, registers and blocked mask.
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
        if table.run != run {
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
    // `TrapFrame` (all `usize` fields), and the magic/signum checks below reject
    // a frame this kernel did not write.
    let signal = unsafe { core::ptr::read_unaligned(raw.as_ptr().cast::<SignalFrame>()) };
    if signal.magic != SIGFRAME_MAGIC || signal.signum == 0 || signal.signum >= MAX_SIGNALS as u64 {
        return Err(EINVAL);
    }

    let restored_mask = signal.mask & VALID_MASK & !UNBLOCKABLE_MASK;
    let prev_frame = signal.prev_frame as usize;
    let restored = signal.saved;

    let restored_ok = with_table(|table| {
        if table.run != run {
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
