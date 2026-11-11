//! Kernel threads: the scheduler, the context switch it drives, and the tests
//! that keep both honest.
//!
//! Threads *inside* the kernel plus the scheduling primitives user threads use:
//! a round-robin ready queue, a preemption hook, `block_current`/`wake_task` with
//! tick deadlines, and `kill_group` for `exit_group`/fault. There is no address
//! space per thread and no `sscratch` handling here — a user thread is a task
//! whose context starts at the U-mode trap-return path (see `src/user/`).
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
    snapshot, spawn_demo, stats, tick, TaskInfo, GUARD_SIZE, MAX_TASKS, QUANTUM_TICKS, STACK_SIZE,
};

/// Build the scheduler. Called once from `kmain`, after the PMM exists and after
/// the trap vector is installed; see [`scheduler::init`].
pub fn init() {
    scheduler::init();
}
