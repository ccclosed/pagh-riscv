//! The platform-level interrupt controller (PLIC): device lines to `scause` 9.
//!
//! The kernel needs exactly one fact from the controller today — "the UART's
//! receive line is source N, and it is routed to this hart's S-mode context" —
//! but getting it wrong is silent: the UART stays quiet, the console falls back
//! to polling, and the boot log is the only place the mistake shows. So the base
//! and the source number come from the device tree ([`crate::fdt::Fdt::plic`],
//! [`crate::fdt::Fdt::uart_interrupt`]), never from a constant.
//!
//! # What a context is
//!
//! A PLIC context is one hart at one privilege level, numbered in hardware order:
//! context `2 * hart` is hart `hart` in M-mode, context `2 * hart + 1` the same
//! hart in S-mode. The kernel runs on hart 0 in S-mode, so it owns context 1;
//! the number is derived from [`crate::arch::riscv64::hart_id`] rather than
//! hard-coded, so the day SMP arrives the derivation is already right. (The
//! device tree does not encode PLIC context numbers — its `interrupts-extended`
//! names the CPU's interrupt wires, not the controller's contexts.)
//!
//! # The handler's contract
//!
//! [`handle_external_interrupt`] runs from the trap handler, with interrupts
//! masked, while the trap frame is live. It therefore prints nothing, allocates
//! nothing and takes no kernel lock — the interrupted context may hold any of
//! them. The claim/complete loop is bounded ([`MAX_CLAIMS`]) so a controller that
//! keeps claiming cannot pin the hart. A source with no handler is disabled
//! (rather than "completed" forever) so a stray level-triggered line cannot turn
//! into an interrupt storm; the counters below, surfaced by the shell's `time`
//! command and the selftest, are the diagnostic record.
//!
//! One deliberate omission: nothing here writes the PLIC's priority registers
//! for sources it does not route, and nothing configures the M-mode contexts —
//! firmware owns those.

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use crate::arch::riscv64::csr;

/// Priority registers: one 32-bit word per source (source 0 is reserved).
const PRIORITY_BASE: usize = 0x000_0000;
/// Per-context enable bits: 0x80 bytes (32 words) per context.
const ENABLE_BASE: usize = 0x000_2000;
const ENABLE_STRIDE: usize = 0x80;
/// Per-context threshold (word 0) and claim/complete (word 1) registers.
const CONTEXT_BASE: usize = 0x0020_0000;
const CONTEXT_STRIDE: usize = 0x1000;

/// Priority programmed for every source the kernel routes. QEMU's PLIC delivers a
/// source only if its priority is strictly above the context threshold; a
/// threshold of 0 and a priority of 1 is the minimum that is definitely routed.
const SOURCE_PRIORITY: u32 = 1;

/// Highest source id a PLIC may report (the binding allows 1..=1023).
const MAX_SOURCE: usize = 1023;

/// Claim/complete rounds one trap may perform. The UART's FIFO is drained inside
/// each round, so the bound is "how many devices one trap is willing to service",
/// not a byte count; a level-triggered line that is still asserted simply causes
/// another trap after this one returns.
const MAX_CLAIMS: usize = 8;

/// The PLIC register windows this driver touches, as `(offset, length)` from the
/// controller base. They are mapped by the boot path ([`crate::memory::paging`])
/// and are deliberately *not* "the whole 0xc000000..0xc400000 region": only the
/// priority block, the enable block and this hart's context page are ever
/// addressed.
///
/// The context window is the one that cannot be a constant: context `n`'s
/// threshold and claim/complete registers live at
/// `CONTEXT_BASE + n * CONTEXT_STRIDE`, so mapping `CONTEXT_BASE` alone maps
/// *firmware's* M-mode context 0 and faults the first threshold write to the
/// S-mode context 1 — the `store page fault (stval=0xc201000)` the first M3 boot
/// produced. The window is therefore derived from [`context`].
pub fn windows() -> [(usize, usize); 3] {
    [
        (PRIORITY_BASE, 0x1000), // priority words for sources 1..=1023
        (ENABLE_BASE, 0x1000),   // enable bits, contexts 0..=63 (S-mode hart 0 = 1)
        (CONTEXT_BASE + context() * CONTEXT_STRIDE, 0x1000), // threshold + claim/complete
    ]
}

/// PLIC base address as discovered at boot (`0` means "not routed").
static BASE: AtomicUsize = AtomicUsize::new(0);
/// The interrupt source that carries the console UART (`0` means "none").
static UART_IRQ: AtomicUsize = AtomicUsize::new(0);
/// Whether `init` completed and `sie.SEIE` was raised.
static ROUTED: AtomicBool = AtomicBool::new(false);
/// Sources claimed since boot.
static CLAIMS: AtomicU64 = AtomicU64::new(0);
/// Claims completed since boot (normally equal to [`CLAIMS`]).
static COMPLETIONS: AtomicU64 = AtomicU64::new(0);
/// Claims for sources with no handler; each one is disabled.
static UNKNOWN: AtomicU64 = AtomicU64::new(0);

/// Why the PLIC could not be configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlicError {
    /// The device tree reported no base (`0` cannot be a PLIC).
    NoBase,
    /// The UART's source number is outside the controller's range.
    BadSource(usize),
}

impl core::fmt::Display for PlicError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            PlicError::NoBase => write!(f, "the device tree reports no PLIC base"),
            PlicError::BadSource(source) => {
                write!(
                    f,
                    "UART interrupt source {source} is outside 1..={MAX_SOURCE}"
                )
            }
        }
    }
}

/// The PLIC context this hart owns: S-mode hart `n` is context `2n + 1`.
pub fn context() -> usize {
    2 * crate::arch::riscv64::hart_id() + 1
}

/// Route `uart_irq` from the PLIC at `base` to this hart's S-mode context.
///
/// Programs the source priority and the context threshold, sets the enable bit,
/// and only then raises `sie.SEIE` — a source enabled before its vector is ready
/// is an interrupt into an empty handler. The UART driver enables its own IER bit
/// after this returns ([`crate::drivers::uart::enable_rx_interrupt`]).
pub fn init(base: usize, uart_irq: usize) -> Result<(), PlicError> {
    if base == 0 {
        return Err(PlicError::NoBase);
    }
    if uart_irq == 0 || uart_irq > MAX_SOURCE {
        return Err(PlicError::BadSource(uart_irq));
    }
    let ctx = context();
    BASE.store(base, Ordering::Relaxed);
    UART_IRQ.store(uart_irq, Ordering::Relaxed);

    // Priority above the threshold, but only for the one line we route.
    write32(base + PRIORITY_BASE + 4 * uart_irq, SOURCE_PRIORITY);
    // Threshold 0: accept every source with a non-zero priority.
    write32(base + CONTEXT_BASE + ctx * CONTEXT_STRIDE, 0);
    // Enable bit, read-modify-write because 31 other sources share the word.
    let enable = base + ENABLE_BASE + ctx * ENABLE_STRIDE + (uart_irq / 32) * 4;
    write32(enable, read32(enable) | (1 << (uart_irq % 32)));
    // The device reads these registers from the bus, not from the CPU's store
    // buffer: on real hardware the enable could otherwise be sampled before the
    // priority/threshold writes land. Under QEMU this is free.
    core::sync::atomic::fence(Ordering::SeqCst);

    ROUTED.store(true, Ordering::Release);
    csr::write_sie(csr::read_sie() | csr::SIE_SEIE);
    Ok(())
}

/// Service the supervisor external interrupt: claim, dispatch, complete.
///
/// Bounded and silent by construction (see the module docs). A claim of `0` means
/// "no source", which is how a spurious line ends without work.
pub fn handle_external_interrupt() {
    let base = BASE.load(Ordering::Relaxed);
    if base == 0 {
        return;
    }
    let ctx = context();
    let claim = base + CONTEXT_BASE + ctx * CONTEXT_STRIDE + 4;
    let uart_irq = UART_IRQ.load(Ordering::Relaxed);
    let mut rounds = 0;
    while rounds < MAX_CLAIMS {
        let source = read32(claim) as usize;
        if source == 0 || source > MAX_SOURCE {
            break;
        }
        rounds += 1;
        CLAIMS.fetch_add(1, Ordering::Relaxed);
        if source == uart_irq {
            crate::drivers::uart::handle_interrupt();
        } else {
            // No handler: disabling the enable bit is what stops a level-
            // triggered source from re-entering this path forever. The counter
            // (not a print — this is trap context) records that it happened.
            let enable = base + ENABLE_BASE + ctx * ENABLE_STRIDE + (source / 32) * 4;
            write32(enable, read32(enable) & !(1 << (source % 32)));
            UNKNOWN.fetch_add(1, Ordering::Relaxed);
        }
        // Complete even an unknown source: it was claimed, and the controller
        // will not deliver it again until it is completed.
        write32(claim, source as u32);
        COMPLETIONS.fetch_add(1, Ordering::Relaxed);
    }
}

/// Whether a PLIC line is configured and `sie.SEIE` is raised.
pub fn routed() -> bool {
    ROUTED.load(Ordering::Acquire)
}

/// The routed console UART source number (`0` when the console is polled).
pub fn uart_irq() -> usize {
    UART_IRQ.load(Ordering::Relaxed)
}

/// The discovered PLIC base (`0` when external interrupts are not routed).
pub fn base() -> usize {
    BASE.load(Ordering::Relaxed)
}

/// `(claims, completions, unknown_sources)` since boot, for the report lines.
pub fn stats() -> (u64, u64, u64) {
    (
        CLAIMS.load(Ordering::Relaxed),
        COMPLETIONS.load(Ordering::Relaxed),
        UNKNOWN.load(Ordering::Relaxed),
    )
}

/// Selftest: the UART line is routed, and the *hardware* agrees.
///
/// Reads the enable bit back out of the controller instead of trusting the flag
/// [`init`] set: a base address that was wrong, a context number that was wrong or
/// a missed store would all look configured from the inside and deliver nothing.
///
/// On a machine with no PLIC (or no UART interrupt) the designed fallback is a
/// polled console, and that is what this reports; the check passes because the
/// kernel's chosen state is the fallback, and it says so on the serial line.
pub fn selftest_uart_line() -> bool {
    let base = BASE.load(Ordering::Relaxed);
    let irq = UART_IRQ.load(Ordering::Relaxed);
    if base == 0 || irq == 0 {
        crate::kprintln!("[selftest] plic: no PLIC/UART interrupt routed — the console is polled");
        return true;
    }
    let ctx = context();
    let enable = base + ENABLE_BASE + ctx * ENABLE_STRIDE + (irq / 32) * 4;
    let bit = read32(enable) & (1 << (irq % 32)) != 0;
    if !bit {
        crate::kprintln!(
            "[selftest] plic: base {base:#x} context {ctx} source {irq} is not enabled"
        );
    }
    bit
}

#[inline]
fn read32(address: usize) -> u32 {
    // SAFETY: every caller passes one of the [`windows`] register addresses — an MMIO register the boot
    // page table maps read/write for the kernel. A 32-bit volatile load is the
    // width the PLIC specification defines.
    unsafe { core::ptr::read_volatile(address as *const u32) }
}

#[inline]
fn write32(address: usize, value: u32) {
    // SAFETY: as above; device memory, no aliasing with kernel RAM.
    unsafe { core::ptr::write_volatile(address as *mut u32, value) }
}
