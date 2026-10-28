//! A user program that must be killed by the kernel, not the machine: it loads
//! from virtual address 0, which the loader never maps (the user window starts
//! at 0x1_0000). The trap handler must print one diagnostic, end this task and
//! return to the shell with the machine still running.
//!
//! Build with `userland/build.sh`; the kernel embeds the resulting ELF as its
//! `run` fault fixture and its `SELFTEST user-fault` case.

#![no_std]
#![no_main]

core::arch::global_asm!(
    r#"
    .section .text.entry, "ax", @progbits
    .globl _start
_start:
    /* getpid(): prove the U-mode entry and a syscall round-trip first. */
    li      a7, 172
    ecall

    /* Load from 0: unmapped, so scause = 13 (load page fault). */
    li      t0, 0
    ld      a0, 0(t0)

1:  j       1b

    .section .data, "aw", @progbits
    .align 3
    .dword  0x1234

    .section .bss, "aw", @nobits
    .align 3
    .zero   8
"#
);

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
