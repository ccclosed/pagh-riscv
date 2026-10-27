//! The task subsystem's in-QEMU self-tests, run by the shell's `selftest`
//! command after the memory suite.
//!
//! Same contract as `memory::selftest`: non-destructive, deterministic and
//! idempotent. Running `selftest` twice in one boot must print the same per-test
//! verdicts *and* the same task frame ledger — that is the proof that a thread's
//! stack goes back to the PMM when the thread exits, instead of being quietly
//! retained by the scheduler. What legitimately persists at idle is named in
//! `src/task/README.md` (the idle thread's stack and the static table); every
//! other frame this suite takes is returned before the next test runs.
//!
//! The suite starts by cancelling the demo cohort from the shell's `spawn`
//! command, so its ledger does not depend on what the user ran before
//! `selftest`. Every pump is bounded: a scheduler that stops making progress
//! reports `FAIL` and the machine keeps running, which is the difference between
//! a regression and a hang.
//!
//! Header: the worker bodies below are kernel threads exactly like the demo
//! ones — the selftest drives them by calling the same `tick` the timer will
//! call, not by entering the scheduler from the side.

use crate::memory::{pmm, FRAME_SIZE};
use crate::task::scheduler::{self, State, TaskInfo, GUARD_SIZE, MAX_TASKS, STACK_SIZE};

/// Iterations each interleaving worker runs.
const INTERLEAVE_WORK: u32 = 64;

/// Ticks the suite may pump while waiting for spawned work to finish. Every
/// worker needs a handful; this is orders of magnitude of headroom and exists so
/// a stuck scheduler fails instead of hanging the boot.
const PUMP_LIMIT: usize = 200_000;

/// A two-thread interleaving worker: bump the counter, hand the CPU back.
///
/// It uses `yield_now` rather than a tick so the switch itself is exercised even
/// if a future clock went silent. Two of these threads are queued together, so
/// the counters can only both reach [`INTERLEAVE_WORK`] if the scheduler kept
/// alternating them.
fn yield_worker() {
    for _ in 0..INTERLEAVE_WORK {
        scheduler::bump_counter();
        scheduler::yield_now();
    }
}

/// A worker that just returns: bump once and fall out of the entry point, which
/// is the `exit` path.
fn one_shot_worker() {
    scheduler::bump_counter();
}

/// A worker that overflows its own stack on purpose: one byte into the canary
/// word that sits on top of its guard frame, then a clean exit.
///
/// The write is inside the thread's own stack allocation, so it is not memory
/// corruption for anyone else — it is precisely the "a few bytes too deep" case
/// the guard exists to detect, and the reap must report it.
fn overflow_worker() {
    if let Some(canary) = scheduler::current_canary_address() {
        // SAFETY: `canary` points into this thread's own guard frame (the last
        // word of a PMM allocation this task owns until it exits), which is
        // mapped read/write. Overwriting it is the deliberate bug the test
        // measures; nothing reads through this pointer as a typed value again.
        unsafe { core::ptr::write_volatile(canary as *mut u8, 0x00) };
    }
    scheduler::exit();
}

/// Pump ticks until no spawned thread is runnable, then reap every exited stack.
///
/// Returns `false` if the deadline was hit (something is stuck), in which case
/// the caller reports `FAIL` and the suite continues on a best-effort basis.
fn pump_until_quiet() -> bool {
    let mut pumps = 0usize;
    while scheduler::foreign_tasks() > 0 && pumps < PUMP_LIMIT {
        scheduler::tick();
        pumps += 1;
    }
    let reaped = scheduler::reap();
    let quiet = scheduler::foreign_tasks() == 0;
    if !quiet {
        crate::kprintln!(
            "[selftest] task pumps stopped after {} ticks with {} spawned thread(s) still runnable",
            pumps,
            scheduler::foreign_tasks()
        );
    }
    let _ = reaped;
    quiet
}

/// Two kernel threads must interleave and both finish.
///
/// Evidence printed: iterations each thread completed, times each was switched
/// out, and the number of scheduler pumps the shell spent driving them. A thread
/// with `counter == INTERLEAVE_WORK` and `switches >= 2` ran in at least three
/// turns, which is only possible if the other thread also ran in between.
fn interleave() -> bool {
    let before_violations = scheduler::stats().canary_violations;
    let mut ids = [0u64; 2];
    for id in ids.iter_mut() {
        match scheduler::spawn(yield_worker) {
            Some(new_id) => *id = new_id,
            None => {
                crate::kprintln!("[selftest] no slot or no frames to spawn an interleaving worker");
                return false;
            }
        }
    }
    let mut ok = pump_until_quiet();

    let mut infos = [TaskInfo::zeroed(); MAX_TASKS];
    let count = scheduler::snapshot(&mut infos);
    let mut total_switches = 0u64;
    for id in ids {
        match infos[..count].iter().find(|info| info.id == id) {
            Some(info) => {
                total_switches += info.switches;
                if info.counter != INTERLEAVE_WORK as u64 {
                    crate::kprintln!(
                        "[selftest] task {} ran {} of {} iterations",
                        id,
                        info.counter,
                        INTERLEAVE_WORK
                    );
                    ok = false;
                }
                if info.switches < 2 {
                    crate::kprintln!(
                        "[selftest] task {} was switched out {} time(s); it never interleaved",
                        id,
                        info.switches
                    );
                    ok = false;
                }
                if info.state != State::Exited {
                    crate::kprintln!(
                        "[selftest] task {} is {} after the run",
                        id,
                        info.state.name()
                    );
                    ok = false;
                }
            }
            None => {
                crate::kprintln!("[selftest] task {} disappeared from the table", id);
                ok = false;
            }
        }
    }

    let after_violations = scheduler::stats().canary_violations;
    if after_violations != before_violations {
        crate::kprintln!(
            "[selftest] a normal task tripped the stack canary ({} -> {} violations)",
            before_violations,
            after_violations
        );
        ok = false;
    }
    crate::kprintln!(
        "[selftest] task interleave: 2 threads x {} iterations, {} switch-outs, {} live slots",
        INTERLEAVE_WORK,
        total_switches,
        count
    );
    ok
}

/// A thread's stack frames must come back to the PMM when it exits.
///
/// The check is done in three readings: before the spawn, while the thread is
/// alive (exactly [`scheduler::STACK_ALLOC_FRAMES`] frames fewer), and after the
/// reap (back to the starting number). A stack that is retained forever shows up
/// here as a `FAIL` *and* as a drifting ledger between two `selftest` passes.
fn exit_returns_stack() -> bool {
    let (before, _) = pmm::stats();
    if scheduler::spawn(one_shot_worker).is_none() {
        crate::kprintln!("[selftest] no slot or no frames to spawn the exit-test thread");
        return false;
    }
    let (during, _) = pmm::stats();
    let mut ok = pump_until_quiet();
    let quiet = scheduler::foreign_tasks() == 0;
    let (after, _) = pmm::stats();

    let expected_during = match before.checked_sub(scheduler::STACK_ALLOC_FRAMES) {
        Some(expected) => expected,
        None => {
            crate::kprintln!(
                "[selftest] the pool started with {} frames, fewer than a stack",
                before
            );
            return false;
        }
    };
    if during != expected_during {
        crate::kprintln!(
            "[selftest] spawning a thread moved the pool {} -> {}, expected {}",
            before,
            during,
            expected_during
        );
        ok = false;
    }
    if after != before {
        crate::kprintln!(
            "[selftest] an exited thread did not return its stack: {} frames free, {} before the spawn",
            after,
            before
        );
        ok = false;
    }
    if !quiet {
        ok = false;
    }
    crate::kprintln!(
        "[selftest] task exit: free frames {} -> {} (alive) -> {} (reaped), stack costs {} frames",
        before,
        during,
        after,
        scheduler::STACK_ALLOC_FRAMES
    );
    ok
}

/// A fresh stack must be real memory with an intact guard, its top 16-byte
/// aligned, and the fill pattern untouched before the thread runs.
///
/// This is what makes the canary check meaningful: if the guard frame were not
/// mapped and writable, "canary intact" would be vacuous.
fn stack_layout() -> bool {
    let (before, _) = pmm::stats();
    let Some(id) = scheduler::spawn(one_shot_worker) else {
        crate::kprintln!("[selftest] no slot or no frames to spawn the layout-test thread");
        return false;
    };
    let mut infos = [TaskInfo::zeroed(); MAX_TASKS];
    let count = scheduler::snapshot(&mut infos);
    let Some(info) = infos[..count].iter().find(|info| info.id == id).copied() else {
        crate::kprintln!("[selftest] task {} missing from the snapshot", id);
        return false;
    };

    let mut ok = true;
    if info.stack_base % FRAME_SIZE != 0 {
        crate::kprintln!(
            "[selftest] stack base {:#x} is not frame aligned",
            info.stack_base
        );
        ok = false;
    }
    if info.stack_top % 16 != 0 {
        crate::kprintln!(
            "[selftest] stack top {:#x} is not 16-byte aligned",
            info.stack_top
        );
        ok = false;
    }
    if info.stack_top != info.stack_base + GUARD_SIZE + STACK_SIZE {
        crate::kprintln!(
            "[selftest] stack span {:#x}..{:#x} is not guard {:#x} + usable {:#x}",
            info.stack_base,
            info.stack_top,
            GUARD_SIZE,
            STACK_SIZE
        );
        ok = false;
    }
    if info.stack_bytes() != STACK_SIZE {
        crate::kprintln!(
            "[selftest] stack reports {} usable bytes",
            info.stack_bytes()
        );
        ok = false;
    }

    let mut health_used = 0usize;
    match scheduler::check_stack(info.slot) {
        Some(health) => {
            health_used = health.used;
            if !health.canary_ok {
                crate::kprintln!("[selftest] a fresh stack's canary is already gone");
                ok = false;
            }
            if !health.guard_ok {
                crate::kprintln!("[selftest] a fresh stack's guard frame is already dirty");
                ok = false;
            }
            if health.usable != STACK_SIZE {
                crate::kprintln!(
                    "[selftest] stack reports {} usable of {} bytes",
                    health.usable,
                    STACK_SIZE
                );
                ok = false;
            }
            if health.used != 0 {
                crate::kprintln!(
                    "[selftest] a thread that has not run already used {} stack bytes",
                    health.used
                );
                ok = false;
            }
        }
        None => {
            crate::kprintln!("[selftest] the spawned thread has no inspectable stack");
            ok = false;
        }
    }
    let held = pmm::stats().0;
    if held + scheduler::STACK_ALLOC_FRAMES != before {
        crate::kprintln!(
            "[selftest] the stack is not {} contiguous PMM frames ({} free now, {} before the spawn)",
            scheduler::STACK_ALLOC_FRAMES,
            held,
            before
        );
        ok = false;
    }

    if !pump_until_quiet() {
        ok = false;
    }
    crate::kprintln!(
        "[selftest] task stack: base {:#x}, top {:#x}, guard {:#x} + usable {} B, fill untouched ({} B used)",
        info.stack_base,
        info.stack_top,
        GUARD_SIZE,
        STACK_SIZE,
        health_used
    );
    ok
}

/// A stack overflow must be detected when the stack is reaped, not silently
/// handed into the PMM.
///
/// One worker writes a byte into its own canary and exits; the reap before the
/// frames go back must count exactly one violation and print the event. The
/// counter is the assertion — the print is for the human reading the log.
fn canary_detects_overflow() -> bool {
    let before = scheduler::stats().canary_violations;
    if scheduler::spawn(overflow_worker).is_none() {
        crate::kprintln!("[selftest] no slot or no frames to spawn the overflow-test thread");
        return false;
    }
    let quiet = pump_until_quiet();
    let after = scheduler::stats().canary_violations;
    if !quiet {
        return false;
    }
    if after != before + 1 {
        crate::kprintln!(
            "[selftest] a smashed canary counted {} violation(s), expected {}",
            after - before,
            1
        );
        return false;
    }
    crate::kprintln!(
        "[selftest] task canary: overflow detected on reap (violations {} -> {})",
        before,
        after
    );
    true
}

/// The idle thread must exist, be reachable and hand control back without a
/// hardware clock.
///
/// Before M3 there is no interrupt to wake a `wfi`, so idle's software-clock
/// branch steps back after one pass; that is the path this probe exercises. With
/// M3 attached the probe returns one quantum later, and the assertion still
/// holds because it only requires idle's wake counter to advance.
fn idle_probe() -> bool {
    let before = scheduler::stats();
    let current = scheduler::current_id();
    let entered = scheduler::run_idle_once();
    let after = scheduler::stats();
    let mut ok = true;
    if !entered || after.idle_wakes <= before.idle_wakes {
        crate::kprintln!("[selftest] the idle thread was not entered");
        ok = false;
    }
    if scheduler::current_id() != current {
        crate::kprintln!(
            "[selftest] the probe did not come back to thread {} (now {})",
            current,
            scheduler::current_id()
        );
        ok = false;
    }
    crate::kprintln!(
        "[selftest] task idle: entered {} time(s) total, switched back to thread {}",
        after.idle_wakes,
        after.current_id
    );
    ok
}

/// Run every task check.
///
/// Nothing is asserted or panicked: the shell must keep running, and the verdicts
/// go to the serial line where the E2E tools grep them. Returns the number of
/// routines that passed, like the memory suite does.
pub fn run() -> usize {
    // The shell's `spawn` demo may have left a cohort running; its slots and its
    // frames are legitimate persistent state while it lives, but the suite's
    // ledger should not depend on it. Cancelling is deterministic: every demo
    // body checks the cohort epoch each iteration.
    scheduler::cancel_demo();
    let (before, total) = pmm::stats();
    crate::kprintln!(
        "[selftest] task PMM free frames before: {} of {}",
        before,
        total
    );

    let checks: [(&str, fn() -> bool); 5] = [
        ("task-interleave", interleave),
        ("task-exit-returns-stack", exit_returns_stack),
        ("task-stack-layout", stack_layout),
        ("task-canary", canary_detects_overflow),
        ("task-idle", idle_probe),
    ];
    let mut passed = 0usize;
    for (name, check) in checks {
        let ok = check();
        if ok {
            passed += 1;
        }
        crate::kprintln!("SELFTEST {} {}", name, if ok { "PASS" } else { "FAIL" });
    }

    // Belt to the reap's braces: anything a failed check left exited is returned
    // here, so even a failing pass leaves the pool as it found it.
    scheduler::reap();
    let (after, _) = pmm::stats();
    crate::kprintln!(
        "SELFTEST SUMMARY (task): {}/{} passed",
        passed,
        checks.len()
    );
    crate::kprintln!(
        "[selftest] task PMM hygiene: free frames {} -> {}",
        before,
        after
    );
    passed
}
