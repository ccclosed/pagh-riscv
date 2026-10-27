//! RV64: entry, CSRs, traps.
//!
//! Privilege model, because everything else is easier to read with it in mind:
//! OpenSBI runs in M-mode and hands control to us in **S-mode**. We never touch
//! M-mode CSRs (`mstatus`, `mtimecmp`, …); the timer is programmed through
//! SBI calls, and traps that arrive while we are in S-mode are the only ones we
//! handle (`stvec`).

pub mod boot;
pub mod csr;
pub mod mmu;
pub mod plic;
pub mod sbi;
pub mod selftest;
pub mod timer;
pub mod traps;

/// Stop this hart forever. `wfi` lets the emulator skip the hart's time slice,
/// which is what keeps an idle kernel from burning a core.
pub fn park_forever() -> ! {
    loop {
        // SAFETY: `wfi` is a hint; it cannot fault and needs no state set up.
        unsafe { core::arch::asm!("wfi", options(nomem, nostack)) };
    }
}

/// The hart the kernel is running on.
///
/// `mhartid` is an **M-mode** CSR: reading it from S-mode is an illegal
/// instruction, which OpenSBI does not emulate (the `hart` shell command used to
/// halt the machine with `scause=2`). The id is therefore captured once, in
/// `_start`, while it is still in `a0` — see [`boot::BOOT_HART_ID`].
pub fn hart_id() -> usize {
    // SAFETY: written exactly once by `_start` before Rust runs, and only read
    // afterwards; no other context can be mid-write.
    unsafe { boot::BOOT_HART_ID }
}
