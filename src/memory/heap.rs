//! The kernel heap: a free-list allocator that grows in 64 KiB chunks taken from
//! the frame allocator.
//!
//! Why chunks and not one frame per allocation: `alloc` must serve any size, and
//! 16 consecutive frames are enough for every `Vec`/`String` a kernel builds while
//! running a shell command, while still bounding what one leak can cost. Why a
//! free list *inside* the chunk and not a bump pointer: the self-test pushes past
//! a page into a `Vec`, which reallocates and frees its buffer several times, and
//! a bump allocator would leak every intermediate buffer — the free-frame ledger
//! that proves idempotency would never balance. That is the whole reason the extra
//! hundred lines are here.
//!
//! Chunks are given back. When a chunk's entire 64 KiB has become one free block,
//! it is returned to the frame allocator, so a run that allocated and freed leaves
//! the pool exactly as it found it. This is what makes `selftest` twice in one boot
//! print the same two ledgers.
//!
//! Deliberate limits, named in `src/memory/README.md`:
//! * a request aligned to more than 16 bytes is refused (null), not rounded — every
//!   block is 16-byte aligned, so 16 is the strongest promise the layout can keep,
//!   and a wrong-alignment pointer is corruption nobody sees;
//! * a single request may not exceed one chunk;
//! * at most [`MAX_CHUNKS`] chunks (4 MiB) are ever mapped;
//! * `realloc` is the trait's default (alloc, copy, free): growing in place is not
//!   worth its bug surface while every allocation here is a shell-sized vector.
//!
//! Lock order: the heap's lock is taken first, the frame allocator's second (when a
//! chunk is taken or given back). Never the other way round — the PMM never calls
//! into the heap. Both are taken with interrupts masked (`csr::without_interrupts`),
//! because the kernel's spinlock is not reentrant, `traps::init` leaves `sstatus.SIE`
//! set, and a trap handler that allocated while an interrupted context held the lock
//! would spin forever waiting for a context that cannot run again.

use core::alloc::{GlobalAlloc, Layout};
use core::ptr;

use crate::arch::riscv64::csr;
use crate::memory::pmm;
use crate::memory::FRAME_SIZE;
use crate::sync::{Guard, Spinlock};

/// Bytes taken from the frame allocator at a time.
pub const CHUNK_SIZE: usize = 64 * 1024;

/// Frames per chunk — the reason `pmm` has a contiguous-allocation entry point at
/// all: a chunk is only useful if its 16 frames are physically consecutive.
const CHUNK_FRAMES: usize = CHUNK_SIZE / FRAME_SIZE;

/// Chunks the heap may hold at once: the hard cap on its footprint. 64 chunks is
/// 4 MiB, far more than a shell command needs and small enough that a runaway
/// allocation surfaces as a null return instead of eating the machine.
const MAX_CHUNKS: usize = 64;

/// Alignment every block start (and therefore every payload) satisfies.
const ALIGN: usize = 16;

/// Size of [`Header`], and the offset from a block's start to its payload.
const HEADER_SIZE: usize = 16;

/// A remainder smaller than this stays part of the block it would be split from:
/// below a header plus one minimum payload, splitting buys nothing.
const MIN_SPLIT: usize = ALIGN + ALIGN;

/// Block header, at the start of every block, free or allocated.
///
/// `size` counts the header itself and is always a multiple of [`ALIGN`], so the
/// payload behind it is aligned too. `next` links free blocks and is only
/// meaningful while the block is free: a caller's writes start at
/// `block + HEADER_SIZE`, never over the header.
#[repr(C)]
struct Header {
    size: usize,
    next: usize,
}

// Stated once, checked once: a field added to `Header` would move every payload
// and silently break the alignment promise above.
const _: () = assert!(core::mem::size_of::<Header>() == HEADER_SIZE);

/// The heap's mutable state, behind the kernel spinlock.
struct State {
    /// Head of the free list, sorted by address; `0` means empty.
    ///
    /// Addresses, not pointers: the state is then trivially `Send`, and physical
    /// addresses are what everything else in `memory` deals in.
    head: usize,
    /// Bases of the chunks the heap owns right now, so a chunk that becomes wholly
    /// free can be handed back to the frame allocator.
    chunks: [usize; MAX_CHUNKS],
    chunk_count: usize,
    /// Bytes taken from the frame allocator, headers included: what the heap costs
    /// RAM. The boot line and `mem` report this.
    mapped: usize,
    /// Bytes currently handed out to callers.
    used: usize,
}

impl State {
    const fn new() -> Self {
        State {
            head: 0,
            chunks: [0; MAX_CHUNKS],
            chunk_count: 0,
            mapped: 0,
            used: 0,
        }
    }

    /// True when `addr` lies inside one of the chunks the heap owns.
    fn owns(&self, addr: usize) -> bool {
        self.chunks[..self.chunk_count]
            .iter()
            .any(|&base| addr >= base && addr < base + CHUNK_SIZE)
    }

    /// Map one more chunk and put it on the free list.
    ///
    /// `false` when the heap is at its cap, or the frame allocator has no run of 16
    /// consecutive frames left. Both are honest failures: the caller's allocation
    /// returns null, which `Vec` turns into a panic, which is a visible boot log
    /// rather than a silent short allocation.
    fn grow(&mut self) -> bool {
        if self.chunk_count == MAX_CHUNKS {
            return false;
        }
        // Lock order: heap, then PMM (see the module docs).
        let Some(base) = pmm::alloc_frames_contiguous(CHUNK_FRAMES) else {
            return false;
        };
        self.chunks[self.chunk_count] = base;
        self.chunk_count += 1;
        self.mapped += CHUNK_SIZE;
        // A chunk straight from the frame allocator cannot already be on the list,
        // so the "already free" answer is impossible here.
        let _ = self.insert_free(base, CHUNK_SIZE);
        true
    }

    /// Put a block back on the free list, keeping the list sorted by address and
    /// merging it with its neighbours.
    ///
    /// Merging on *every* insert is what lets [`State::release_free_chunks`] be a
    /// lookup instead of a walk: with adjacent free blocks always merged, "one free
    /// block covers the whole chunk" is exactly "the chunk is unused".
    ///
    /// `false` — with the list untouched — when `addr` is *already* on it. That is a
    /// double free, and the caller has to hear about it **before** moving a counter:
    /// reporting after the subtraction means the report is unreachable in a debug
    /// build, because the subtraction overflows first and `panic = "abort"` turns the
    /// line into a dead machine.
    fn insert_free(&mut self, addr: usize, size: usize) -> bool {
        let mut prev = 0usize;
        let mut cur = self.head;
        while cur != 0 && cur < addr {
            prev = cur;
            // SAFETY: every address on the free list is the start of a live header
            // inside a chunk the heap owns, and `next` is its link.
            cur = unsafe { (*(cur as *const Header)).next };
        }
        if cur == addr {
            // A double free is undefined behaviour by `GlobalAlloc`'s contract, but it
            // is also the first bug a new allocator grows; unchecked it would corrupt
            // the free list of a *different* allocation.
            return false;
        }

        let mut size = size;
        if cur != 0 && addr + size == cur {
            // SAFETY: `cur` is a live free block that starts exactly where this one
            // ends; merging two free neighbours is the invariant this function keeps.
            size += unsafe { (*(cur as *const Header)).size };
            cur = unsafe { (*(cur as *const Header)).next };
        }
        if prev != 0 {
            // SAFETY: `prev` is a live free block; its size is read to ask whether it
            // ends exactly where the new block starts.
            let prev_size = unsafe { (*(prev as *const Header)).size };
            if prev + prev_size == addr {
                // SAFETY: growing `prev` over the new block is sound because both
                // are free and adjacent; the new block's header is simply absorbed.
                unsafe { (*(prev as *mut Header)).size += size };
                unsafe { (*(prev as *mut Header)).next = cur };
                return true;
            }
        }
        // SAFETY: `addr` is a block start inside a chunk the heap owns (checked by
        // the caller) and it is not currently on the list (`cur != addr`), so
        // writing its header clobbers only the stale fields of an allocated block.
        unsafe { ptr::write(addr as *mut Header, Header { size, next: cur }) };
        if prev == 0 {
            self.head = addr;
        } else {
            // SAFETY: `prev` is a live free block, and `addr` goes right after it.
            unsafe { (*(prev as *mut Header)).next = addr };
        }
        true
    }

    /// Hand every chunk whose whole 64 KiB is one free block back to the frame
    /// allocator.
    ///
    /// This is what keeps the heap invisible in the PMM ledger when nothing is
    /// allocated, and therefore what makes two `selftest` runs in one boot print
    /// the same free-frame counts.
    fn release_free_chunks(&mut self) {
        let mut i = 0;
        while i < self.chunk_count {
            let base = self.chunks[i];
            let mut prev = 0usize;
            let mut cur = self.head;
            let mut whole = false;
            while cur != 0 {
                // SAFETY: free-list entries are live headers — see `insert_free`.
                let size = unsafe { (*(cur as *const Header)).size };
                if cur == base && size == CHUNK_SIZE {
                    whole = true;
                    break;
                }
                prev = cur;
                cur = unsafe { (*(cur as *const Header)).next };
            }
            if !whole {
                i += 1;
                continue;
            }
            // SAFETY: `cur` is the free block found just above; unlinking one node
            // from a sorted, merged list leaves the rest of it intact.
            let next = unsafe { (*(cur as *const Header)).next };
            if prev == 0 {
                self.head = next;
            } else {
                unsafe { (*(prev as *mut Header)).next = next };
            }
            pmm::free_frames_contiguous(base, CHUNK_FRAMES);
            self.mapped -= CHUNK_SIZE;
            // Swap-remove: chunk order carries no meaning, and a loop that must
            // re-examine the chunk now sitting at `i` must not advance.
            self.chunk_count -= 1;
            self.chunks[i] = self.chunks[self.chunk_count];
            self.chunks[self.chunk_count] = 0;
        }
    }
}

/// The global allocator. The `#[global_allocator]` wiring is in `src/main.rs`;
/// this type is the allocator itself, so that its state has exactly one owner.
pub struct Heap {
    state: Spinlock<State>,
}

impl Heap {
    pub const fn new() -> Self {
        Heap {
            state: Spinlock::new(State::new()),
        }
    }

    /// Take the heap's lock with interrupts masked.
    ///
    /// Same reasoning as `pmm::lock`: the kernel's [`Spinlock`] is not reentrant,
    /// `traps::init` leaves `sstatus.SIE` set, and a trap handler that allocated
    /// while an interrupted context held this lock would spin forever. Nesting is
    /// safe — `without_interrupts` restores the state it found — which matters here
    /// because `grow` and `release_free_chunks` call into the PMM under this lock.
    fn lock(&self) -> Guard<'_, State> {
        csr::without_interrupts(|| self.state.lock())
    }

    /// `(bytes in use, bytes mapped from the frame allocator, chunks held)`.
    pub fn stats(&self) -> (usize, usize, usize) {
        let state = self.lock();
        (state.used, state.mapped, state.chunk_count)
    }
}

// SAFETY: every allocation and deallocation goes through `self.state`'s lock, and
// the free list plus the chunk registry are the only way to reach memory this
// allocator owns. The `GlobalAlloc` contract is what the two methods below
// implement — a null return on failure, no aliasing of live allocations — and the
// module docs name the two places where they refuse rather than guess.
unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // Refusals, loudly, before anything is mutated: a wrong-alignment pointer
        // is corruption no one sees, a null is a panic with a stack trace of sorts.
        let need = match layout
            .size()
            .max(ALIGN)
            .checked_add(HEADER_SIZE + ALIGN - 1)
        {
            Some(rounded) => rounded & !(ALIGN - 1),
            None => return ptr::null_mut(),
        };
        if layout.align() > ALIGN || need > CHUNK_SIZE {
            return ptr::null_mut();
        }

        let mut state = self.lock();
        let mut grown = false;
        loop {
            let mut prev = 0usize;
            let mut cur = state.head;
            while cur != 0 {
                // SAFETY: every free-list address is the start of a live header in a
                // chunk the heap owns; `size` and `next` are its fields.
                let (size, next) = unsafe {
                    (
                        (*(cur as *const Header)).size,
                        (*(cur as *const Header)).next,
                    )
                };
                if size >= need {
                    // First fit. Unlink the block, then keep the tail of it free if
                    // there is enough of it to be worth its own header.
                    if prev == 0 {
                        state.head = next;
                    } else {
                        // SAFETY: `prev` is a live free block that pointed at `cur`.
                        unsafe { (*(prev as *mut Header)).next = next };
                    }
                    if size - need >= MIN_SPLIT {
                        let rest = cur + need;
                        // SAFETY: `rest` is inside the same chunk as `cur` — a split
                        // only ever shrinks the tail — and it was part of this free
                        // block, so no live allocation refers to it.
                        unsafe {
                            ptr::write(
                                rest as *mut Header,
                                Header {
                                    size: size - need,
                                    next,
                                },
                            )
                        };
                        if prev == 0 {
                            state.head = rest;
                        } else {
                            // SAFETY: as above; the remainder takes the block's place.
                            unsafe { (*(prev as *mut Header)).next = rest };
                        }
                        // SAFETY: `cur` stays a live header; only its size changes.
                        unsafe { (*(cur as *mut Header)).size = need };
                    }
                    state.used += layout.size();
                    // The payload begins `HEADER_SIZE` bytes into a block whose start
                    // is `ALIGN`-aligned — what the check at the top of `alloc`
                    // promised — and the block is now exclusively the caller's. The
                    // pointer arithmetic itself is not an unsafe operation.
                    return (cur + HEADER_SIZE) as *mut u8;
                }
                prev = cur;
                cur = next;
            }
            // Nothing fits: map one more chunk and look again — once. A fresh chunk
            // is empty and `need` is at most a chunk, so the second pass either
            // succeeds or the frame allocator has nothing left to give.
            if grown || !state.grow() {
                return ptr::null_mut();
            }
            grown = true;
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let addr = ptr as usize - HEADER_SIZE;
        let mut state = self.lock();
        if !state.owns(addr) {
            // The registry is the only witness that a free belongs to this heap,
            // and it is checked before a single byte is written: a wild free must
            // not be able to reach into the free list.
            crate::kprintln!("heap: refusing to free {:#x}: not a heap block", addr);
            return;
        }
        // SAFETY: `ptr` came from `alloc` (the trait's contract), so `addr` is a
        // live block start whose header is intact — callers only write the payload.
        let size = unsafe { (*(addr as *const Header)).size };
        // The duplicate check, and therefore the report, comes *before* any counter
        // moves: `insert_free` refuses a block that is already on the list, and the
        // caller must not have subtracted first — in a debug build that subtraction
        // would panic (and `panic = "abort"` ends the machine) before the intended
        // one-line report could be printed.
        if !state.insert_free(addr, size) {
            crate::kprintln!("heap: double free of {:#x} ignored", addr);
            return;
        }
        // `saturating_sub`, not `-=`: an allocator must never panic, and a caller
        // that frees a layout it never allocated is already undefined behaviour — the
        // one thing worth doing with it is clamping the counter and saying so.
        let before = state.used;
        state.used = before.saturating_sub(layout.size());
        if before < layout.size() {
            crate::kprintln!(
                "heap: freeing {} B while {} B are in use — counters clamped",
                layout.size(),
                before
            );
        }
        state.release_free_chunks();
    }
}
