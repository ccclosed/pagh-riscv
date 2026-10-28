//! The M6b timer-preemption fixture: a U-mode program that runs for longer than
//! the 100 Hz tick period, so the timer interrupt is guaranteed to be taken
//! while the hart is in U-mode, and then exits with code 7.
//!
//! The kernel's `trap_dispatch` counts timer interrupts taken with
//! `sstatus.SPP == 0`; `SELFTEST user-timer-preempts` asserts that this run
//! moved that counter. Without the fixture, `hello` is too short to prove the
//! U-mode timer path — only the `ecall` and page-fault paths would be covered.
//!
//! `rdtime` from U-mode is allowed because OpenSBI sets `mcounteren.TM`; the
//! deadline is 0.2 s of the 10 MHz timebase, i.e. twenty tick periods.
//!
//! Build with `userland/build.sh`; the kernel embeds the result as `SPIN_ELF`.

#![no_std]
#![no_main]

core::arch::global_asm!(
    r#"
    .section .text.entry, "ax", @progbits
    .globl _start
_start:
    /* t0 = the timebase value at entry. */
    csrr    t0, time
1:
    /* Spin until 0.2 s have passed: 100 Hz means ~20 timer interrupts, all of
       them taken in U-mode between the two `rdtime`s. */
    csrr    t1, time
    sub     t2, t1, t0
    li      t3, 2000000
    bltu    t2, t3, 1b

    /* One syscall after the timer storm: the return-to-U path must still work. */
    li      a7, 172
    ecall

    li      a0, 7
    li      a7, 93
    ecall

2:  j       2b
"#
);

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
