//! The physical frame allocator: one bit per 4 KiB frame of physical RAM.
//!
//! Why a bitmap and not a free list: the pool is a few hundred MiB at most, the
//! kernel is single-hart, and a bitmap answers both questions that matter —
//! "which frame is free" and "how many are free" — without storing metadata in
//! memory the allocator does not own yet. It also makes the heap's 64 KiB chunks
//! possible: a run of set bits *is* a physically contiguous allocation, which a
//! singly-linked free list cannot promise.
//!
//! Polarity is deliberate: a **set** bit means **free**. The table is 128 KiB of
//! `.bss`, so zero-filled memory means "nothing is allocatable yet" — an
//! allocation before [`init`] fails instead of handing out the kernel's own
//! image, with no `initialized` flag to forget to check. Bits for addresses
//! outside the `/memory` node are never set, which keeps
//! `free_bits.count_ones()` equal to the free-frame count by construction rather
//! than by bookkeeping.
//!
//! What this is not: no paging (see `src/memory/README.md`), no frame stealing,
//! no NUMA, no per-frame reference counts (nothing shares a frame yet). The
//! spinlock is the kernel's, and it protects against a second context appearing
//! later; today only hart 0 runs Rust at all. Every lock site takes it with
//! interrupts masked (see [`lock`]): `traps::init` enables `sstatus.SIE`, so without
//! the mask the first trap handler that asked for a frame would deadlock against a
//! lock the interrupted context still holds.

use crate::arch::riscv64::csr;
use crate::memory::FRAME_SIZE;
use crate::sync::{Guard, Spinlock};

/// Highest physical address the table can describe: 4 GiB.
///
/// `run.sh` boots `virt` with 256 MiB, so the window is three quarters empty —
/// but it is what a frame index can reach in one `usize` arithmetic step, and the
/// cost is 128 KiB of `.bss` whether the machine uses it or not. RAM above the
/// window is reported and left unusable rather than half-tracked.
pub const WINDOW: usize = 1 << 32;

/// Frames in the window.
const FRAMES: usize = WINDOW / FRAME_SIZE;

/// 64-bit words in the table.
const WORDS: usize = FRAMES / 64;

struct State {
    /// One bit per frame, **set = free**; see the polarity note above.
    free_bits: [u64; WORDS],
    /// Frames the `/memory` node contributes: the `total` of [`stats`].
    tracked: usize,
    /// Free frames inside the pool.
    ///
    /// Kept as a counter so `stats` is O(1) and the boot line does not walk
    /// 128 KiB; table and counter are updated together, under the lock, in
    /// exactly three places: `init`, `alloc_frames_contiguous`, `free_…`.
    free: usize,
    /// Frame-aligned `(start, end)` of the pool, for `mem`.
    start: usize,
    end: usize,
    /// Next-fit hint: the frame after the last allocation.
    ///
    /// Scanning from a cursor instead of from frame zero matters once the pool is
    /// full: every allocation would otherwise re-walk the long-used prefix, and
    /// the heap asks for 16-frame runs.
    cursor: usize,
}

static PMM: Spinlock<State> = Spinlock::new(State {
    free_bits: [0; WORDS],
    tracked: 0,
    free: 0,
    start: 0,
    end: 0,
    cursor: 0,
});

/// Take the pool's lock with interrupts masked.
///
/// The kernel's [`Spinlock`] is not reentrant and this is a single-hart kernel:
/// `traps::init` enables `sstatus.SIE`, so a trap handler that allocated a frame —
/// or merely asked for [`stats`] — while the interrupted context held the lock
/// would spin forever, because the interrupted context cannot run again to release
/// it. Masking interrupts for the critical section closes that window, and nesting
/// is safe: `without_interrupts` restores the state it found, so the inner call of
/// a `heap::alloc` → `alloc_frames_contiguous` pair does not re-enable anything.
fn lock() -> Guard<'static, State> {
    csr::without_interrupts(|| PMM.lock())
}

/// Build the pool.
///
/// `memory` is the device tree's `/memory` range — the only RAM the kernel knows
/// about — and `reserved` lists the ranges inside it that must never be handed
/// out: the kernel image and firmware (`(0, kernel_end)`), the boot stack, and
/// the device tree blob. Ranges are rounded outward to whole frames, so a
/// reserved byte anywhere in a frame reserves the frame.
///
/// Everything else inside `/memory` becomes free — including, deliberately,
/// memory the firmware might be using for something the device tree does not
/// describe. That is a real assumption, named in `src/memory/README.md`: on this
/// machine the firmware's contract is "everything in `/memory` except what you
/// are told to keep is yours".
pub fn init(memory: (usize, usize), reserved: &[(usize, usize)]) {
    let mut state = lock();

    // Filled in place, never by value: the table is 128 KiB and the boot stack is
    // 64 KiB, so a temporary `State` on the stack would smash the stack it is
    // running on. `fill` writes through the lock's `&mut`.
    state.free_bits.fill(0);

    let start = memory.0 & !(FRAME_SIZE - 1);
    let end = ((memory.1 + FRAME_SIZE - 1) & !(FRAME_SIZE - 1)).min(WINDOW);
    let end = end.max(start);
    state.start = start;
    state.end = end;
    state.tracked = (end - start) / FRAME_SIZE;
    state.cursor = start / FRAME_SIZE;
    state.free = 0;

    if memory.1 <= memory.0 {
        crate::kprintln!("pmm: the device tree reports an empty /memory range");
    } else if memory.1 > WINDOW {
        crate::kprintln!(
            "pmm: RAM above {:#x} is outside the bitmap window and stays unusable",
            WINDOW
        );
    }

    for frame in start / FRAME_SIZE..end / FRAME_SIZE {
        set_bit(&mut state.free_bits, frame);
    }
    for &(lo, hi) in reserved {
        mark_used(&mut state.free_bits, lo, hi);
    }

    // Counted after all marking: the counter has one author (`init`) and the
    // table is the truth, so the two cannot disagree about the initial pool.
    state.free = state
        .free_bits
        .iter()
        .map(|w| w.count_ones() as usize)
        .sum();
}

/// Take one 4 KiB frame. `None` when the pool is empty.
pub fn alloc_frame() -> Option<usize> {
    alloc_frames_contiguous(1)
}

/// Take `count` frames that are physically consecutive, as `(start address)`.
///
/// `None` when `count` is zero or no run of that length is free. Contiguity is
/// the whole point of the call: the heap needs 16 consecutive frames per 64 KiB
/// chunk, and a request a free-list allocator could satisfy by handing out
/// scattered frames would be useless here.
pub fn alloc_frames_contiguous(count: usize) -> Option<usize> {
    if count == 0 {
        return None;
    }
    let mut state = lock();
    if count > state.tracked || count > state.free {
        // Both cheap bounds; without them the scan below could report a run made
        // of frames outside the pool (they are always "used", so it is only a
        // waste of time — but `first + offset` with a huge `count` overflows).
        return None;
    }

    let first = state.start / FRAME_SIZE;
    let total = state.tracked;
    // Start positions to try: frame `first .. first + total - count`.
    let starts = total - count + 1;
    let begin = if state.cursor >= first && state.cursor + count <= first + total {
        state.cursor - first
    } else {
        0
    };

    let mut run = 0usize;
    let mut run_start = 0usize;
    for step in 0..starts {
        // Wrapping on purpose: the cursor is a hint, and a run that begins before
        // the cursor must still be found — otherwise `alloc_frame` would report a
        // full pool while free frames sit below the cursor.
        let frame = first + (begin + step) % starts;
        if !is_free(&state.free_bits, frame) {
            run = 0;
            continue;
        }
        if run == 0 {
            run_start = frame;
        }
        run += 1;
        if run == count {
            for f in run_start..run_start + count {
                clear_bit(&mut state.free_bits, f);
            }
            state.free -= count;
            state.cursor = run_start + count;
            return Some(run_start * FRAME_SIZE);
        }
    }
    None
}

/// Give one frame back.
pub fn free_frame(address: usize) {
    free_frames_contiguous(address, 1)
}

/// Give `count` consecutive frames back, starting at `address`.
///
/// A free of something the pool does not own, or of a frame that is already free,
/// is reported and ignored: both are bugs elsewhere, and neither must be allowed
/// to corrupt the table or the counter (a double free that silently inflated the
/// free count would turn every later ledger into a lie).
pub fn free_frames_contiguous(address: usize, count: usize) {
    if count == 0 {
        return;
    }
    let mut state = lock();
    let first = state.start / FRAME_SIZE;
    let last = first + state.tracked;
    let frame = address / FRAME_SIZE;
    let in_pool = address % FRAME_SIZE == 0
        && frame >= first
        && frame.checked_add(count).is_some_and(|end| end <= last);
    if !in_pool {
        crate::kprintln!(
            "pmm: refusing to free {:#x}+{} frame(s): outside the pool",
            address,
            count
        );
        return;
    }
    for f in frame..frame + count {
        if is_free(&state.free_bits, f) {
            crate::kprintln!("pmm: double free of frame {:#x} ignored", f * FRAME_SIZE);
            continue;
        }
        set_bit(&mut state.free_bits, f);
        state.free += 1;
    }
}

/// `(free, total)` frames in the pool.
pub fn stats() -> (usize, usize) {
    let state = lock();
    (state.free, state.tracked)
}

/// Frame-aligned `(start, end)` of the range the pool was built from.
pub fn pool_range() -> (usize, usize) {
    let state = lock();
    (state.start, state.end)
}

/// Mark every frame the range `lo..hi` touches as used.
///
/// The end is rounded up: a reserved range that ends mid-frame still owns that
/// frame. The range is clamped to the window, because the inputs are a linker
/// symbol, a firmware address and a device tree number that a buggy blob could
/// make enormous.
fn mark_used(free_bits: &mut [u64; WORDS], lo: usize, hi: usize) {
    let first = (lo / FRAME_SIZE).min(FRAMES);
    let last = hi
        .saturating_add(FRAME_SIZE - 1)
        .div_euclid(FRAME_SIZE)
        .min(FRAMES);
    for frame in first..last.max(first) {
        clear_bit(free_bits, frame);
    }
}

fn is_free(free_bits: &[u64; WORDS], frame: usize) -> bool {
    free_bits[frame / 64] & (1u64 << (frame % 64)) != 0
}

fn set_bit(free_bits: &mut [u64; WORDS], frame: usize) {
    free_bits[frame / 64] |= 1u64 << (frame % 64);
}

fn clear_bit(free_bits: &mut [u64; WORDS], frame: usize) {
    free_bits[frame / 64] &= !(1u64 << (frame % 64));
}
