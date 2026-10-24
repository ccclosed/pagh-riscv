# memory — physical frames and the kernel heap

Two allocators and the boundary between them: `pmm.rs` owns **physical frames**,
`heap.rs` owns **bytes**. Nothing above them owns addresses yet — this milestone has no
paging, so a physical address *is* a pointer (§ [Not implemented](#not-implemented)).

## Frames — `pmm.rs`

| | |
|---|---|
| Unit | 4 KiB (`memory::FRAME_SIZE`), the granule Sv39 will map |
| Pool | the device tree's `/memory` node, rounded outward to frames |
| Table | a bitmap, 1 bit per frame, covering physical addresses `0..4 GiB` |
| Strategy | first fit from a next-fit cursor, run-length scan |
| API | `alloc_frame`, `free_frame`, `alloc_frames_contiguous`, `free_frames_contiguous`, `stats`, `pool_range` |

`run.sh` boots `virt` with 256 MiB, so the pool is 65536 frames. The bitmap's window is
4 GiB — 128 KiB of `.bss`, paid whether the machine uses it or not — because that is what
a frame index can reach in one `usize` step. RAM above the window is reported at boot and
left unusable rather than half-tracked.

**Polarity: a set bit means free.** A zero-filled `.bss` then means "nothing is
allocatable yet", so an allocation before `init` fails instead of handing back the
kernel's own image — with no `initialized` flag to forget to check. Bits for addresses
outside the pool are never set, so `free_bits.count_ones()` *is* the free-frame count;
the counter is a cache of that fact, not a second source of truth.

**Reserved at boot** (`memory::init(memory, reserved)`, called from `kmain` after the
device tree is parsed and before anything allocates):

| Range | Why it must never be handed out |
|---|---|
| `(0, __kernel_end)` | OpenSBI's runtime and the whole kernel image — `.text`, `.rodata`, `.data`, `.bss`, which is where the boot stack and this bitmap live |
| `__stack_bottom..__stack_top` | named explicitly so that a linker script which ever moves the stack out of `.bss` cannot silently donate it to the allocator |
| the device tree blob | firmware wrote it and the kernel reads it all boot; its size is the header's `totalsize` field, not a guessed window |

Everything else inside `/memory` is free — including memory firmware may be using for
something the device tree does not describe. That is the assumption this machine's
contract offers: *everything in `/memory` that you are not told to keep is yours*. On
`virt` + OpenSBI it holds; a machine with hidden reservations (a secure monitor, a
framebuffer the firmware keeps alive) would need a `/reserved-memory` reader, which is
not implemented.

## Heap — `heap.rs`

A free-list allocator that grows in **64 KiB chunks**, each chunk 16 consecutive frames
from the PMM (`alloc_frames_contiguous` — the reason that entry point exists).

* Blocks are `[16-byte header | payload]`, all sizes multiples of 16, so every payload is
  16-byte aligned. `size` counts the header; `next` links free blocks only, sorted by
  address.
* First fit, split when the remainder is worth its own header (≥ 32 B). On free the block
  is inserted in address order and **merged with both neighbours** — merging on every
  insert is what makes the next property a lookup instead of a walk.
* **Bad frees are absorbed, not fatal.** A block that is already on the free list (a double
  free) or an address outside every owned chunk is reported on the serial line and changes
  nothing. Both checks run *before* any counter moves: with the subtraction first, a debug
  build would panic (overflow check) and the report would never be printed. `used` is
  decremented with `saturating_sub` for the same reason — an allocator must not panic, and
  this kernel cannot unwind.
* **Chunks are given back.** When a chunk's whole 64 KiB is one free block, the chunk is
  returned to the PMM. This is what makes the heap invisible in the frame ledger when
  nothing is allocated, and therefore what makes `selftest` idempotent to the frame.
* Lock order: **heap, then PMM** (`grow` and `release_free_chunks` take a chunk while the
  heap lock is held). The PMM never calls into the heap, so the reverse cannot happen.
* Refusals, not guesses: a request aligned above 16 bytes returns null (16 is the
  strongest alignment the layout promises; a wrong-alignment pointer is corruption nobody
  sees), a request larger than a chunk returns null, and `realloc` is the trait's default
  (alloc + copy + free) — in-place growth is not worth its bug surface yet.
* Cap: `MAX_CHUNKS = 64`, i.e. 4 MiB mapped at most. A runaway allocation surfaces as a
  null return (a panic on the serial line) instead of eating the machine.

The `#[global_allocator]` claim itself is in `src/main.rs`; `memory::heap::Heap` is the
allocator, so its state has exactly one owner.

## Locking

Both allocators sit behind `sync::Spinlock`, and every lock site takes it with interrupts
masked (`csr::without_interrupts`, through the private `lock()` helper each module has).
That is not decoration: `traps::init` enables `sstatus.SIE`, the lock is not reentrant, and
this is a single-hart kernel — a trap handler that allocated a frame, freed one, or even
called `pmm::stats()` while an interrupted context held the lock would spin forever,
because the interrupted context cannot run again to release it. Masking is nestable
(`without_interrupts` restores the state it found), which matters here: `heap::alloc` takes
the PMM lock inside the heap lock. Lock order is therefore **heap, then PMM**, and the PMM
never calls into the heap.

## Testing

Two shell commands and one ledger:

```text
pagh:rv64> mem
memory 0x80000000..0x90000000 (256 MiB)
pmm    62940 free of 65536 frames (256 MiB), pool 0x80000000..0x90000000
heap   0 B used, 0 B mapped from the pool in 0 chunk(s) of 64 KiB
pagh:rv64> selftest
[selftest] free frames before: 62940 of 65536
SELFTEST frames-roundtrip PASS
SELFTEST frame-pattern PASS
[selftest] heap: 8256 B in use, 65536 B mapped, 1 chunk(s)
SELFTEST heap-vec PASS
heap: double free of 0x80a73090 ignored
heap: refusing to free 0x802302b8: not a heap block
SELFTEST heap-defenses PASS
SELFTEST SUMMARY: 4/4 passed
[selftest] PMM hygiene: free frames 62940 -> 62940
```

`selftest` is non-destructive, deterministic and **idempotent**: every frame and heap byte
it takes is given back, including on its failure paths. The `PMM hygiene` line is the
verdict that matters — running `selftest` twice in one boot must print the same
`before -> after` pair, and a routine that retains frames makes the two runs differ. (On
the x86 kernel exactly this ledger caught an ELF fuzz routine draining the pool.)

Four routines, and one of them is deliberately impolite: `heap-defenses` calls
`GlobalAlloc::dealloc` twice on the same block and once on a stack address — the trait
calls both undefined behaviour, and the routine exists to prove that *this* allocator
defines them as "report and change nothing". If that defense ever regressed, the two lines
above would not appear; the run would die inside the allocator instead.

## Not implemented

* **Paging.** Sv39 is on, but the map is the kernel's alone: identity, 4 KiB pages,
  no user bit, `satp.ASID = 0` (`src/memory/paging.rs`). Before that landing `satp`
  was untouched and the kernel ran on the mapping OpenSBI
  leaves behind, so physical addresses are dereferenced directly. Everything here is
  already in the shape the VMM will need (4 KiB frames, physical addresses), but there is
  no virtual→physical translation, no per-process address space and no guard pages.
* **Frame stealing / swap / reclaim.** Once a frame is handed out it is out until freed;
  there is no way to take one back from another owner.
* **NUMA and multi-hart.** One pool, one bitmap, one lock; nothing is per-node or
  per-hart.
* **Per-frame reference counts.** Nothing shares a frame yet, so there is no COW and no
  `cow_ref`/`cow_unref` equivalent.
* **`/reserved-memory`.** The reserved list is what `kmain` hands in; the device tree's own
  reservation entries are ignored.
* **Heap shrinking.** A partially used chunk is kept; only wholly free chunks are
  returned. There is no compaction and no large-allocation path beyond one chunk.