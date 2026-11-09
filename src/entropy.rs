//! Kernel entropy pool — the seed behind every TLS session key.
//!
//! # Where the randomness comes from
//!
//! RISC-V `virt` has no RDSEED/RDRAND and (on the machines this port runs on)
//! no entropy CSR, so the x86-64 sibling's "fail closed without hardware
//! entropy" rule would refuse every handshake here. The machine does offer one
//! real source, and QEMU hands it over in the device tree:
//!
//!   * **`/chosen/rng-seed`** — a host-generated random seed QEMU places in the
//!     FDT at startup (the same mechanism Linux uses for early-boot entropy).
//!     This is the pool's root secret: it never leaves guest memory, it is
//!     produced by the host's CSPRNG, and 32 bytes of it is 256 bits of entropy
//!     — enough for any number of session keys. [`init`] refuses an empty seed.
//!   * **Timing jitter** — the RTC's nanosecond counter, the 10 MHz `time` CSR,
//!     the tick and timer-interrupt counters, and a stack address are folded
//!     into the seed as well. This is *not* a claim of extra entropy: it is
//!     defence in depth, so that a seed that was somehow truncated or constant
//!     still does not produce a constant pool.
//!
//! # What the pool is
//!
//! A SHA-256 hash ratchet. The seed is absorbed into a 32-byte state; each
//! 32-byte output block is `SHA256(state || counter)` and then the state is
//! re-keyed with `SHA256(state || 0x01 || block)`. Re-keying gives the usual
//! backtracking resistance: recovering a later state says nothing about earlier
//! outputs. This is a deliberately small construction, not a formally validated
//! DRBG (no HMAC-DRBG, no CTR-DRBG with a derivation function); it is the
//! strongest thing that fits the kernel's no-dependency, `panic = "abort"`
//! shape, and its security rests entirely on the 256-bit secret above.
//!
//! # Fail-closed
//!
//! [`is_available`] is `false` until [`init`] has absorbed a non-empty
//! `/chosen/rng-seed`. `net::tls::https_get` checks it *before* opening a
//! socket and refuses the fetch, exactly as the x86 kernel refuses without
//! RDSEED: a TLS client with predictable randomness is worse than no TLS
//! client, because it looks secure. There is no path that substitutes
//! "whatever the counters say" for the seed.

use crate::sync::Spinlock;
use sha2::{Digest, Sha256};

/// State size, and the block size of the generator (`SHA-256` output).
const STATE: usize = 32;
/// Most seed bytes absorbed from `/chosen/rng-seed` (QEMU's property is 32).
pub const SEED_MAX: usize = 64;

struct Pool {
    /// Ratchet state — the secret. Never handed out directly.
    state: [u8; STATE],
    /// Domain-separation counter for the next block.
    counter: u64,
    /// Bytes of device-tree seed absorbed at [`init`].
    seeded: usize,
    /// Whether the pool may be used by a security-sensitive caller.
    ready: bool,
    /// Output bytes handed out since boot (reported by the `entropy` command).
    out_bytes: u64,
}

impl Pool {
    const fn empty() -> Self {
        Pool {
            state: [0; STATE],
            counter: 0,
            seeded: 0,
            ready: false,
            out_bytes: 0,
        }
    }
}

static POOL: Spinlock<Pool> = Spinlock::new(Pool::empty());

/// One jitter sample: several independent clocks plus an address.
///
/// The value is fed to SHA-256, so its *distribution* does not matter — only
/// that it is not attacker-controlled. It is not accounted as entropy.
fn jitter() -> u64 {
    let time = crate::arch::riscv64::csr::read_time() as u64;
    let ticks = crate::arch::riscv64::timer::ticks();
    let interrupts = crate::arch::riscv64::timer::interrupts();
    let rtc = crate::drivers::rtc::now_nanos().unwrap_or(0);
    let here = &time as *const u64 as u64;
    time.rotate_left(13)
        ^ ticks.rotate_left(29)
        ^ interrupts.rotate_left(47)
        ^ rtc.rotate_left(7)
        ^ here.rotate_left(3)
}

/// Absorb `seed` (plus jitter) and mark the pool available.
///
/// Returns the number of seed bytes absorbed, or `0` when `seed` is empty — in
/// which case the pool stays **unavailable** and TLS refuses to run. Called
/// once from `kmain` with the device tree's `/chosen/rng-seed`.
pub fn init(seed: &[u8]) -> usize {
    if seed.is_empty() {
        return 0;
    }
    let take = seed.len().min(SEED_MAX);

    let mut hasher = Sha256::new();
    hasher.update(b"pagh-riscv entropy pool v1");
    hasher.update((take as u64).to_le_bytes());
    hasher.update(&seed[..take]);
    // 32 jitter samples, read back to back: the spread between them is host
    // scheduling and cache behaviour, which a remote peer cannot observe.
    for _ in 0..32 {
        hasher.update(jitter().to_le_bytes());
    }
    let digest = hasher.finalize();

    let mut pool = POOL.lock();
    pool.state.copy_from_slice(&digest);
    pool.counter = 0;
    pool.seeded = take;
    pool.ready = true;
    take
}

/// Discard the seed material. The pool is then unavailable until [`init`] runs
/// again; used by the boot selftest to prove the fail-closed gate, and by the
/// (test-only) "no entropy" path. A machine that has never been seeded is
/// already in this state, so this only ever narrows what the kernel will do.
pub fn clear() {
    let mut pool = POOL.lock();
    *pool = Pool::empty();
}

/// Whether a security-sensitive caller (TLS) may draw random bytes.
pub fn is_available() -> bool {
    POOL.lock().ready
}

/// How many bytes of device-tree seed were absorbed (`0` before [`init`]).
pub fn seeded_bytes() -> usize {
    POOL.lock().seeded
}

/// Output bytes produced since boot.
pub fn out_bytes() -> u64 {
    POOL.lock().out_bytes
}

/// Fill `dest` from the pool. Returns `false` (and writes nothing) when the
/// pool is unavailable, so a caller cannot accidentally use zeros as a key.
pub fn fill(dest: &mut [u8]) -> bool {
    let mut pool = POOL.lock();
    if !pool.ready {
        return false;
    }
    let mut written = 0usize;
    while written < dest.len() {
        let block = next_block(&mut pool);
        let take = (dest.len() - written).min(STATE);
        dest[written..written + take].copy_from_slice(&block[..take]);
        written += take;
    }
    pool.out_bytes += dest.len() as u64;
    true
}

/// One generator step: derive a block, then ratchet the state forward.
fn next_block(pool: &mut Pool) -> [u8; STATE] {
    let mut hasher = Sha256::new();
    hasher.update(pool.state);
    hasher.update(pool.counter.to_le_bytes());
    let block = hasher.finalize();
    pool.counter = pool.counter.wrapping_add(1);

    // Re-key: the new state commits to the old state and the block just
    // produced, so an attacker who learns a later state cannot run the ratchet
    // backwards to earlier outputs.
    let mut ratchet = Sha256::new();
    ratchet.update(pool.state);
    ratchet.update([0x01u8]);
    ratchet.update(block);
    let next = ratchet.finalize();
    pool.state.copy_from_slice(&next);

    let mut out = [0u8; STATE];
    out.copy_from_slice(&block);
    out
}

/// A one-line summary for the shell's `entropy` command and the boot log.
pub struct Status {
    /// Seed bytes absorbed from `/chosen/rng-seed`.
    pub seeded: usize,
    /// Output bytes produced since boot.
    pub out_bytes: u64,
    /// Whether TLS will be allowed to run.
    pub available: bool,
}

/// Snapshot the pool's non-secret counters.
pub fn status() -> Status {
    let pool = POOL.lock();
    Status {
        seeded: pool.seeded,
        out_bytes: pool.out_bytes,
        available: pool.ready,
    }
}
