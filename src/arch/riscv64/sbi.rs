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
