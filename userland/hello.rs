//! The M6b acceptance program: a static RV64 ELF that prints one line, yields
//! the CPU once, and exits with code 42.
//!
//! All of it is assembly on purpose: the point is to exercise the kernel's
//! user-mode entry and its syscall ABI, not the Rust runtime. The RISC-V Linux
//! ABI this kernel implements is the same one: syscall number in `a7`, arguments
//! in `a0..a2`, result back in `a0`.
//!
//! Build with `userland/build.sh` (see `userland/README.md`).

#![no_std]
#![no_main]

core::arch::global_asm!(
    r#"
    .section .text.entry, "ax", @progbits
    .globl _start
_start:
    /* write(1, message, message_end - message) */
    li      a0, 1
    la      a1, message
    li      a2, message_end - message
    li      a7, 64
    ecall

    /* sched_yield(): proves that a voluntary switch out of U-mode and back
       works, not only the timer path. */
    li      a7, 124
    ecall

    /* Touch .data and .bss so the image has two PT_LOAD segments: the loader
       must map both, and the fault fixture reuses the same layout. */
    la      t0, data_word
    ld      t1, 0(t0)
    la      t0, bss_word
    ld      t2, 0(t0)
    add     a0, t1, t2

    /* exit(42) */
    li      a0, 42
    li      a7, 93
    ecall

    /* exit(2) never returns; park if it somehow does. */
1:  j       1b

    .section .rodata, "a", @progbits
message:
    .ascii  "hello from user mode\n"
message_end:

    .section .data, "aw", @progbits
    .align 3
data_word:
    .dword  7

    .section .bss, "aw", @nobits
    .align 3
bss_word:
    .zero   8
"#
);

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
