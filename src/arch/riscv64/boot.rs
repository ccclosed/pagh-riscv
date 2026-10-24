//! The very first instructions: set a stack, park the secondary harts, call Rust.
//!
//! OpenSBI enters S-mode at `_start` with `a0` = hart id and `a1` = the device
//! tree's physical address, interrupts disabled and no stack. The only thing
//! this file may assume is that much.

use core::arch::global_asm;

global_asm!(
    r#"
    .section .text.entry
    .globl _start
_start:
    /* Only hart 0 boots the kernel. The others park here, before any Rust code
       can run, so no kernel data structure is ever touched by two harts. */
    bnez a0, 2f

    /* A stack before anything else: `sp` is arbitrary on entry. */
    la      sp, __stack_top
    /* Rust's ABI wants 16-byte alignment; the linker script aligns the top. */
    andi    sp, sp, -16

    /* Remember the hart id where Rust can read it. `mhartid` is an M-mode CSR:
       reading it from S-mode is an illegal instruction (verified — the `hart`
       command used to kill the machine), so the id is captured here, while it is
       still in `a0`, and the kernel reads this static from then on. */
    la      t0, BOOT_HART_ID
    sd      a0, 0(t0)

    /* a0 = hart id, a1 = device tree address: exactly `kmain`'s arguments. */
    call    kmain

1:  wfi
    j       1b

2:  /* Secondary harts: wait for an IPI that will only come once SMP exists. */
    wfi
    j       2b
"#
);

// The boot stack is defined here, in the same file that loads it: `_start` sets
// `sp` to `__stack_top`, so the size and the symbol have to be stated next to
// each other or one of them drifts. 64 KiB is generous for a boot path that
// allocates nothing, and the `.align 4` (16 bytes) keeps the ABI's requirement.
global_asm!(
    r#"
    .section .bss
    .align 4
    .globl __stack_bottom
__stack_bottom:
    .space 65536
    .globl __stack_top
__stack_top:
"#
);

/// The hart id OpenSBI booted us on, written by `_start` (see the assembly
/// above) before any Rust code runs.
///
/// It is a plain `static mut` rather than a lock-protected cell because it is
/// written exactly once, before the kernel has more than one context to race
/// with, and never again.
#[no_mangle]
pub static mut BOOT_HART_ID: usize = 0;
