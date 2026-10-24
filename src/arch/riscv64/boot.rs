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

    /* a0 = hart id, a1 = device tree address: exactly `kmain`'s arguments. */
    call    kmain

1:  wfi
    j       1b

2:  /* Secondary harts: wait for an IPI that will only come once SMP exists. */
    wfi
    j       2b
"#
);

// The stack lives in Rust (`main::BOOT_STACK`) so that its size and alignment
// are stated once; the symbol below is what the assembly above loads.
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
