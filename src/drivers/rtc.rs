//! Goldfish RTC — the machine's wall clock.
//!
//! QEMU's `virt` machine declares a `google,goldfish-rtc` node at
//! `rtc@101000` (the address comes from the device tree, never from a constant
//! here). The device is a plain MMIO 64-bit nanosecond counter since the Unix
//! epoch, latched by reading the low word first — which is exactly the shape a
//! kernel that wants "what time is it" needs, and the reason TLS certificate
//! validity windows can be checked at all on this port.
//!
//! Why this exists: the x86-64 sibling reads the CMOS RTC through its Linux
//! compat layer; RISC-V `virt` has no CMOS. Without a wall clock the TLS
//! verifier's clock gate ([`crate::net::tls_chain::CLOCK_FLOOR`]) would refuse
//! every handshake — correctly, because "I do not know what time it is" must
//! never validate a certificate — and the port would be unable to authenticate
//! anything. A machine whose tree has no RTC keeps that fail-closed behaviour:
//! [`now_unix`] returns `None` and the handshake is refused.
//!
//! The registers (Android goldfish spec, implemented by
//! `hw/rtc/goldfish_rtc.c`):
//!
//! | offset | name |
//! |---|---|
//! | `0x00` | `TIME_LOW` (read: latches `TIME_HIGH`) |
//! | `0x04` | `TIME_HIGH` |
//! | `0x08`/`0x0c` | `ALARM_LOW`/`HIGH` |
//! | `0x10` | `IRQ_ENABLED` |
//! | `0x14` | `CLEAR_ALARM` |
//! | `0x18` | `ALARM_STATUS` |
//! | `0x1c` | `CLEAR_INTERRUPT` |
//! | `0x20`/`0x24` | `READ_TIME_LOW`/`HIGH` (latched read) |
//!
//! This driver never enables an interrupt: the kernel polls, and a periodic
//! alarm would be a second time source disagreeing with the SBI timer. Only
//! the read path is implemented — a clock the kernel never writes is a clock
//! whose value is still the host's.

use core::sync::atomic::{AtomicUsize, Ordering};

/// `TIME_LOW`: reading it latches the high word into `TIME_HIGH`.
const TIME_LOW: usize = 0x00;
/// `TIME_HIGH`: the latched upper 32 bits.
const TIME_HIGH: usize = 0x04;

/// Base address of the RTC window, or 0 when the device tree had none.
static BASE: AtomicUsize = AtomicUsize::new(0);

/// Register the RTC window found in the device tree. Called once, before the
/// shell or the network can ask for the time; a second call is ignored so the
/// first (tree-derived) address wins.
pub fn init(base: usize) {
    let _ = BASE.compare_exchange(0, base, Ordering::SeqCst, Ordering::SeqCst);
}

/// Whether an RTC window is present and mapped.
pub fn present() -> bool {
    BASE.load(Ordering::Relaxed) != 0
}

/// One 32-bit MMIO read.
fn read_u32(offset: usize) -> u32 {
    let base = BASE.load(Ordering::Relaxed);
    // SAFETY: `base` is the `reg` address of the `google,goldfish-rtc` node,
    // mapped read-only as device memory by `memory::paging::init` before any
    // caller can reach this function (the same contract `uart` and
    // `virtio_mmio` rely on). The offsets are the device's own register map,
    // all inside the mapped page, and every access is volatile because the
    // value is a live counter, not memory.
    unsafe { core::ptr::read_volatile((base + offset) as *const u32) }
}

/// Nanoseconds since the Unix epoch, or `None` when the machine has no RTC.
///
/// The low word is read first, which latches the high word: a read that
/// straddled a 32-bit carry would otherwise combine a stale high half with a
/// fresh low half and jump a whole 2^32 ns (≈4.3 s). The latch is the device's
/// own contract, not an assumption about how fast we read.
pub fn now_nanos() -> Option<u64> {
    if !present() {
        return None;
    }
    let low = read_u32(TIME_LOW) as u64;
    let high = read_u32(TIME_HIGH) as u64;
    Some((high << 32) | low)
}

/// Seconds since the Unix epoch, or `None` without an RTC.
pub fn now_unix() -> Option<u64> {
    now_nanos().map(|nanos| nanos / 1_000_000_000)
}

/// Raw dump of the RTC register page, for the diagnosis of a clock that reads
/// as nonsense. Returns the first eight words starting at `offset`.
pub fn dump() -> [u32; 8] {
    let mut out = [0u32; 8];
    if !present() {
        return out;
    }
    for (index, slot) in out.iter_mut().enumerate() {
        *slot = read_u32(index * 4);
    }
    out
}

/// Convert Unix seconds to `(year, month, day, hour, minute, second)` (UTC).
///
/// Howard Hinnant's `civil_from_days`, the inverse of the `days_from_civil`
/// used by the X.509 decoder — the same calendar math, so a validity window is
/// compared against a time rendered by the same rules it was parsed with.
pub fn civil_from_unix(seconds: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (seconds / 86_400) as i64;
    let secs_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    (
        year,
        month,
        day,
        (secs_of_day / 3600) as u32,
        ((secs_of_day % 3600) / 60) as u32,
        (secs_of_day % 60) as u32,
    )
}

/// Days since 1970-01-01 to a civil date (UTC), after Howard Hinnant.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}
