//! The demand-grown-stack acceptance fixture: a call chain that uses more than
//! the 64 KiB initial user stack window and still returns.
//!
//! Each `descend` level takes one whole 4 KiB frame and *stores* into the bottom
//! word of it, so the run provably touches every page it claims: the store is
//! what faults on the first page below the initially mapped window and makes the
//! kernel grow the stack (`SELFTEST user-stack-grows` asserts the growth
//! counters moved by at least the 16 pages the initial window does not cover).
//!
//! `TARGET = 40` levels is 160 KiB of frames — about 96 KiB of demand-grown stack
//! below the initial 64 KiB, comfortably inside the documented 1 MiB cap. The
//! kernel maps the missing pages on the fault and retries the store, so the
//! recursion completes; without demand growth the very first crossing of
//! `USER_STACK_BASE` would kill the program with a store page fault.
//!
//! All assembly, like `hello`/`spin`: the point is the kernel's stack-growth
//! path, not a Rust runtime, and a hand-written chain cannot be turned into a
//! loop by the optimiser.
//!
//! Build with `userland/build.sh`; the kernel embeds the result as `STACK_ELF`.
//!
//! # ABI
//!
//! `write(1, ...)` is syscall 64 (`a0` = fd, `a1` = buffer, `a2` = length) and
//! `exit(code)` is syscall 93 — the same RISC-V Linux ABI the rest of
//! `userland/` uses.

#![no_std]
#![no_main]

core::arch::global_asm!(
    r#"
    .section .text.entry, "ax", @progbits
    .globl _start
_start:
    li      a0, 0                   /* depth */
    call    descend

    /* Reaching here means every frame in the chain was mapped and usable:
       print the proof and exit 0. */
    li      a0, 1
    la      a1, message
    li      a2, message_end - message
    li      a7, 64
    ecall

    li      a0, 0
    li      a7, 93
    ecall
1:  j       1b

/* descend(depth): one 4 KiB frame per level, the frame's bottom word touched
   with a real store — the access that faults when the chain leaves the mapped
   window and the kernel has to grow the stack. 40 levels is 160 KiB, i.e.
   beyond the initial 64 KiB and inside the 1 MiB cap. */
descend:
    /* 4 KiB does not fit `addi`'s 12-bit immediate. */
    li      t0, 4096
    sub     sp, sp, t0
    sd      ra, 8(sp)
    sd      a0, 16(sp)
    sd      a0, 0(sp)
    li      t0, 40
    bgeu    a0, t0, 2f
    addi    a0, a0, 1
    call    descend
2:
    ld      ra, 8(sp)
    ld      a0, 16(sp)
    li      t0, 4096
    add     sp, sp, t0
    ret

    .section .rodata, "a", @progbits
message:
    .ascii  "stack: deep recursion ok\n"
message_end:
"#
);

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
