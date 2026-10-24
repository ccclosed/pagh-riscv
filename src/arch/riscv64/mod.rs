//! RV64: entry, CSRs, traps.
//!
//! Privilege model, because everything else is easier to read with it in mind:
//! OpenSBI runs in M-mode and hands control to us in **S-mode**. We never touch
//! M-mode CSRs (`mstatus`, `mtimecmp`, …); the timer is programmed through
//! SBI calls, and traps that arrive while we are in S-mode are the only ones we
//! handle (`stvec`).

pub mod boot;
pub mod csr;
pub mod traps;

/// Stop this hart forever. `wfi` lets the emulator skip the hart's time slice,
/// which is what keeps an idle kernel from burning a core.
pub fn park_forever() -> ! {
    loop {
        // SAFETY: `wfi` is a hint; it cannot fault and needs no state set up.
        unsafe { core::arch::asm!("wfi", options(nomem, nostack)) };
    }
}

/// The hart's id (`mhartid` is readable from S-mode on RISC-V).
pub fn hart_id() -> usize {
    csr::read_mhartid()
}
