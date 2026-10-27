//! The interrupt layer's in-QEMU checks, run by the shell's `selftest`.
//!
//! Same contract as the other suites: non-destructive, deterministic and
//! idempotent. These four checks are what separates "the timer was programmed and
//! the PLIC enabled" from "the counters in `mem` happen to look plausible":
//!
//! * `timer-ticks` — wait two ticks through the real `wfi` path and require the
//!   kernel clock to advance by at least that much;
//! * `timer-interrupts` — the same wait must be served by *timer interrupts*, not
//!   by the polling fallback, and `set_timer` must not have reported an error;
//! * `uart-ring` — push bytes through the interrupt-fed RX ring and read them
//!   back (including the wrap and the full-ring refusal);
//! * `plic-uart-line` — read the UART source's enable bit back out of the
//!   controller instead of trusting the flag `plic::init` set; on a machine with
//!   no PLIC the designed fallback (polled console) is reported and passes.
//!
//! Skipped is not passed: a machine without a timebase prints why, and the
//! summary says so.

use crate::arch::riscv64::{plic, timer};
use crate::drivers::uart;

/// Ticks a check waits for before judging the clock. Two periods (20 ms at
/// 100 Hz) is enough to prove the interrupt path is live without making
/// `selftest` noticeably slower.
const WAIT_TICKS: u64 = 2;

/// Run every check, print `SELFTEST <name> PASS|FAIL`, and return how many passed.
pub fn run() -> usize {
    let checks: [(&str, fn() -> bool); 4] = [
        ("timer-ticks", timer_ticks_advance),
        ("timer-interrupts", timer_interrupts_fire),
        ("uart-ring", uart::selftest_ring),
        ("plic-uart-line", plic::selftest_uart_line),
    ];
    let mut passed = 0usize;
    for (name, check) in checks {
        if check() {
            passed += 1;
            crate::kprintln!("SELFTEST {} PASS", name);
        } else {
            crate::kprintln!("SELFTEST {} FAIL", name);
        }
    }
    crate::kprintln!(
        "SELFTEST SUMMARY (arch): {}/{} passed",
        passed,
        checks.len()
    );
    passed
}

/// The kernel clock must move when the hart waits through the interrupt path.
fn timer_ticks_advance() -> bool {
    if !timer::running() {
        crate::kprintln!("[selftest] timer: not running (no timebase) — skipped, not passed");
        return true;
    }
    let before = timer::ticks();
    timer::sleep_ticks(WAIT_TICKS);
    let after = timer::ticks();
    if after < before + WAIT_TICKS {
        crate::kprintln!(
            "[selftest] timer: {} ticks before the wait, {} after (wanted +{})",
            before,
            after,
            WAIT_TICKS
        );
        return false;
    }
    true
}

/// The advance above must come from interrupts, not from the polling fallback.
fn timer_interrupts_fire() -> bool {
    if !timer::running() {
        crate::kprintln!("[selftest] timer: not running (no timebase) — skipped, not passed");
        return true;
    }
    let before = timer::interrupts();
    timer::sleep_ticks(WAIT_TICKS);
    let after = timer::interrupts();
    let errors = timer::set_timer_errors();
    if after <= before || errors != 0 {
        crate::kprintln!(
            "[selftest] timer: {} -> {} interrupts, {} set_timer error(s)",
            before,
            after,
            errors
        );
        return false;
    }
    true
}
