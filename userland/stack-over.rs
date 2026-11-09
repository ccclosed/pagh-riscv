//! The stack-cap fixture: the same 4 KiB-per-level chain as `stack.rs`, but deep
//! enough that the 1 MiB per-process cap must stop it.
//!
//! `TARGET = 300` levels asks for 1.2 MiB of stack. The kernel grows the stack
//! page by page until the chain reaches `USER_STACK_FLOOR` (1 MiB below the top,
//! i.e. 256 pages), refuses to map the page below it, and the store that crossed
//! the cap dies as a normal store page fault. `SELFTEST user-stack-cap` requires
//! exactly that: the run ends `Exit::Fault` with `cause == store page fault`, the
//! growth-refusal counter moved, the PMM ledger is back to where it started, and
//! the kernel keeps running.
//!
//! This is the other half of the contract: demand growth must not turn into an
//! unbounded stack. The guard page below `USER_STACK_FLOOR` is never mapped.
//!
//! Build with `userland/build.sh`; the kernel embeds the result as
//! `STACK_OVER_ELF`.

#![no_std]
#![no_main]

core::arch::global_asm!(
    r#"
    .section .text.entry, "ax", @progbits
    .globl _start
_start:
    li      a0, 0                   /* depth */
    call    descend

    /* Unreachable while the cap holds: a run that gets here means growth went
       past 1 MiB, so exit with a distinctive code instead of claiming success. */
    li      a0, 111
    li      a7, 93
    ecall
1:  j       1b

/* descend(depth): 300 levels x 4 KiB = 1.2 MiB, which is past the cap; the
   store into the page below USER_STACK_FLOOR is the fault this fixture exists
   to produce. */
descend:
    /* 4 KiB does not fit `addi`'s 12-bit immediate. */
    li      t0, 4096
    sub     sp, sp, t0
    sd      ra, 8(sp)
    sd      a0, 16(sp)
    sd      a0, 0(sp)
    li      t0, 300
    bgeu    a0, t0, 2f
    addi    a0, a0, 1
    call    descend
2:
    ld      ra, 8(sp)
    ld      a0, 16(sp)
    li      t0, 4096
    add     sp, sp, t0
    ret
"#
);

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
