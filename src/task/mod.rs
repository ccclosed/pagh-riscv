//! Kernel threads: the scheduler, the context switch it drives, and the tests
//! that keep both honest.
//!
//! This is the M6a half of the milestone: threads *inside* the kernel, a
//! round-robin ready queue and a preemption hook. There is no user mode, no
//! system call and no address space per thread yet — those are M6b, and the
//! interfaces here are the ones it builds on (`spawn` a thread whose body will
//! become a user-program trampoline, `yield`/`tick` to switch, `exit` to end).
//!
//! The architecture split follows the rest of the tree: [`crate::arch::riscv64::switch`]
//! owns the register contract (which registers a switch must preserve and the
//! assembly that does it), while this subsystem owns policy — what a task is,
//! where its stack comes from, who runs next. A future port replaces the former
//! and keeps the latter.
//!
//! Read [`scheduler`] for the design (states, ready queue, lock discipline,
//! stacks and the M3 timer seam) and [`selftest`] for the evidence.

pub mod scheduler;
pub mod selftest;

// The facade the rest of the kernel uses. The tests and the M3 seam reach into
// `scheduler::` directly: they need internals (`check_stack`, `tick_from_timer`)
// that are deliberately not part of this surface.
pub use scheduler::{
    snapshot, spawn_demo, stats, tick, timer_clock_attached, TaskInfo, GUARD_SIZE, MAX_TASKS,
    QUANTUM_TICKS, STACK_SIZE,
};

/// Build the scheduler. Called once from `kmain`, after the PMM exists and after
/// the trap vector is installed; see [`scheduler::init`].
pub fn init() {
    scheduler::init();
}
