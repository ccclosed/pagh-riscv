//! The kernel's second ABI: what a suspended kernel thread's register file looks
//! like, and the one routine that swaps two of them.
//!
//! Why this is not part of `traps.rs`: the trap frame is the *CPU's* contract
//! (33 slots, 264 bytes, `sret` restores it), while a context switch is a plain
//! function call's contract — save what the C ABI says a call must preserve
//! (`ra`, `sp`, `s0`–`s11`) and nothing else. Caller-saved registers (`t0`–`t6`,
//! `a0`–`a7`) are deliberately *not* saved: [`switch`] is called from Rust as an
//! `extern "C"` function, so the compiler already assumes the call may clobber
//! them and has spilled anything it still needs. Saving them would be 128 more
//! bytes per thread and buy nothing.
//!
//! The trap frame is untouched by this module. A thread switched out inside a
//! trap handler keeps its trap frame on its own stack; when it is resumed the
//! handler continues and `sret`s exactly as it would have without the switch —
//! which is why timer preemption needs no separate "preempted frame" concept.
//! The two contracts compose because a context switch never runs `sret`: it is
//! called *inside* the handler, and each thread's stack holds its own frame.
//!
//! Layout is load-bearing, so it is not trusted to a comment: the assembly takes
//! its offsets from [`TaskContext`]'s Rust layout through `global_asm!` `const`
//! operands, and the `const _` block below fails the build if the struct stops
//! being exactly 14 pointer-sized fields. Edit the struct and the assembly
//! follows; edit the assembly's operand list and it stops compiling.

use core::arch::global_asm;

/// The callee-saved state of a suspended kernel thread.
///
/// Field order *is* the save order in `task_switch` (see the `global_asm!` block
/// below); `#[repr(C)]` makes that order a promise, and every `sd`/`ld` offset
/// is computed from this layout rather than written by hand. A fresh thread's
/// context is built by [`initial_context`]: `ra` points at its entry trampoline
/// and `sp` at its stack top, so the first switch "returns" into the trampoline.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TaskContext {
    /// Where the thread continues when it is scheduled again. For a thread that
    /// has never run this is its entry trampoline (`task_entry` in `task::scheduler`).
    pub ra: usize,
    /// The stack pointer. Each thread owns its stack, so this field is the only
    /// place a suspended thread's stack position is recorded; the saved value
    /// points into the switch call's frame and unwinds correctly on resume.
    pub sp: usize,
    /// Frame pointer (`s0`): the kernel is built with `force-frame-pointers=yes`,
    /// so this is a live value a debugger would want, not padding.
    pub s0: usize,
    /// `s1`, callee-saved.
    pub s1: usize,
    /// `s2`, callee-saved.
    pub s2: usize,
    /// `s3`, callee-saved.
    pub s3: usize,
    /// `s4`, callee-saved.
    pub s4: usize,
    /// `s5`, callee-saved.
    pub s5: usize,
    /// `s6`, callee-saved.
    pub s6: usize,
    /// `s7`, callee-saved.
    pub s7: usize,
    /// `s8`, callee-saved.
    pub s8: usize,
    /// `s9`, callee-saved.
    pub s9: usize,
    /// `s10`, callee-saved.
    pub s10: usize,
    /// `s11`, callee-saved.
    pub s11: usize,
}

// Stated once, checked at build time. The assembly reads these offsets via
// `const` operands, so this block is the second half of the same statement: the
// struct has 14 eight-byte fields in the documented order, and `TaskContext` is
// exactly one pointer per field with no padding the assembly could walk over.
const _: () = {
    use core::mem::{offset_of, size_of};
    assert!(size_of::<usize>() == 8);
    assert!(size_of::<TaskContext>() == 14 * 8);
    assert!(offset_of!(TaskContext, ra) == 0 * 8);
    assert!(offset_of!(TaskContext, sp) == 1 * 8);
    assert!(offset_of!(TaskContext, s0) == 2 * 8);
    assert!(offset_of!(TaskContext, s1) == 3 * 8);
    assert!(offset_of!(TaskContext, s2) == 4 * 8);
    assert!(offset_of!(TaskContext, s3) == 5 * 8);
    assert!(offset_of!(TaskContext, s4) == 6 * 8);
    assert!(offset_of!(TaskContext, s5) == 7 * 8);
    assert!(offset_of!(TaskContext, s6) == 8 * 8);
    assert!(offset_of!(TaskContext, s7) == 9 * 8);
    assert!(offset_of!(TaskContext, s8) == 10 * 8);
    assert!(offset_of!(TaskContext, s9) == 11 * 8);
    assert!(offset_of!(TaskContext, s10) == 12 * 8);
    assert!(offset_of!(TaskContext, s11) == 13 * 8);
};

global_asm!(
    r#"
    .section .text
    .align 4
    .globl task_switch
task_switch:
    /* a0 = from (*mut TaskContext), a1 = to (*const TaskContext).

       Save the caller's callee-saved registers, then load the next thread's.
       `ret` is a jump to the restored `ra`: for a thread that has run before,
       that is the instruction after its own `call task_switch`; for a fresh
       thread it is its entry trampoline, whose `sp` is the stack top. */
    sd      ra,  {RA}(a0)
    sd      sp,  {SP}(a0)
    sd      s0,  {S0}(a0)
    sd      s1,  {S1}(a0)
    sd      s2,  {S2}(a0)
    sd      s3,  {S3}(a0)
    sd      s4,  {S4}(a0)
    sd      s5,  {S5}(a0)
    sd      s6,  {S6}(a0)
    sd      s7,  {S7}(a0)
    sd      s8,  {S8}(a0)
    sd      s9,  {S9}(a0)
    sd      s10, {S10}(a0)
    sd      s11, {S11}(a0)

    ld      ra,  {RA}(a1)
    ld      sp,  {SP}(a1)
    ld      s0,  {S0}(a1)
    ld      s1,  {S1}(a1)
    ld      s2,  {S2}(a1)
    ld      s3,  {S3}(a1)
    ld      s4,  {S4}(a1)
    ld      s5,  {S5}(a1)
    ld      s6,  {S6}(a1)
    ld      s7,  {S7}(a1)
    ld      s8,  {S8}(a1)
    ld      s9,  {S9}(a1)
    ld      s10, {S10}(a1)
    ld      s11, {S11}(a1)
    ret
"#,
    RA = const core::mem::offset_of!(TaskContext, ra),
    SP = const core::mem::offset_of!(TaskContext, sp),
    S0 = const core::mem::offset_of!(TaskContext, s0),
    S1 = const core::mem::offset_of!(TaskContext, s1),
    S2 = const core::mem::offset_of!(TaskContext, s2),
    S3 = const core::mem::offset_of!(TaskContext, s3),
    S4 = const core::mem::offset_of!(TaskContext, s4),
    S5 = const core::mem::offset_of!(TaskContext, s5),
    S6 = const core::mem::offset_of!(TaskContext, s6),
    S7 = const core::mem::offset_of!(TaskContext, s7),
    S8 = const core::mem::offset_of!(TaskContext, s8),
    S9 = const core::mem::offset_of!(TaskContext, s9),
    S10 = const core::mem::offset_of!(TaskContext, s10),
    S11 = const core::mem::offset_of!(TaskContext, s11),
);

extern "C" {
    /// The assembly above. `from` receives the caller's registers and `to`'s
    /// registers are loaded before the jump; see [`switch`] for the safe wrapper
    /// and its preconditions.
    fn task_switch(from: *mut TaskContext, to: *const TaskContext);
}

/// Save the calling thread's context into `from`, then resume `to`.
///
/// When the calling thread is scheduled again, this call *returns* (the saved
/// `ra` points just after the call), and the caller continues as if nothing had
/// happened. A fresh thread never returns here: its `to.ra` is the entry
/// trampoline, which runs the thread body and calls `exit` instead of returning.
///
/// # Safety
///
/// * `from` and `to` must point to live, distinct [`TaskContext`] values that
///   stay at those addresses until the switch completes. The scheduler keeps
///   every context in a slot of its static task table, so this holds by
///   construction and no allocation is involved.
/// * Interrupts must be disabled around the pair of stores/loads: a trap that
///   re-entered the scheduler between the two halves would switch the same
///   thread twice. The scheduler's critical section provides that mask.
/// * `to` must describe a context that is runnable: either a saved context of a
///   live thread or one built by [`initial_context`]. A `sp` that is not the
///   thread's own 16-byte-aligned stack top is memory corruption as far as this
///   function can tell.
#[inline]
pub unsafe fn switch(from: *mut TaskContext, to: *const TaskContext) {
    // SAFETY: the caller upheld the contract above; `task_switch` is the
    // documented implementation of exactly this operation and does not return to
    // this point in the *current* thread's timeline except on a later resume.
    unsafe { task_switch(from, to) }
}

impl TaskContext {
    /// A context with every field zero: the "no thread here yet" value of a
    /// task-table slot. A runnable context comes from [`initial_context`], and a
    /// suspended one from [`switch`] having saved it.
    pub const fn zeroed() -> TaskContext {
        TaskContext {
            ra: 0,
            sp: 0,
            s0: 0,
            s1: 0,
            s2: 0,
            s3: 0,
            s4: 0,
            s5: 0,
            s6: 0,
            s7: 0,
            s8: 0,
            s9: 0,
            s10: 0,
            s11: 0,
        }
    }
}

/// Build the context a thread is first switched to.
///
/// `entry` is a C-ABI function that never returns (it calls the thread body and
/// then `exit`), and `stack_top` must be the 16-byte-aligned top of a stack that
/// belongs to nobody else. The callee-saved registers start at zero: they carry
/// no meaning for a thread that has not run, and a debugger reads them as "fresh"
/// rather than as a plausible leftover from whoever ran before.
pub fn initial_context(entry: extern "C" fn() -> !, stack_top: usize) -> TaskContext {
    // The ABI condition is cheap to check and catastrophic to get wrong: a stack
    // top that is off by 8 makes every frame in the new thread misaligned.
    debug_assert!(
        stack_top % 16 == 0,
        "thread stack top must be 16-byte aligned"
    );
    TaskContext {
        ra: entry as usize,
        sp: stack_top,
        s0: 0,
        s1: 0,
        s2: 0,
        s3: 0,
        s4: 0,
        s5: 0,
        s6: 0,
        s7: 0,
        s8: 0,
        s9: 0,
        s10: 0,
        s11: 0,
    }
}
