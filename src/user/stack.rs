//! Demand-grown user stacks.
//!
//! A process starts with the fixed 64 KiB window
//! [`USER_STACK_BASE`]`..`[`USER_TOP`] — sixteen frames, all mapped and zeroed
//! at load time. This module grows that window downwards, one page at a time,
//! when a U-mode load or store faults between
//! [`USER_STACK_FLOOR`] and the lowest page mapped so far: the ordinary way a
//! deep call chain or a large stack frame asks for more stack.
//!
//! # What counts as a stack fault
//!
//! [`try_grow`] is called from the trap dispatcher *before* signal delivery, so
//! an accepted fault maps its page and resumes the faulting instruction instead
//! of becoming a SIGSEGV. A fault is accepted only when every one of these
//! holds:
//!
//! * the cause is a load or store page fault — an instruction fetch is never
//!   growth, because the pages it maps are RW and not executable, so the retry
//!   would fault forever;
//! * the address is below the lowest mapped stack page (`Process::stack_low`);
//! * it is not below [`USER_STACK_FLOOR`] — the 1 MiB per-process cap;
//! * it lies within [`FAULT_SLACK`] of the interrupted user `sp`, so a wild
//!   pointer far below the stack that was running is not "just below the stack
//!   bottom";
//! * no page between the fault and the current bottom is mapped, so a crafted
//!   image that placed a `PT_LOAD` page in the growth window keeps it (and the
//!   fault stays fatal) instead of being overwritten, and a refused fault never
//!   leaves a half-grown span behind;
//! * the frame pool can supply every page down to the fault, otherwise the pages
//!   already mapped stay tracked by `stack_low` and the fault is declined.
//!
//! The out-of-bounds case is the documented failure mode: recursion past 1 MiB
//! faults on the guard page below the floor and the process dies with the
//! ordinary `[user] … killed: store page fault` diagnostic, exactly as
//! recursion past 64 KiB did before growth existed.
//!
//! # Why this is safe in a trap handler
//!
//! A synchronous U-mode exception runs with `SIE = 0` and is not preemptible, so
//! the PMM and page-table locks taken here cannot be held by an interrupt; the
//! work is bounded by [`FAULT_SLACK`] worth of pages and uses no heap allocation
//! and no large stack buffer. Nothing is printed — the counters below are the
//! evidence the selftest reads.
//!
//! # Teardown
//!
//! Grown pages are deliberately *not* recorded in `Process`: 240 frame
//! addresses would move it through a trap stack on every `fork`/`wait4`.
//! [`release`] walks the range from `stack_low` up to the initial window and
//! frees whatever is mapped there (the `release_mapping` pattern); the initial
//! 64 KiB stays a contiguous PMM run owned by `Process::stack_base` and is
//! released by the runner's stack teardown as before.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::arch::riscv64::mmu::Flags;
use crate::arch::riscv64::traps::{CAUSE_LOAD_PAGE_FAULT, CAUSE_STORE_PAGE_FAULT};
use crate::memory::{paging, pmm, FRAME_SIZE};
use crate::user::elf::{USER_STACK_BASE, USER_STACK_FLOOR};
use crate::user::process;

/// How far below the interrupted user `sp` a faulting address may sit and still
/// count as a stack access.
///
/// A real prologue lowers `sp` first, so the address it then stores to is at or
/// above the new `sp`; 64 KiB of slack covers a stack frame larger than the
/// whole initial stack. A store more than this far below `sp` is a wild pointer
/// (or a stack clash) and stays fatal — mapping it would let a buggy program
/// silently claim the whole 1 MiB window with one bad access.
const FAULT_SLACK: usize = 64 * 1024;

/// Faults that mapped at least one demand-grown page.
static GROWTHS: AtomicU64 = AtomicU64::new(0);

/// Pages those growths mapped (always at least one per [`GROWTHS`] event).
static GROWN_PAGES: AtomicU64 = AtomicU64::new(0);

/// Load/store page faults the growth window refused: an address outside the
/// window, below the floor, too far below `sp`, or a page in the span already
/// mapped. A fault with any other cause never reaches the counter.
static REFUSED: AtomicU64 = AtomicU64::new(0);

/// Faults inside the window the frame pool could not supply.
static REFUSED_FRAMES: AtomicU64 = AtomicU64::new(0);

/// A snapshot of the demand-growth counters, read by the user-mode selftest.
///
/// The counters never reset: a selftest compares them against a baseline it
/// took before its own runs, which keeps `selftest` idempotent in one boot.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Stats {
    pub growths: u64,
    pub pages: u64,
    pub refused: u64,
    pub refused_frames: u64,
}

/// Read the counters (relaxed: one hart, and every reader only needs a value
/// that is not older than its own last syscall).
pub fn stats() -> Stats {
    Stats {
        growths: GROWTHS.load(Ordering::Relaxed),
        pages: GROWN_PAGES.load(Ordering::Relaxed),
        refused: REFUSED.load(Ordering::Relaxed),
        refused_frames: REFUSED_FRAMES.load(Ordering::Relaxed),
    }
}

/// Try to satisfy a U-mode page fault with demand-grown stack pages.
///
/// `stval` is the faulting address and `user_sp` the interrupted user `sp`
/// (frame slot 8 on a trap from U-mode). Returns `true` only when the faulting
/// page is mapped, which is the caller's licence to retry the instruction at the
/// unchanged `sepc`; `false` leaves the process state exactly as it was, except
/// for pages a partial growth already mapped (they are tracked and freed at
/// teardown).
pub fn try_grow(scause: usize, stval: usize, user_sp: usize) -> bool {
    if scause != CAUSE_LOAD_PAGE_FAULT && scause != CAUSE_STORE_PAGE_FAULT {
        return false;
    }
    let Some(pid) = process::current_pid() else {
        return false;
    };
    // Read the two fields under the table lock, then do the walking without it:
    // the page-table walk allocates frames, and a `Process` borrow must not
    // outlive the guard.
    let Some((root, low)) = process::peek(pid, |run| (run.root, run.stack_low)) else {
        return false;
    };
    let page = stval & !(FRAME_SIZE - 1);
    let eligible = page < low
        && page >= USER_STACK_FLOOR
        && stval >= user_sp.saturating_sub(FAULT_SLACK)
        && paging::flags_in(root, page).is_none();
    if !eligible {
        REFUSED.fetch_add(1, Ordering::Relaxed);
        return false;
    }

    // Every page between the fault and the current bottom has to be free before
    // anything is mapped. A `PT_LOAD` page is allowed inside this window (the
    // loader only requires `page_end <= USER_STACK_BASE`), and refusing *before*
    // the first allocation is what keeps `stack_low..USER_STACK_BASE` exactly
    // "the pages this module mapped": a partial growth that stopped under an
    // image page would leave a hole the teardown walk and `copy_grown_stack`
    // both assume cannot exist. The walk is a table lookup per page, bounded by
    // the cap.
    let mut probe = page;
    while probe < low {
        if paging::flags_in(root, probe).is_some() {
            REFUSED.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        probe += FRAME_SIZE;
    }

    // Grow downwards from the current bottom to the faulting page. A frame-pool
    // failure stops the loop with the pages mapped so far still valid; the
    // process keeps them (`stack_low` moves) but the fault is declined, because
    // the instruction that faulted cannot be resumed.
    let mut va = low;
    let mut new_low = low;
    while va > page {
        va -= FRAME_SIZE;
        let Some(frame) = pmm::alloc_frame() else {
            break;
        };
        // SAFETY: `frame` is a fresh PMM frame — RAM, therefore identity-mapped
        // supervisor RW by `paging::init` — and nothing else references it. A
        // U-mode program must never read a previous tenant of a stack page, so
        // the frame is zeroed through the physical address (the loader's
        // pattern) before it is published in the user root.
        unsafe { core::ptr::write_bytes(frame as *mut u8, 0, FRAME_SIZE) };
        if paging::map_in(root, va, frame, Flags::user_rw()).is_err() {
            // `map_in` changes nothing on error, so the frame is still ours.
            pmm::free_frame(frame);
            break;
        }
        new_low = va;
    }

    if new_low != low {
        let pages = ((low - new_low) / FRAME_SIZE) as u64;
        process::update(pid, |run| run.stack_low = new_low);
        GROWTHS.fetch_add(1, Ordering::Relaxed);
        GROWN_PAGES.fetch_add(pages, Ordering::Relaxed);
    }
    if new_low > page {
        REFUSED_FRAMES.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    true
}

/// Unmap and free the demand-grown stack pages of `root`, from `low` up to the
/// initial window.
///
/// A page that is not mapped (a growth that failed part-way, or a process whose
/// stack was never built) is skipped, so this is safe on any `Process` state;
/// every mapped page in the range belongs to this stack by construction. The
/// loop is bounded by the cap — at most 240 iterations. The caller frees the
/// initial contiguous run separately, and must call this *before* releasing the
/// page tables.
pub fn release(root: usize, low: usize) {
    if root == 0 {
        return;
    }
    let mut va = low;
    while va < USER_STACK_BASE {
        if let Ok(frame) = paging::unmap_in(root, va) {
            pmm::free_frame(frame);
        }
        va += FRAME_SIZE;
    }
}

/// Pages one process may still map below the initial window: the selftest's and
/// the docs' single source for "how much stack growth is left".
pub const MAX_GROWTH_PAGES: usize = (USER_STACK_BASE - USER_STACK_FLOOR) / FRAME_SIZE;

// A cap that was not a whole number of pages would make the floor address
// unaligned and the range above wrong; keep the two in step at compile time.
const _: () = assert!(USER_STACK_FLOOR % FRAME_SIZE == 0);
const _: () = assert!(USER_STACK_BASE - USER_STACK_FLOOR == MAX_GROWTH_PAGES * FRAME_SIZE);
