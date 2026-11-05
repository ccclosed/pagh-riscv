//! The futex wait queue: static storage, a `(run, uaddr)` key and the scheduler
//! block/wake pair.
//!
//! This is the minimum a pthread-style implementation needs: `FUTEX_WAIT`
//! atomically checks the user word and blocks if it still has the expected
//! value; `FUTEX_WAKE` makes the first N waiters on that word runnable. There is
//! no requeue, no priority inheritance and no PI futex — those operations are
//! `-ENOSYS` in the syscall layer, not half-implemented here.
//!
//! # Why `(run, uaddr)` and not `uaddr` alone
//!
//! User virtual addresses are process-relative only in name: this kernel has one
//! address space, and the next program will map a stack or a `mmap` page at the
//! same virtual address. Keying on the run id (monotonic for the boot, see
//! `user::NEXT_RUN_ID`) makes a waiter left behind by a killed run invisible to
//! the next one; [`clear_run`] additionally drops them at teardown so the fixed
//! table cannot fill up across `selftest` passes.
//!
//! # Storage and limits
//!
//! [`MAX_WAITERS`] (`32`) waiter slots, at most [`MAX_ADDRESSES`] (`16`)
//! distinct user addresses at a time (across all runs, though only one run can be
//! live). Each waiter stores only `run`, `uaddr` and the scheduler tid — 32
//! entries of a few machine words, no heap and no allocation on the wait path.
//! A full waiter table is `-EAGAIN`, a seventeenth address is `-ENOMEM`; both are
//! checked before the blocking switch, so a failed wait changes nothing.
//!
//! # Lock order and the lost-wakeup argument
//!
//! The one rule: **futex queue lock → scheduler lock** (never the reverse), and
//! the queue lock is never held across [`block_current`]. The wait path is
//!
//! ```text
//! check *uaddr == val      (syscall layer, with copy_from_user)
//! lock queue; insert waiter; unlock queue
//! block_current()          (scheduler lock inside, switch inside)
//! lock queue; remove waiter; unlock queue
//! ```
//!
//! A wake cannot slip between the insert and the block: this is a single hart,
//! and the whole syscall trap runs with `sstatus.SIE = 0`, so no timer and no
//! other thread can run in that window. Once the thread is blocked, `wake_task`
//! finds its waiter entry under the queue lock and marks it ready; the woken
//! thread does not take the queue lock until it runs again, so there is no
//! lock inversion. A timeout follows the same shape: the tick marks the task
//! ready, the waiter removes its own entry when it resumes.
//!
//! [`WaitOutcome::TimedOut`] and [`WaitOutcome::Woken`] are decided by the
//! scheduler, not by a second timestamp in this table; a wake that arrives while
//! the thread is already ready (it timed out first) is ignored by `wake_task`
//! and counted as not woken.

use crate::arch::riscv64::csr;
use crate::sync::Spinlock;
use crate::task::scheduler::{self, BlockOutcome};

/// Waiter slots in the static table.
pub(crate) const MAX_WAITERS: usize = 32;

/// Distinct user addresses the table can track at once.
pub(crate) const MAX_ADDRESSES: usize = 16;

/// One blocked thread waiting on one address.
#[derive(Clone, Copy)]
struct Waiter {
    used: bool,
    run: u64,
    uaddr: usize,
    tid: u64,
}

impl Waiter {
    const fn empty() -> Self {
        Waiter {
            used: false,
            run: 0,
            uaddr: 0,
            tid: 0,
        }
    }
}

struct FutexTable {
    waiters: [Waiter; MAX_WAITERS],
}

impl FutexTable {
    const fn empty() -> Self {
        FutexTable {
            waiters: [const { Waiter::empty() }; MAX_WAITERS],
        }
    }

    fn used(&self) -> usize {
        self.waiters.iter().filter(|waiter| waiter.used).count()
    }

    /// Distinct addresses in use. The table is tiny and this runs only when a
    /// new address is about to be added, so a quadratic scan is fine.
    fn addresses(&self) -> usize {
        let mut count = 0usize;
        for index in 0..MAX_WAITERS {
            let waiter = self.waiters[index];
            if !waiter.used {
                continue;
            }
            let first = self.waiters[..index]
                .iter()
                .all(|earlier| !earlier.used || earlier.uaddr != waiter.uaddr);
            if first {
                count += 1;
            }
        }
        count
    }

    fn has_address(&self, uaddr: usize) -> bool {
        self.waiters
            .iter()
            .any(|waiter| waiter.used && waiter.uaddr == uaddr)
    }

    fn free_slot(&self) -> Option<usize> {
        self.waiters.iter().position(|waiter| !waiter.used)
    }

    fn remove_tid(&mut self, tid: u64) {
        for waiter in &mut self.waiters {
            if waiter.used && waiter.tid == tid {
                waiter.used = false;
            }
        }
    }
}

static FUTEX: Spinlock<FutexTable> = Spinlock::new(FutexTable::empty());

/// Why a wait could not be registered. Both are translated by the syscall layer.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum WaitError {
    /// All [`MAX_WAITERS`] slots are occupied (`-EAGAIN`).
    Full,
    /// [`MAX_ADDRESSES`] distinct addresses are already tracked (`-ENOMEM`).
    NoMemory,
}

/// How a registered wait ended.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum WaitOutcome {
    /// [`wake`] (or a kill) made the thread ready.
    Woken,
    /// The tick deadline expired first.
    TimedOut,
}

/// Register a waiter, block until woken or timed out, then unregister.
///
/// The caller has already verified that `*uaddr == val` through
/// `copy_from_user`; doing the check here would require a user pointer in a
/// module that deliberately only sees a key. `timeout` is in scheduler ticks
/// (`None` = no deadline); `Some(0)` is answered by the scheduler immediately.
pub(crate) fn wait(run: u64, uaddr: usize, timeout: Option<u64>) -> Result<WaitOutcome, WaitError> {
    let tid = scheduler::current_id();
    // Interrupts are already masked in a syscall trap; masking here makes the
    // table correct for any future caller too, and keeps the timer from
    // preempting a spinlock holder that a woken thread might spin on.
    let registered = csr::without_interrupts(|| -> Result<(), WaitError> {
        let mut table = FUTEX.lock();
        // A task has at most one wait in flight, and a stale entry for this tid
        // can only be left by a bug (a resumed wait always removes itself); drop
        // it instead of letting it occupy a slot forever.
        table.remove_tid(tid);
        let known = table.has_address(uaddr);
        if !known && table.addresses() >= MAX_ADDRESSES {
            return Err(WaitError::NoMemory);
        }
        let Some(slot) = table.free_slot() else {
            return Err(WaitError::Full);
        };
        table.waiters[slot] = Waiter {
            used: true,
            run,
            uaddr,
            tid,
        };
        Ok(())
    });
    registered?;

    let outcome = scheduler::block_current(timeout);

    csr::without_interrupts(|| {
        FUTEX.lock().remove_tid(tid);
    });
    Ok(match outcome {
        BlockOutcome::TimedOut => WaitOutcome::TimedOut,
        BlockOutcome::Woken => WaitOutcome::Woken,
    })
}

/// Wake at most `count` waiters on `(run, uaddr)`; returns how many were made
/// runnable.
///
/// A waiter that already timed out is not blocked anymore, so `wake_task`
/// returns false and it is not counted — the entry is still removed, because it
/// can never wait on it again without registering a new one. The scheduler lock
/// is taken *inside* the futex lock (the documented order); `wake_task` never
/// calls back here.
pub(crate) fn wake(run: u64, uaddr: usize, count: usize) -> usize {
    if count == 0 {
        return 0;
    }
    csr::without_interrupts(|| {
        let mut table = FUTEX.lock();
        let mut woken = 0usize;
        for index in 0..MAX_WAITERS {
            if woken == count {
                break;
            }
            let waiter = table.waiters[index];
            if !waiter.used || waiter.run != run || waiter.uaddr != uaddr {
                continue;
            }
            table.waiters[index].used = false;
            if scheduler::wake_task(waiter.tid) {
                woken += 1;
            }
        }
        woken
    })
}

/// Forget every waiter of `run`, without waking anybody.
///
/// Called by the runner after the last thread is gone (and by nothing on the
/// hot path). At that point every task carrying the run is `Exited`, so a
/// "wake" would only queue a slot that is about to be reused; dropping the
/// entries is the honest cleanup.
pub(crate) fn clear_run(run: u64) {
    csr::without_interrupts(|| {
        let mut table = FUTEX.lock();
        for waiter in &mut table.waiters {
            if waiter.used && waiter.run == run {
                waiter.used = false;
            }
        }
    });
}

/// Waiters currently registered (the selftest reads this after a run).
pub(crate) fn live_waiters() -> usize {
    csr::without_interrupts(|| FUTEX.lock().used())
}

/// Distinct addresses currently registered (evidence that `clear_run` worked).
#[allow(dead_code)] // read by diagnostics/tests; kept next to `live_waiters`
pub(crate) fn live_addresses() -> usize {
    csr::without_interrupts(|| FUTEX.lock().addresses())
}
