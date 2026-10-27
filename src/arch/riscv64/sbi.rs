//! The Supervisor Binary Interface calls this kernel makes.
//!
//! Everything in M-mode — powering the machine off, programming the timer — is
//! reached with `ecall` from S-mode. OpenSBI owns those calls; the kernel never
//! touches an M-mode CSR itself (see `boot::BOOT_HART_ID` for what that costs
//! when it is forgotten).

/// Extension ID for the System Reset extension ("SRST" as ASCII).
const EXT_SRST: usize = 0x5352_5354;
/// Function 0 of SRST: `system_reset(reset_type, reset_reason)`.
const FID_SYSTEM_RESET: usize = 0;
/// `reset_type` 0 = shutdown, 1 = cold reboot.
const RESET_TYPE_SHUTDOWN: usize = 0;
/// `reset_reason` 0 = no reason given.
const RESET_REASON_NONE: usize = 0;

/// Extension ID for the Timer extension ("TIME" as ASCII).
///
/// Not the legacy EID `0x00`: that one could only clear/set a timer and returned
/// nothing useful, and OpenSBI v1.7 implements the v0.2+ extension below.
const EXT_TIME: usize = 0x5449_4D45;
/// Function 0 of TIME: `set_timer(stime_value)`.
const FID_SET_TIMER: usize = 0;

/// A non-zero error code returned by firmware (SBI v0.2+ returns `(error, value)`
/// in `a0`/`a1`; the error is a negative `long`, so any non-zero value is a
/// failure). The raw value is kept instead of decoded into an enum because the
/// kernel only ever needs "did it work", and a wrong guess about which of
/// `SBI_ERR_FAILED`/`_NOT_SUPPORTED`/`_INVALID_PARAM` came back would be a lie
/// dressed as detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SbiError(pub usize);

impl core::fmt::Display for SbiError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "SBI error {} ({:#x})", self.0 as isize, self.0)
    }
}

/// The `time` CSR: a counter that increments at the device tree's timebase
/// frequency. Reading it is an ordinary S-mode CSR read; the *compare* value is
/// M-mode, which is why [`set_timer`] exists.
#[inline]
pub fn time() -> usize {
    crate::arch::riscv64::csr::read_time()
}

/// Schedule the next supervisor timer interrupt at absolute time `stime_value`.
///
/// `stime_value` is in `time` units (compare against [`time`]); firmware owns the
/// compare register, so this is the only way to arm the timer from S-mode.
/// Passing a value in the past makes the interrupt pending immediately — callers
/// that mean "one period from now" compute the deadline from the *previous*
/// deadline, not from "now", so the clock does not drift.
pub fn set_timer(stime_value: usize) -> Result<(), SbiError> {
    let mut error: usize;
    let mut value: usize;
    // SAFETY: `ecall` from S-mode enters firmware for the TIME extension, which
    // either programs the compare value or returns an error code; it does not
    // touch our memory. The asm contract matches SBI v0.2+: EID in a7, FID in a6,
    // parameters from a0, results (error, value) in a0/a1.
    unsafe {
        core::arch::asm!(
            "ecall",
            in("a7") EXT_TIME,
            in("a6") FID_SET_TIMER,
            inlateout("a0") stime_value => error,
            lateout("a1") value,
            options(nostack)
        )
    };
    let _ = value;
    if error == 0 {
        Ok(())
    } else {
        Err(SbiError(error))
    }
}

/// Power the machine off through firmware and never return.
///
/// This is what makes the `halt` command mean what it says: parking the hart in
/// `wfi` leaves QEMU running until something kills it, because a parked hart is
/// still a live machine. A machine without SRST (very old firmware) simply never
/// comes back from the `ecall`, which is why the caller prints its message
/// first.
pub fn shutdown() -> ! {
    // SAFETY: `ecall` from S-mode enters M-mode firmware, which either powers
    // the machine off or returns an error code; `options(noreturn)` is a promise
    // that this function does not fall through, and `kmain` never resumes after
    // a successful shutdown anyway.
    unsafe {
        core::arch::asm!(
            "ecall",
            in("a7") EXT_SRST,
            in("a6") FID_SYSTEM_RESET,
            in("a0") RESET_TYPE_SHUTDOWN,
            in("a1") RESET_REASON_NONE,
            options(noreturn)
        )
    }
}
