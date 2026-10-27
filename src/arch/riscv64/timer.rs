//! The supervisor timer tick: an absolute-deadline periodic interrupt that gives
//! the kernel a clock, an idle wake-up, and the hook a scheduler will preempt on.
//!
//! # Why SBI and not `mtimecmp`
//!
//! `mtimecmp` is an **M-mode** CSR on this platform: S-mode cannot write it (the
//! same class of mistake as reading `mhartid`, see [`super::boot`]), so the next
//! deadline is handed to firmware with the SBI TIME extension
//! ([`crate::arch::riscv64::sbi::set_timer`]). The `time` CSR *is* readable from
//! S-mode, which makes the accounting side ordinary register reads.
//!
//! # The contract
//!
//! * The tick is periodic at [`TICK_HZ`], but it is programmed as an **absolute**
//!   deadline: [`NEXT_DEADLINE`] advances by exactly one period per interrupt, so
//!   firmware latency does not accumulate into drift. If the hart was stalled for
//!   several periods (a long `wfi` in a debugger, a stopped emulator), the
//!   handler skips to the next deadline in the future in a bounded loop instead
//!   of taking a burst of interrupts to "catch up" — a tick is a clock stamp, not
//!   an event that must be replayed.
//! * [`on_tick`] runs from the trap handler, with interrupts masked and the trap
//!   frame still live. It therefore does no allocation, no console output, takes
//!   no kernel spinlock, and calls the optional [`set_tick_handler`] hook under
//!   the same restrictions. These are not style notes: a panic or a deadlock here
//!   takes the machine down with the frame as the only evidence.
//! * Nothing here knows about scheduling. The sequencer is M6: it installs a
//!   `fn()` with [`set_tick_handler`] and decides there whether a tick means
//!   "preempt".

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use crate::arch::riscv64::csr;
use crate::arch::riscv64::sbi;

/// Ticks per second of the kernel clock.
///
/// Named rather than inlined because three places must agree on it: the boot log,
/// `sleep` (which converts seconds to ticks), and `time` (which converts ticks to
/// milliseconds). 100 Hz is the same rate the x86 kernel uses: fine enough for an
/// interactive `sleep` and coarse enough that the tick costs nothing noticeable.
pub const TICK_HZ: usize = 100;

/// Milliseconds per tick, derived from [`TICK_HZ`] so the two cannot drift.
pub const TICK_MS: usize = 1000 / TICK_HZ;

/// How many periods the handler may skip when it notices it is behind schedule.
///
/// Bounded on purpose: an unbounded catch-up loop in trap context is a hang with
/// extra steps, and after this many skipped periods the counter lag is already so
/// large that the exact number does not matter.
const MAX_CATCHUP: usize = 16;

/// `time` units per second, as reported by the device tree.
static TIMEBASE_HZ: AtomicUsize = AtomicUsize::new(0);
/// `time` units between two ticks (timebase units, not milliseconds).
static TICK_PERIOD: AtomicUsize = AtomicUsize::new(0);
/// Absolute `time` value of the next tick, in timebase units.
static NEXT_DEADLINE: AtomicU64 = AtomicU64::new(0);
/// Ticks elapsed since [`init`].
static TICKS: AtomicU64 = AtomicU64::new(0);
/// Timer interrupts dispatched to [`on_tick`], counted separately from [`TICKS`]
/// so a handler that runs but fails to count is distinguishable from a trap that
/// never arrived at all.
static INTERRUPTS: AtomicU64 = AtomicU64::new(0);
/// Non-zero SBI error codes from `set_timer` (zero on a healthy machine).
static SET_TIMER_ERRORS: AtomicU64 = AtomicU64::new(0);
/// Whether the timer is programmed and `sie.STIE` is set.
static RUNNING: AtomicBool = AtomicBool::new(false);
/// The optional per-tick hook (M6's preemption seam); `0` means "none".
static TICK_HANDLER: AtomicUsize = AtomicUsize::new(0);

/// Program the first deadline and enable the supervisor timer interrupt.
///
/// `timebase_hz` is the device tree's `/cpus:timebase-frequency`. Returns whether
/// the timer is now running; a `false` is a loud, non-fatal fallback — `time` and
/// `sleep` still work by polling the `time` CSR, they just cannot sleep.
///
/// Order inside matters: the first deadline is written with SBI **before**
/// `sie.STIE` is set. Enabling the interrupt first would expose firmware's
/// default compare value (often 0, i.e. "long past") as an immediate interrupt
/// storm during the rest of the boot.
pub fn init(timebase_hz: usize) -> bool {
    if timebase_hz == 0 {
        return false;
    }
    let period = timebase_hz / TICK_HZ;
    if period == 0 {
        // A timebase slower than the tick rate cannot express the period.
        return false;
    }
    TIMEBASE_HZ.store(timebase_hz, Ordering::Relaxed);
    TICK_PERIOD.store(period, Ordering::Relaxed);

    let first = sbi::time().wrapping_add(period) as u64;
    NEXT_DEADLINE.store(first, Ordering::Relaxed);
    if sbi::set_timer(first as usize).is_err() {
        SET_TIMER_ERRORS.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    csr::write_sie(csr::read_sie() | csr::SIE_STIE);
    RUNNING.store(true, Ordering::Relaxed);
    true
}

/// Account for one timer interrupt and schedule the next one.
///
/// Called by [`crate::arch::riscv64::traps::trap_dispatch`] for `scause` 5 with
/// the interrupt bit set. Acknowledging the interrupt *is* pointing firmware at a
/// new deadline — there is no separate EOI register for a timer.
pub fn on_tick() {
    let period = TICK_PERIOD.load(Ordering::Relaxed);
    if period != 0 {
        // Absolute deadline: `previous + period`, never `now + period`. The clock
        // then reflects real elapsed time instead of the handler's own latency.
        let mut next = NEXT_DEADLINE
            .load(Ordering::Relaxed)
            .wrapping_add(period as u64);
        let now = sbi::time() as u64;
        let mut skipped = 0;
        while next <= now && skipped < MAX_CATCHUP {
            next = next.wrapping_add(period as u64);
            skipped += 1;
        }
        NEXT_DEADLINE.store(next, Ordering::Relaxed);
        if sbi::set_timer(next as usize).is_err() {
            SET_TIMER_ERRORS.fetch_add(1, Ordering::Relaxed);
        }
    }
    TICKS.fetch_add(1, Ordering::Relaxed);
    INTERRUPTS.fetch_add(1, Ordering::Relaxed);

    let handler = TICK_HANDLER.load(Ordering::Relaxed);
    if handler != 0 {
        // SAFETY: `set_tick_handler` is the only writer and it stores exactly a
        // `fn()` converted with `as usize`; converting the stored value back to
        // the same function-pointer type is therefore valid. The handler is a
        // plain `fn()` (no captures, no `'static` data), so calling it here has no
        // lifetime or aliasing requirements beyond its own.
        let hook: fn() = unsafe { core::mem::transmute::<usize, fn()>(handler) };
        hook();
    }
}

/// Install the per-tick hook (M6's preemption seam). `None` clears it.
///
/// # Contract for the hook
///
/// It is called **from trap context**, with interrupts masked, once per tick:
///
/// * no allocation;
/// * no console output (`kprintln!` takes the framebuffer lock, which the
///   interrupted context may hold — the same class of deadlock M3 fixed for the
///   UART);
/// * no kernel spinlock a normal thread may hold; the one non-reentrant lock is
///   held by whoever the tick interrupted, and the handler runs before that
///   context can release it;
/// * short and panic-free (`panic = "abort"`).
///
/// The scheduler is expected to record "preempt requested" here and switch at a
/// point where the interrupted context is a task, not to switch from inside the
/// hook on the boot thread.
pub fn set_tick_handler(handler: Option<fn()>) {
    TICK_HANDLER.store(handler.map(|f| f as usize).unwrap_or(0), Ordering::Relaxed);
}

/// Ticks since [`init`]: the kernel's monotonic clock.
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// Timer interrupts dispatched since [`init`].
pub fn interrupts() -> u64 {
    INTERRUPTS.load(Ordering::Relaxed)
}

/// Uptime in milliseconds, derived from [`ticks`].
pub fn uptime_ms() -> u64 {
    ticks().saturating_mul(TICK_MS as u64)
}

/// Whether the tick is programmed and its `sie` bit set.
pub fn running() -> bool {
    RUNNING.load(Ordering::Relaxed)
}

/// `time` units per second the tick is derived from (`0` before [`init`]).
pub fn timebase_hz() -> usize {
    TIMEBASE_HZ.load(Ordering::Relaxed)
}

/// `time` units between two ticks (`0` before [`init`]).
pub fn tick_period() -> usize {
    TICK_PERIOD.load(Ordering::Relaxed)
}

/// `set_timer` failures since boot (`0` on a healthy machine).
pub fn set_timer_errors() -> u64 {
    SET_TIMER_ERRORS.load(Ordering::Relaxed)
}

/// Block the caller for `wait_ticks` ticks.
///
/// This is the shell's `sleep`, expressed in the kernel's own clock. It waits with
/// `wfi`: the next interrupt is exactly what should wake it, and the timer is one,
/// so the hart does not spin (an external interrupt — a keypress — may also wake
/// the wait early; the loop then simply goes back to sleep).
///
/// When the timer never came up (a machine without a timebase, or firmware
/// refusing the SBI call) it degrades to polling the `time` CSR rather than
/// hanging: the command must never be a way to wedge the machine.
pub fn sleep_ticks(wait_ticks: u64) {
    if wait_ticks == 0 {
        return;
    }
    let period = TICK_PERIOD.load(Ordering::Relaxed);
    if !running() {
        if period == 0 {
            return; // no clock at all: there is no "later" to wait for
        }
        let deadline =
            sbi::time().wrapping_add((period as u64).saturating_mul(wait_ticks) as usize);
        while sbi::time() < deadline {
            core::hint::spin_loop();
        }
        return;
    }
    let deadline = ticks().wrapping_add(wait_ticks);
    while ticks() < deadline {
        // SAFETY: `wfi` is a hint; it cannot fault, and every interrupt source
        // enabled at this point returns through `trap_entry`/`sret`, so a pending
        // one resumes exactly here. If interrupts were disabled by a caller, the
        // loop still makes progress on the next `wfi` wake-up (a pending timer
        // interrupt resumes `wfi` even when it cannot be taken).
        unsafe { core::arch::asm!("wfi", options(nomem, nostack)) };
    }
}
