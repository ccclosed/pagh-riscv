//! Typed access to the CSRs this kernel uses.
//!
//! Every helper is one instruction. The names are the architectural ones so a
//! reader can look them up in the RISC-V privileged spec without translating.

use core::arch::asm;

macro_rules! read_csr {
    ($name:ident, $csr:literal) => {
        /// Read the CSR (see the privileged spec for the field layout).
        #[inline]
        pub fn $name() -> usize {
            let value: usize;
            // SAFETY: reading a CSR cannot fault from S-mode for the CSRs used
            // here, and it has no memory side effects.
            unsafe { asm!(concat!("csrr {}, ", $csr), out(reg) value, options(nomem, nostack)) };
            value
        }
    };
}

macro_rules! write_csr {
    ($name:ident, $csr:literal) => {
        /// Write the CSR.
        #[inline]
        pub fn $name(value: usize) {
            // SAFETY: writing a CSR has no memory side effects; the values passed
            // here are built by this module's callers from the spec's layout.
            unsafe { asm!(concat!("csrw ", $csr, ", {}"), in(reg) value, options(nomem, nostack)) };
        }
    };
}

read_csr!(read_mhartid, "mhartid");
read_csr!(read_scause, "scause");
read_csr!(read_sepc, "sepc");
read_csr!(read_stval, "stval");
read_csr!(read_sstatus, "sstatus");
read_csr!(read_sie, "sie");
read_csr!(read_time, "time");
write_csr!(write_stvec, "stvec");
write_csr!(write_sscratch, "sscratch");
write_csr!(write_sie, "sie");
write_csr!(write_sip, "sip");
write_csr!(write_satp, "satp");
write_csr!(write_sstatus, "sstatus");

/// Supervisor interrupt-enable bit in `sstatus`.
pub const SSTATUS_SIE: usize = 1 << 1;
/// Supervisor timer interrupt-enable bit in `sie`/`sip`.
pub const SIE_STIE: usize = 1 << 5;
/// Supervisor external interrupt-enable bit in `sie`/`sip`.
pub const SIE_SEIE: usize = 1 << 9;

/// Enable or disable interrupts on this hart, returning whether they were on.
///
/// The kernel's critical sections depend on this being exact: a lock taken with
/// interrupts enabled would let a trap handler take the same lock.
#[inline]
pub fn interrupts_enabled() -> bool {
    read_sstatus() & SSTATUS_SIE != 0
}

/// Run `f` with interrupts disabled, restoring the previous state afterwards.
#[inline]
pub fn without_interrupts<R>(f: impl FnOnce() -> R) -> R {
    let was_enabled = interrupts_enabled();
    disable_interrupts();
    let out = f();
    if was_enabled {
        enable_interrupts();
    }
    out
}

#[inline]
pub fn disable_interrupts() {
    // SAFETY: `csrci` clears a bit in `sstatus`; no memory effects.
    unsafe { asm!("csrci sstatus, 2", options(nomem, nostack)) };
}

#[inline]
pub fn enable_interrupts() {
    // SAFETY: `csrsi` sets a bit in `sstatus`; no memory effects.
    unsafe { asm!("csrsi sstatus, 2", options(nomem, nostack)) };
}
