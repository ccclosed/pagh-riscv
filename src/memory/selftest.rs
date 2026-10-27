//! The memory subsystem's in-QEMU self-tests, run by the shell's `selftest`.
//!
//! Contract, in the spirit of the x86 kernel's suite: non-destructive,
//! deterministic and idempotent. Every frame and every heap byte a routine takes
//! is given back before the routine returns — including on its failure paths,
//! because a test that fails must not also poison the state the next test
//! measures.
//!
//! The `[selftest] PMM hygiene` line prints the free-frame count before and after
//! the run. Two runs in one boot must print the same pair; a routine that retains
//! frames makes them differ, and that is the cheapest possible detector for the
//! class of bug that is otherwise invisible until the pool is empty. (On x86
//! exactly this ledger caught an ELF fuzz routine draining the whole pool.)
//!
//! Each routine prints `SELFTEST <name> PASS` / `FAIL` as it finishes, so a
//! regression names itself on the serial line instead of only moving a counter.

use alloc::string::String;
use alloc::vec::Vec;
use core::alloc::{GlobalAlloc, Layout};

use crate::arch::riscv64::mmu::{self, Flags};
use crate::memory::paging::{self, MapError};
use crate::memory::pmm;
use crate::memory::FRAME_SIZE;

/// Frames the round-trip routine takes in one go.
const BATCH: usize = 64;

/// Elements the heap routine pushes: 1024 × 8 B = 8 KiB, twice a page, so the
/// growth path (not just the first allocation) is exercised.
const VEC_LEN: usize = 1024;

/// Bytes the heap routine pushes into a `String`, cyclically through the alphabet.
const STRING_LEN: usize = 64;

/// Multiplier of the `Vec` contents; any odd constant that spreads bits works, but
/// it is named because the check below must use the same one.
const VEC_STRIDE: u64 = 0x9e37_79b9_7f4a_7c15;

/// Byte the frame routine fills a whole frame with, and the same byte repeated
/// across a 64-bit word, so a partial write shows up as a word mismatch.
const PATTERN: u8 = 0xa5;
const PATTERN_WORD: u64 = 0xa5a5_a5a5_a5a5_a5a5;

/// Virtual address the Sv39 routine maps a frame at. It sits inside Sv39's lower
/// half (which ends at 2^38) and far above RAM, so nothing else in the kernel can
/// be using it, and it is unmapped until that routine maps it.
const TEST_VA: usize = 0x3f_c000_0000;

/// A virtual address in a different middle-level subtree, mapped by nothing: the
/// `NotMapped` case, and the address whose `unmap` must be refused.
const UNMAPPED_VA: usize = 0x3f_c020_0000;

/// Words the Sv39 routine writes through its new mapping: 512 B of one page.
/// The whole frame is not the point here — `frame_pattern` already proves a frame
/// is real memory; this routine proves the *translation* is.
const MAPPED_WORDS: usize = 64;

/// The word pattern written through the mapping and read back through both
/// names. Any odd constant works; this one is the golden-ratio word the x86 tree
/// uses, and it is named because three places must agree on it.
const MAP_PATTERN: u64 = 0x9e37_79b9_7f4a_7c15;

/// Run every routine.
///
/// Nothing is asserted or panicked: the caller is the shell, whose job is to keep
/// running, and the verdicts are on the serial line where the E2E tools can grep
/// them. Returns the number of routines that passed, for callers that want it.
pub fn run() -> usize {
    let (before, total) = pmm::stats();
    crate::kprintln!("[selftest] free frames before: {} of {}", before, total);

    let checks: [(&str, fn() -> bool); 7] = [
        ("frames-roundtrip", frames_roundtrip),
        ("frame-pattern", frame_pattern),
        ("sv39-map-roundtrip", sv39_map_roundtrip),
        ("sv39-translate", sv39_translate),
        ("heap-vec", heap_vec),
        ("heap-defenses", heap_defenses),
        ("heap-chunks", heap_chunks),
    ];
    let mut passed = 0usize;
    for (name, check) in checks {
        let ok = check();
        if ok {
            passed += 1;
        }
        crate::kprintln!("SELFTEST {} {}", name, if ok { "PASS" } else { "FAIL" });
    }

    let (after, _) = pmm::stats();
    crate::kprintln!("SELFTEST SUMMARY: {}/{} passed", passed, checks.len());
    crate::kprintln!(
        "[selftest] PMM hygiene: free frames {} -> {}",
        before,
        after
    );
    passed
}

/// Take [`BATCH`] frames, check them, give them back, and require the free count
/// to land exactly where it started.
///
/// Three things are asserted at once: frames come back 4 KiB-aligned, no frame is
/// handed out twice, and the pool's counter moves by exactly the number of frames
/// taken. A counter that drifts here makes every later ledger meaningless, so it is
/// checked while the frames are held, not only after they are returned.
fn frames_roundtrip() -> bool {
    let (before, _) = pmm::stats();
    let mut frames = [0usize; BATCH];
    let mut taken = 0usize;
    let mut ok = true;

    for i in 0..BATCH {
        match pmm::alloc_frame() {
            Some(frame) => {
                if frame % FRAME_SIZE != 0 {
                    crate::kprintln!("[selftest] frame {:#x} is not 4 KiB aligned", frame);
                    ok = false;
                }
                if frames[..i].contains(&frame) {
                    crate::kprintln!("[selftest] frame {:#x} was handed out twice", frame);
                    ok = false;
                }
                frames[i] = frame;
                taken += 1;
            }
            None => {
                // Not fatal for the run: the frames taken so far are still given
                // back below, and the ledger will show the pool as it was.
                crate::kprintln!("[selftest] the pool ran out after {} frames", taken);
                ok = false;
                break;
            }
        }
    }

    let (during, _) = pmm::stats();
    if during != before - taken {
        crate::kprintln!(
            "[selftest] free frames went to {} for {} taken, expected {}",
            during,
            taken,
            before - taken
        );
        ok = false;
    }
    for frame in frames[..taken].iter() {
        pmm::free_frame(*frame);
    }
    let (after, _) = pmm::stats();
    if after != before {
        crate::kprintln!(
            "[selftest] free frames are {} after the round trip, {} before",
            after,
            before
        );
        ok = false;
    }
    ok
}

/// A frame is real memory: write a whole frame through a raw pointer and read it
/// back, then probe the first and last word.
///
/// Volatile accesses on purpose. A frame's physical address is a dereferenceable
/// address *because* the kernel identity-maps RAM — above `__kernel_end` as
/// read/write, see `memory::paging` — so the pointer below is exactly the address
/// the CPU translates to itself; but a non-volatile store to memory nothing else
/// reads may be optimised away, and a test that passes without touching RAM
/// proves nothing.
fn frame_pattern() -> bool {
    let Some(frame) = pmm::alloc_frame() else {
        crate::kprintln!("[selftest] no free frame for the pattern test");
        return false;
    };
    let mut ok = true;
    let mut mismatched = 0usize;

    // SAFETY: `frame` came from the frame allocator, so no other code owns it. It
    // is a whole 4 KiB, and it lies above `__kernel_end`, where the identity map
    // is read/write precisely so a frame address is a usable pointer; this routine
    // writes it and reads it back, nothing else.
    unsafe {
        let bytes = frame as *mut u8;
        for i in 0..FRAME_SIZE {
            bytes.add(i).write_volatile(PATTERN);
        }
        for i in 0..FRAME_SIZE {
            if bytes.add(i).read_volatile() != PATTERN {
                mismatched += 1;
            }
        }
        // The first and last word specifically: a frame that is only partly mapped
        // is the failure this is looking for, and a byte count alone hides which end.
        let words = frame as *mut u64;
        let last = words.add(FRAME_SIZE / 8 - 1);
        words.write_volatile(PATTERN_WORD);
        last.write_volatile(0x0123_4567_89ab_cdef);
        if words.read_volatile() != PATTERN_WORD {
            ok = false;
        }
        if last.read_volatile() != 0x0123_4567_89ab_cdef {
            ok = false;
        }
    }

    if mismatched != 0 {
        crate::kprintln!(
            "[selftest] {}/{} bytes of frame {:#x} did not read back",
            mismatched,
            FRAME_SIZE,
            frame
        );
        ok = false;
    }
    pmm::free_frame(frame);
    ok
}

/// The heap serves `alloc`: push past one page into a `Vec<u64>`, build a `String`,
/// check both, and then check the heap holds nothing once they are dropped.
///
/// The last part is what keeps the ledger even. The heap gives a wholly free chunk
/// back to the frame allocator, so a routine that only allocated and freed leaves
/// the pool exactly as it found it — and if that ever stops being true, this routine
/// says so instead of the ledger merely drifting between runs.
fn heap_vec() -> bool {
    let mut ok = true;
    // A driver or a mounted filesystem owns heap state for the rest of the boot;
    // "empty" is therefore the wrong invariant. What must hold is that *this*
    // routine returns the heap to the numbers it found — that is what keeps the
    // frame ledger even between passes.
    let (base_used, base_mapped, base_chunks) = crate::heap_stats();
    let mut values: Vec<u64> = Vec::new();
    for i in 0..VEC_LEN {
        values.push((i as u64).wrapping_mul(VEC_STRIDE));
    }

    if values.len() != VEC_LEN {
        crate::kprintln!(
            "[selftest] the Vec holds {} of {} elements",
            values.len(),
            VEC_LEN
        );
        ok = false;
    }
    // "Past one page" is the requirement, so assert the buffer really did outgrow a
    // frame rather than trusting the element count to imply it.
    let bytes = values.capacity() * core::mem::size_of::<u64>();
    if bytes <= FRAME_SIZE {
        crate::kprintln!(
            "[selftest] the Vec stayed inside one page ({} bytes)",
            bytes
        );
        ok = false;
    }
    for (i, value) in values.iter().enumerate() {
        if *value != (i as u64).wrapping_mul(VEC_STRIDE) {
            crate::kprintln!(
                "[selftest] vec[{}] = {:#x} after the growth copies",
                i,
                value
            );
            ok = false;
            break;
        }
    }

    // `String` as well as `Vec`: the claim is that the collection types work, and
    // the two reach the allocator through different `Layout`s (a `u64` array and a
    // byte buffer), so the second one is worth its four lines.
    let mut text = String::new();
    for i in 0..STRING_LEN {
        text.push((b'a' + (i % 26) as u8) as char);
    }
    if text.len() != STRING_LEN
        || !text
            .bytes()
            .enumerate()
            .all(|(i, b)| b == b'a' + (i % 26) as u8)
    {
        crate::kprintln!(
            "[selftest] the String did not read back ({} bytes)",
            text.len()
        );
        ok = false;
    }

    let (used, mapped, chunks) = crate::heap_stats();
    crate::kprintln!(
        "[selftest] heap: {} B in use, {} B mapped, {} chunk(s)",
        used,
        mapped,
        chunks
    );
    drop(values);
    drop(text);

    let (used, mapped, _) = crate::heap_stats();
    if used != base_used || mapped != base_mapped {
        crate::kprintln!(
            "[selftest] the heap did not return to its baseline: {} B used / {} B mapped now, {} B / {} B before ({} chunk(s) at start)",
            used,
            mapped,
            base_used,
            base_mapped,
            base_chunks
        );
        ok = false;
    }
    ok
}

/// The heap's failure paths, exercised on purpose: a double free, and a free of a
/// pointer the heap never handed out.
///
/// This is the one place in the kernel that calls `GlobalAlloc::dealloc` on
/// something `alloc` did not return. The trait calls that undefined behaviour; the
/// point of the routine is that *this* allocator defines both cases as "report on
/// the serial line and change nothing". If that ever regressed, the routine would
/// not print `FAIL`: it would panic inside the allocator (debug overflow checks, and
/// this kernel cannot unwind) and the machine would be gone — the loudest failure
/// available. The counters are read after each case, because a "defense" that
/// quietly corrupted `used` or `mapped` would otherwise still look successful.
///
/// The keeper allocation is load-bearing: with it live, the freed block stays on the
/// free list under its own address, which is exactly the state a second free has to
/// recognise. Without it the chunk would be given back to the PMM and the double free
/// would take the other path (a pointer outside every owned chunk).
fn heap_defenses() -> bool {
    let mut ok = true;
    // See `heap_vec`: compare against the state this routine found, not against
    // zero, so a driver's or filesystem's persistent heap does not read as a leak.
    let (base_used, base_mapped, base_chunks) = crate::heap_stats();
    let block_layout = Layout::from_size_align(64, 8).unwrap();
    let keeper_layout = Layout::from_size_align(128, 8).unwrap();

    // SAFETY: both allocations come from this crate's global allocator and are freed
    // once through the same handle; the two extra frees below are the deliberate
    // contract violations described above, and the allocator checks ownership and
    // "already free" before writing a byte or moving a counter.
    unsafe {
        let keeper = crate::ALLOCATOR.alloc(keeper_layout);
        let block = crate::ALLOCATOR.alloc(block_layout);
        if keeper.is_null() || block.is_null() {
            crate::kprintln!("[selftest] the heap refused an allocation");
            if !keeper.is_null() {
                crate::ALLOCATOR.dealloc(keeper, keeper_layout);
            }
            if !block.is_null() {
                crate::ALLOCATOR.dealloc(block, block_layout);
            }
            return false;
        }
        let (allocated, mapped_allocated, chunks_allocated) = crate::heap_stats();

        crate::ALLOCATOR.dealloc(block, block_layout);
        let (used, mapped, _) = crate::heap_stats();
        if used != allocated - block_layout.size() || mapped != mapped_allocated {
            crate::kprintln!(
                "[selftest] a normal free left {} B used, {} B mapped ({} B, {} B before)",
                used,
                mapped,
                allocated,
                mapped_allocated
            );
            ok = false;
        }

        // The double free: same pointer, same layout, twice.
        crate::ALLOCATOR.dealloc(block, block_layout);
        let (used, mapped, chunks) = crate::heap_stats();
        if used != allocated - block_layout.size()
            || mapped != mapped_allocated
            || chunks != chunks_allocated
        {
            crate::kprintln!(
                "[selftest] the double free moved the heap: {} B used, {} B mapped, {} chunk(s)",
                used,
                mapped,
                chunks
            );
            ok = false;
        }

        // A free of an address that is not a heap block at all — the stack, in fact.
        let local = 0u64;
        crate::ALLOCATOR.dealloc(core::ptr::addr_of!(local) as *mut u8, block_layout);
        let (used_after_wild, mapped_after_wild, _) = crate::heap_stats();
        if used_after_wild != used || mapped_after_wild != mapped {
            crate::kprintln!(
                "[selftest] a wild free moved the heap: {} B used, {} B mapped",
                used_after_wild,
                mapped_after_wild
            );
            ok = false;
        }

        crate::ALLOCATOR.dealloc(keeper, keeper_layout);
        let (used, mapped, _) = crate::heap_stats();
        if used != base_used || mapped != base_mapped {
            crate::kprintln!(
                "[selftest] the heap kept {} B ({} B mapped) after both blocks were freed; baseline was {} B ({} B mapped)",
                used,
                mapped,
                base_used,
                base_mapped
            );
            ok = false;
        }
    }
    ok
}

/// Chunks must come back to the frame allocator once their whole span is free —
/// including when two chunks happen to be physically adjacent.
///
/// The free list is a list of addresses, so "the block after this one" can belong
/// to a *different* chunk: the frame allocator hands out contiguous runs, and two
/// chunks taken one after the other are usually neighbours. Merging such blocks
/// produces one free block larger than a chunk, and the whole-chunk test in
/// `release_free_chunks` then never matches — the frames stay allocated forever
/// while the heap reports zero bytes in use. The ext2 suite found exactly that
/// (three chunks mapped, nothing used); this routine keeps it found.
fn heap_chunks() -> bool {
    let mut ok = true;
    let (base_used, base_mapped, base_chunks) = crate::heap_stats();

    // One chunk each: 60 KiB fits inside a 64 KiB chunk but leaves no room for a
    // second one, so three live blocks force three chunks. (`try_reserve_exact`
    // keeps an out-of-memory failure a test result instead of a panic.)
    const BIG: usize = 60 * 1024;
    let mut held: Vec<Vec<u8>> = Vec::new();
    for i in 0..3u8 {
        let mut block: Vec<u8> = Vec::new();
        if block.try_reserve_exact(BIG).is_err() {
            crate::kprintln!("[selftest] the heap refused a {BIG}-byte allocation");
            ok = false;
            break;
        }
        block.resize(BIG, i);
        held.push(block);
    }
    let (_, mapped_mid, chunks_mid) = crate::heap_stats();
    if mapped_mid < base_mapped + 3 * crate::memory::heap::CHUNK_SIZE
        || chunks_mid < base_chunks + 3
    {
        crate::kprintln!(
            "[selftest] three large blocks did not grow the heap as expected: {} B mapped, {} chunk(s)",
            mapped_mid,
            chunks_mid
        );
        ok = false;
    }
    // Contents check: the allocator must not have aliased the three blocks.
    for (i, block) in held.iter().enumerate() {
        if block.iter().any(|byte| *byte != i as u8) {
            crate::kprintln!("[selftest] block {i} was aliased by another allocation");
            ok = false;
        }
    }

    drop(held);
    let (used_after, mapped_after, chunks_after) = crate::heap_stats();
    if (used_after, mapped_after, chunks_after) != (base_used, base_mapped, base_chunks) {
        crate::kprintln!(
            "[selftest] the heap kept {} B / {} B mapped / {} chunk(s) after the blocks were dropped (baseline {} B / {} B / {})",
            used_after,
            mapped_after,
            chunks_after,
            base_used,
            base_mapped,
            base_chunks
        );
        ok = false;
    }
    ok
}

/// Map a fresh frame at an unused virtual address, reach that memory *through* the
/// new mapping, and take the map apart again.
///
/// The order of the checks is the diagnosis: the translation is verified before a
/// byte is written (a wrong physical address would otherwise present itself as a
/// bad pattern), the same words are then read back through both names, and the
/// routine ends by requiring the free-frame count to be exactly what it was —
/// which is also the check that `unmap` handed the intermediate tables back
/// instead of leaving one frame behind per mapping.
fn sv39_map_roundtrip() -> bool {
    let (before, _) = pmm::stats();
    let Some(frame) = pmm::alloc_frame() else {
        crate::kprintln!("[selftest] no free frame for the Sv39 round trip");
        return false;
    };
    let mut ok = true;

    if !paging::active() {
        crate::kprintln!("[selftest] satp does not select Sv39: the map is not in use");
        pmm::free_frame(frame);
        return false;
    }

    if let Err(error) = paging::map(TEST_VA, frame, Flags::kernel_rw()) {
        crate::kprintln!("[selftest] map({TEST_VA:#x}, {frame:#x}) failed: {error}");
        pmm::free_frame(frame);
        return false;
    }
    match paging::translate(TEST_VA) {
        Some(pa) if pa == frame => {}
        other => {
            crate::kprintln!("[selftest] {TEST_VA:#x} translates to {other:?}, not {frame:#x}");
            ok = false;
        }
    }

    // Refusals, because a map that overwrites is worse than one that fails: a
    // second map of a live page must not silently replace the first (whoever
    // mapped it would lose the memory), an unaligned address is not a page, and
    // unmapping an address nothing mapped is not a success.
    if !matches!(
        paging::map(TEST_VA, frame, Flags::kernel_rw()),
        Err(MapError::AlreadyMapped(_))
    ) {
        crate::kprintln!("[selftest] a second map of {TEST_VA:#x} was not refused");
        ok = false;
    }
    if !matches!(
        paging::map(TEST_VA + 1, frame, Flags::kernel_rw()),
        Err(MapError::Misaligned(_))
    ) {
        crate::kprintln!(
            "[selftest] a misaligned map of {:#x} was not refused",
            TEST_VA + 1
        );
        ok = false;
    }
    if !matches!(paging::unmap(UNMAPPED_VA), Err(MapError::NotMapped(_))) {
        crate::kprintln!("[selftest] unmap({UNMAPPED_VA:#x}) of an unmapped page did not fail");
        ok = false;
    }

    // The same words through both names: the virtual address proves the walk, the
    // physical address proves the mapping points at *this* frame rather than at a
    // second one that happens to hold the same pattern.
    // SAFETY: `TEST_VA` was just mapped to `frame` read/write above, and `frame` is
    // owned by this routine (it came from the PMM and is freed below), so both
    // pointers are valid for the [`MAPPED_WORDS`] words written and read here —
    // well inside the one mapped page. Volatile because the point is the memory,
    // not the values: a compiler that saw no reader could drop the store.
    unsafe {
        let through_va = TEST_VA as *mut u64;
        let through_pa = frame as *const u64;
        for i in 0..MAPPED_WORDS {
            through_va.add(i).write_volatile(MAP_PATTERN ^ i as u64);
        }
        for i in 0..MAPPED_WORDS {
            let expected = MAP_PATTERN ^ i as u64;
            if through_va.add(i).read_volatile() != expected {
                crate::kprintln!("[selftest] word {i} of {TEST_VA:#x} did not read back");
                ok = false;
                break;
            }
            if through_pa.add(i).read_volatile() != expected {
                crate::kprintln!(
                    "[selftest] word {i} of frame {frame:#x} differs from {TEST_VA:#x}"
                );
                ok = false;
                break;
            }
        }
    }

    match paging::unmap(TEST_VA) {
        Ok(pa) if pa == frame => {}
        other => {
            crate::kprintln!("[selftest] unmap({TEST_VA:#x}) returned {other:?}, not {frame:#x}");
            ok = false;
        }
    }
    if paging::translate(TEST_VA).is_some() {
        crate::kprintln!("[selftest] {TEST_VA:#x} still translates after unmap");
        ok = false;
    }

    pmm::free_frame(frame);
    let (after, _) = pmm::stats();
    if after != before {
        crate::kprintln!(
            "[selftest] the Sv39 round trip ends with {} free frames, {} before it started",
            after,
            before
        );
        ok = false;
    }
    ok
}

/// The identity map, checked where the rest of the kernel depends on it: a kernel
/// address translates to itself, the console's page is mapped even though it is
/// outside RAM, and an address nothing mapped answers `None` rather than a
/// plausible wrong address.
fn sv39_translate() -> bool {
    let mut ok = true;

    // A kernel address: the last page of the image, which the CPU is executing and
    // printing from right now.
    let kernel_va = crate::memory::kernel_end() - FRAME_SIZE;
    match paging::translate(kernel_va) {
        Some(pa) if pa == kernel_va => {}
        other => {
            crate::kprintln!("[selftest] kernel address {kernel_va:#x} translates to {other:?}");
            ok = false;
        }
    }

    // MMIO: outside RAM, so it is mapped only because `paging::init` was told about
    // the device window — the check that the console survived the switch.
    let uart = crate::drivers::uart::BASE;
    match paging::translate(uart) {
        Some(pa) if pa == uart => {}
        other => {
            crate::kprintln!("[selftest] the console page {uart:#x} translates to {other:?}");
            ok = false;
        }
    }

    // Unmapped: an address in Sv39's lower half that nothing claims. `None` is the
    // only honest answer; a translation here would mean the walk invented one.
    if let Some(pa) = paging::translate(UNMAPPED_VA) {
        crate::kprintln!("[selftest] unmapped {UNMAPPED_VA:#x} translates to {pa:#x}");
        ok = false;
    }

    // W^X, machine-checked instead of asserted in a comment: the image — which the
    // CPU is executing from — is writable *and* executable, while the first frame
    // above `__kernel_end`, the kind the allocator hands out, is writable and
    // **not** executable.
    match paging::flags(kernel_va) {
        Some(bits) if bits.bits() & mmu::PTE_X != 0 && bits.bits() & mmu::PTE_W != 0 => {}
        other => {
            crate::kprintln!(
                "[selftest] the kernel image page {kernel_va:#x} is not RWX: {:x?}",
                other.map(|f| f.bits())
            );
            ok = false;
        }
    }
    let allocatable = crate::memory::kernel_end();
    match paging::flags(allocatable) {
        Some(bits) if bits.bits() & mmu::PTE_W != 0 && bits.bits() & mmu::PTE_X == 0 => {}
        other => {
            crate::kprintln!(
                "[selftest] the allocatable page {allocatable:#x} is not RW without X: {:x?}",
                other.map(|f| f.bits())
            );
            ok = false;
        }
    }
    ok
}
