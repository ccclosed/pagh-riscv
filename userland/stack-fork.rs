//! The fork-with-a-grown-stack fixture: the deep chain of `stack.rs` forks at
//! depth 30, when roughly 14 pages below the initial 64 KiB window are already
//! demand-grown, and both processes walk the rest of the chain on their own copy
//! of it.
//!
//! This is the check `user-stack-grows` cannot make. Growth itself is a kernel
//! fault path; the *copy* of a grown stack is `copy_grown_stack` in `fork`, and
//! the teardown is the same `stack_low` walk through `wait4` that the runner
//! uses. The parent grows first, forks, waits for the child and exits 0; the
//! child (role in `s3`) finishes its own chain — growing the pages its copy did
//! not have — prints `stack: fork child ok` and exits 0. If `fork` copied only
//! the initial 64 KiB, the child's next store below that window would fault and
//! kill it; the parent's `wait4` would return a fault status, not a pid, and the
//! parent exits 9.
//!
//! `s2`/`s3` are callee-saved and untouched by `descend`, so they survive the
//! syscalls and the recursion; they are zeroed before the chain starts because
//! nothing in the user entry ABI promises them.
//!
//! Build with `userland/build.sh`; the kernel embeds the result as
//! `STACK_FORK_ELF`.
//!
//! # ABI
//!
//! `write(1, ...)` is 64, `exit(code)` is 93, `fork` is the pagh-native 1000
//! and `wait4(pid, *status, options, rusage)` is 260 — the same numbers the rest
//! of `userland/` uses.

#![no_std]
#![no_main]

core::arch::global_asm!(
    r#"
    .section .text.entry, "ax", @progbits
    .globl _start
_start:
    li      s2, 0                   /* child pid (parent) */
    li      s3, 0                   /* role: 0 = parent, 1 = child */
    li      a0, 0                   /* depth */
    call    descend

    beqz    s3, parent

    /* Child: the chain below the fork point was copied by `fork`; reaching
       here means every frame was usable on this process's own copy. */
    li      a0, 1
    la      a1, child_message
    li      a2, child_message_end - child_message
    li      a7, 64
    ecall
    li      a0, 0
    li      a7, 93
    ecall

parent:
    /* wait4(s2, 0, 0, 0): reap the child and return its pid, not a fault. */
    mv      a0, s2
    li      a1, 0
    li      a2, 0
    li      a3, 0
    li      a7, 260
    ecall
    bne     a0, s2, fail

    li      a0, 1
    la      a1, parent_message
    li      a2, parent_message_end - parent_message
    li      a7, 64
    ecall
    li      a0, 0
    li      a7, 93
    ecall

fail:
    li      a0, 9                   /* fork/wait failed or the child faulted */
    li      a7, 93
    ecall
1:  j       1b

/* descend(depth): one 4 KiB frame per level. At depth 30 — inside the
   demand-grown part of the stack — fork once; both processes then continue the
   chain, the child on its copied pages. */
descend:
    li      t0, 4096
    sub     sp, sp, t0
    sd      ra, 8(sp)
    sd      a0, 16(sp)
    sd      a0, 0(sp)

    li      t0, 30
    bne     a0, t0, 3f
    li      a7, 1000                /* fork */
    ecall
    bltz    a0, fail                /* fork error: exit 9 */
    bnez    a0, 2f
    li      s3, 1                   /* child */
    j       3f
2:
    /* Parent remembers the pid to wait for. */
    mv      s2, a0
3:
    ld      a0, 16(sp)              /* depth again: fork clobbered a0 */
    li      t0, 40
    bgeu    a0, t0, 4f
    addi    a0, a0, 1
    call    descend
4:
    ld      ra, 8(sp)
    ld      a0, 16(sp)
    li      t0, 4096
    add     sp, sp, t0
    ret

    .section .rodata, "a", @progbits
parent_message:
    .ascii  "stack: fork parent ok\n"
parent_message_end:
child_message:
    .ascii  "stack: fork child ok\n"
child_message_end:
"#
);

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
