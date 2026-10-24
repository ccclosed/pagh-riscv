//! The kernel's Sv39 address space: the map the kernel runs on.
//!
//! `arch::riscv64::mmu` describes the *format*; this module decides *content*:
//! which virtual addresses exist, what they point at, and when the CPU starts
//! using them. One address space, built once at boot — a kernel-only identity
//! map — because nothing in M2 has a second one to switch to.
//!
//! Why identity (virtual == physical) rather than a higher-half kernel: the frame
//! allocator hands out physical addresses and the heap hands them to `alloc` as
//! pointers, so identity is the one mapping under which every existing line of
//! `memory` stays correct. It is also the mapping a `panic = "abort"` kernel can
//! debug: every address in a trap frame is the address in the log.
//!
//! What the map says, and why each part is shaped that way:
//!
//! * `[RAM base, __kernel_end)` — **RWX**. The CPU fetches instructions here
//!   (`.text`, and the firmware runtime below the image) and writes here (`.data`,
//!   `.bss`, the boot stack, the frame bitmap), and both live in the same pages,
//!   so one flag set has to cover both. Per-section W^X needs section symbols the
//!   linker script does not export; it is named as a limitation, not pretended.
//! * `[__kernel_end, RAM end)` — **RW, no X**. Every frame the allocator can hand
//!   out lives here, and this is where W^X actually holds: page tables, heap
//!   chunks, a driver's DMA buffer and a future user page's backing frame are all
//!   non-executable. The boundary is the one address the linker already exports,
//!   so the split costs nothing.
//! * the console UART's page — **RW, no X**, and the reason the map cannot stop at
//!   RAM: `virt` puts the NS16550 at `0x1000_0000`, and the moment `satp` is
//!   written, every printed line would fault without it. Device windows are
//!   mapped when a driver touches them *after* the switch; `fw_cfg` is not in the
//!   list because the console probes it during bring-up, before there is a map to
//!   fault against.
//!
//! The device tree needs no mapping of its own: firmware puts the blob inside
//! RAM, the identity map covers it, and the frames it occupies are reserved in
//! the frame allocator so nothing overwrites it while the kernel reads it.
//!
//! Locking: every entry point runs with interrupts masked (see [`with_space`]).
//! The spinlock alone would not be enough on a single hart — a trap taken while
//! it is held would spin forever on the context it interrupted — and a walk that
//! allocates tables is not a short section either. Lock order is address space,
//! then frame allocator; the frame allocator never calls back here.

use crate::arch::riscv64::csr;
use crate::arch::riscv64::mmu::{self, Flags, PageTable};
use crate::memory::pmm;
use crate::memory::FRAME_SIZE;
use crate::sync::Spinlock;

/// Why a mapping request was refused, or the space could not be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapError {
    /// `init` has not run: there is no root table to walk.
    NotInitialised,
    /// The device tree gave no usable RAM range, so there is nothing to map.
    NoMemory,
    /// A virtual or physical address is not 4 KiB aligned.
    Misaligned(usize),
    /// `va` is not a canonical Sv39 address (bits 63..39 must repeat bit 38).
    NotCanonical(usize),
    /// Something is already mapped at `va`. `map` never replaces a mapping: a
    /// silent replacement would leave whoever made the first one using memory
    /// that is no longer theirs.
    AlreadyMapped(usize),
    /// Nothing is mapped at `va`.
    NotMapped(usize),
    /// A table on the way down is a leaf — a 2 MiB or 1 GiB superpage. This
    /// kernel creates none, so meeting one means the walk is looking at a table
    /// someone else built.
    Superpage(usize),
    /// The frame allocator had nothing to give for a new table.
    OutOfFrames,
}

impl core::fmt::Display for MapError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            MapError::NotInitialised => write!(f, "the address space has not been built"),
            MapError::NoMemory => write!(f, "the device tree reports no usable RAM"),
            MapError::Misaligned(address) => write!(f, "{address:#x} is not 4 KiB aligned"),
            MapError::NotCanonical(address) => {
                write!(f, "{address:#x} is not a canonical Sv39 address")
            }
            MapError::AlreadyMapped(address) => write!(f, "{address:#x} is already mapped"),
            MapError::NotMapped(address) => write!(f, "{address:#x} is not mapped"),
            MapError::Superpage(address) => {
                write!(f, "{address:#x} is behind a superpage mapping")
            }
            MapError::OutOfFrames => write!(f, "the frame allocator is empty"),
        }
    }
}

/// The address space's mutable state: the root, and what it holds.
struct Space {
    /// Physical address of the root table; `0` means "not built yet".
    root: usize,
    /// Identity-mapped RAM as installed, for the report line.
    identity: (usize, usize),
    /// Where the identity map stops being executable: below it the kernel image
    /// and firmware (RWX), from it up the allocatable frames (RW).
    executable_below: usize,
    /// Pages mapped outside RAM — today only the console UART.
    device_pages: usize,
    /// Live 4 KiB mappings, counted so the report line can state the size of the
    /// map without walking three levels of tables.
    mappings: usize,
}

impl Space {
    const fn new() -> Self {
        Space {
            root: 0,
            identity: (0, 0),
            executable_below: 0,
            device_pages: 0,
            mappings: 0,
        }
    }
}

static SPACE: Spinlock<Space> = Spinlock::new(Space::new());

/// A read-only view of the address space, for the report lines and the tests.
#[derive(Clone, Copy)]
pub struct Snapshot {
    /// Physical address of the root table.
    pub root: usize,
    /// The `satp` value this root produces (written at activation).
    pub satp: usize,
    /// Identity-mapped `(start, end)`.
    pub identity: (usize, usize),
    /// Address where the identity map stops being executable.
    pub executable_below: usize,
    /// Live 4 KiB mappings.
    pub mappings: usize,
    /// Pages mapped outside RAM.
    pub device_pages: usize,
}

/// Run `f` with the address space locked and interrupts masked.
///
/// The masking is the point, not decoration: the lock is not reentrant, this
/// kernel is single-hart, and a walk allocates frames — so a trap taken while the
/// lock is held (the M3 timer, or any handler that later maps something) would
/// spin forever waiting for the context it interrupted to be resumed.
/// `without_interrupts` restores the previous state, so nesting this inside
/// another masked section is harmless.
fn with_space<R>(f: impl FnOnce(&mut Space) -> R) -> R {
    csr::without_interrupts(|| f(&mut SPACE.lock()))
}

/// Build the kernel address space and turn it on.
///
/// `memory` is the device tree's `/memory` range, `kernel_end` the linker symbol
/// after the image, and `devices` the `(base, length)` of every MMIO window the
/// kernel touches *after* the switch — today the console UART, because it is the
/// one device the kernel keeps using (a later driver adds its own window here
/// when it exists to use it).
///
/// A failure is fatal at the only call site (boot): a kernel that cannot build
/// the map it is about to switch to has nowhere to run. The partially built
/// tables are therefore left as they are — a rollback nobody ever exercises is
/// worse than frames held by a hart that is about to park.
pub fn init(
    memory: (usize, usize),
    kernel_end: usize,
    devices: &[(usize, usize)],
) -> Result<(), MapError> {
    let ram_start = memory.0 & !(FRAME_SIZE - 1);
    let ram_end = memory.1 & !(FRAME_SIZE - 1);
    if ram_end <= ram_start {
        return Err(MapError::NoMemory);
    }

    let Some(root) = pmm::alloc_frame() else {
        return Err(MapError::OutOfFrames);
    };
    // SAFETY: `root` is a fresh frame from the allocator — 4 KiB aligned, owned
    // by this address space, and referenced by nothing else yet. The walker must
    // never see a table with a stale entry in it.
    unsafe { PageTable::zeroed_at(root) };

    // The map is built into a local `Space` and copied into the published one
    // only when it is complete, so no context can walk a half-built tree. Boot is
    // single-threaded, but the state's invariant should not depend on that.
    let mut space = Space::new();
    space.root = root;
    let split = kernel_end.clamp(ram_start, ram_end);
    install_range(&mut space, ram_start, split, Flags::kernel_rwx())?;
    if split < ram_end {
        install_range(&mut space, split, ram_end, Flags::kernel_rw())?;
    }
    for &(base, length) in devices {
        if length == 0 {
            continue;
        }
        // Rounded outward: a device register file that starts or ends mid-page
        // still owns the page it touches.
        let start = base & !(FRAME_SIZE - 1);
        let end = (base + length + FRAME_SIZE - 1) & !(FRAME_SIZE - 1);
        install_range(&mut space, start, end, Flags::device_rw())?;
        space.device_pages += (end - start) / FRAME_SIZE;
    }
    space.identity = (ram_start, ram_end);
    space.executable_below = split;

    // Switch first, publish second: between the two the map is live but invisible
    // to `snapshot`, and nothing observes that gap (interrupts are masked at the
    // call site, and the only other reader is the report line below).
    mmu::activate(root);
    let mappings = space.mappings;
    let device_pages = space.device_pages;
    with_space(|live| {
        live.root = root;
        live.identity = (ram_start, ram_end);
        live.executable_below = split;
        live.mappings = mappings;
        live.device_pages = device_pages;
    });
    Ok(())
}

/// Map `[start, end)` identity (`va == pa`) with `flags`, one page at a time.
fn install_range(
    space: &mut Space,
    start: usize,
    end: usize,
    flags: Flags,
) -> Result<(), MapError> {
    let mut address = start;
    while address < end {
        install(space, address, address, flags)?;
        address += FRAME_SIZE;
    }
    Ok(())
}

/// Map one page: walk (creating tables as needed) and write the leaf entry.
///
/// Called with the space already owned by the caller — either the local `Space`
/// of [`init`] or the locked one from [`map`].
fn install(space: &mut Space, va: usize, pa: usize, flags: Flags) -> Result<(), MapError> {
    if space.root == 0 {
        return Err(MapError::NotInitialised);
    }
    let mut table = space.root as *mut PageTable;
    // The two levels that can create a table, and the entry each new table was
    // linked from, so a failure can undo exactly what it did: `map` promises
    // "nothing changed" when it returns an error, and a walk that leaks frames on
    // an empty pool would make that promise a lie.
    let mut created = [(0usize, 0usize, 0usize); mmu::LEVELS - 1];
    let mut created_count = 0usize;

    for level in (1..mmu::LEVELS).rev() {
        let index = mmu::vpn(va, level);
        // SAFETY: `table` is the root of this space or a table reached through a
        // valid table entry — always a live, 4 KiB-aligned table owned here — and
        // `index` is `< 512` by construction of `vpn`.
        let entry = unsafe { PageTable::entry(table, index) };
        if mmu::pte_is_leaf(entry) {
            return Err(rollback(created, created_count, MapError::Superpage(va)));
        }
        if mmu::pte_is_valid(entry) {
            table = mmu::pte_address(entry) as *mut PageTable;
            continue;
        }
        let Some(frame) = pmm::alloc_frame() else {
            return Err(rollback(created, created_count, MapError::OutOfFrames));
        };
        // SAFETY: a fresh frame from the allocator: aligned, owned, unreferenced.
        let child = unsafe { PageTable::zeroed_at(frame) };
        // SAFETY: `table` is live (checked above) and `index` is in range. The
        // child is complete before it is linked, so a walk never sees a
        // half-built table.
        unsafe { PageTable::set_entry(table, index, mmu::pte_table(frame)) };
        created[created_count] = (table as usize, index, frame);
        created_count += 1;
        table = child;
    }

    let index = mmu::vpn(va, 0);
    // SAFETY: as in the loop: `table` is live and `index` is in range.
    let entry = unsafe { PageTable::entry(table, index) };
    if mmu::pte_is_valid(entry) {
        return Err(rollback(
            created,
            created_count,
            MapError::AlreadyMapped(va),
        ));
    }
    // SAFETY: as above, plus: the entry is free, so this write is the only claim
    // on `va`. The volatile store, plus the `sfence.vma` in `map` (or the one in
    // `activate`, during `init`), is what publishes it to the walker.
    unsafe { PageTable::set_entry(table, index, mmu::pte_leaf(pa, flags)) };
    space.mappings += 1;
    Ok(())
}

/// Unlink the tables a failed [`install`] created and hand back the error.
///
/// Only the tables *this call* linked are removed, deepest first. Each is
/// unlinked from its parent before its frame is freed, and the subtree under it
/// holds nothing but the deeper table this loop removes next — so no live mapping
/// can be left pointing into freed memory.
fn rollback(
    created: [(usize, usize, usize); mmu::LEVELS - 1],
    count: usize,
    error: MapError,
) -> MapError {
    for &(parent, index, frame) in created[..count].iter().rev() {
        // SAFETY: each entry was linked by this same call from a live parent
        // table, and nothing else can have written it since (the space is owned
        // by the caller for the duration).
        unsafe { PageTable::set_entry(parent as *mut PageTable, index, 0) };
        pmm::free_frame(frame);
    }
    error
}

/// Map one 4 KiB page. `Err` when the address is unusable or already mapped.
///
/// With the map active the translation of `va` is flushed before returning, so
/// the caller can use the address immediately; on error nothing changed, and the
/// flush is skipped because there is nothing new to publish.
pub fn map(va: usize, pa: usize, flags: Flags) -> Result<(), MapError> {
    if va % FRAME_SIZE != 0 || pa % FRAME_SIZE != 0 {
        return Err(MapError::Misaligned(if va % FRAME_SIZE != 0 {
            va
        } else {
            pa
        }));
    }
    if !mmu::is_canonical(va) {
        return Err(MapError::NotCanonical(va));
    }
    let result = with_space(|space| install(space, va, pa, flags));
    if result.is_ok() {
        mmu::sfence_vma_page(va);
    }
    result
}

/// Remove the mapping at `va`, returning the physical address it pointed at.
///
/// Tables that become empty are handed back to the frame allocator, so a
/// map/unmap pair leaves the pool exactly as it found it — the property the
/// self-test's free-frame ledger measures.
pub fn unmap(va: usize) -> Result<usize, MapError> {
    if va % FRAME_SIZE != 0 {
        return Err(MapError::Misaligned(va));
    }
    with_space(|space| {
        if space.root == 0 {
            return Err(MapError::NotInitialised);
        }
        // The path down: `tables[depth]`, and the index in its parent that
        // reaches it, so an empty table can be unlinked from the right entry.
        let mut tables = [0usize; mmu::LEVELS];
        let mut indices = [0usize; mmu::LEVELS];
        let mut depth = 0usize;
        tables[0] = space.root;
        for level in (1..mmu::LEVELS).rev() {
            let index = mmu::vpn(va, level);
            // SAFETY: `tables[depth]` is the root or a table reached through a
            // valid entry, exactly as in `install`; `index` is `< 512`.
            let entry = unsafe { PageTable::entry(tables[depth] as *const PageTable, index) };
            if mmu::pte_is_leaf(entry) {
                return Err(MapError::Superpage(va));
            }
            if !mmu::pte_is_valid(entry) {
                return Err(MapError::NotMapped(va));
            }
            depth += 1;
            tables[depth] = mmu::pte_address(entry);
            indices[depth] = index;
        }

        let index = mmu::vpn(va, 0);
        // SAFETY: as above: live table, in-range index.
        let leaf = unsafe { PageTable::entry(tables[depth] as *const PageTable, index) };
        if !mmu::pte_is_valid(leaf) {
            return Err(MapError::NotMapped(va));
        }
        let pa = mmu::pte_address(leaf);
        // SAFETY: the entry is live and this walk owns the tables it reaches.
        unsafe { PageTable::set_entry(tables[depth] as *mut PageTable, index, 0) };
        space.mappings -= 1;

        // Free every table that just became empty, leaf first. The root is never
        // freed: `depth > 0` is the guard, and it is the only thing stopping this
        // from handing the whole address space back to the allocator.
        while depth > 0 && table_is_empty(tables[depth]) {
            let frame = tables[depth];
            // SAFETY: `tables[depth - 1]` is a live parent of `tables[depth]`, and
            // `indices[depth]` is the entry that links them.
            unsafe { PageTable::set_entry(tables[depth - 1] as *mut PageTable, indices[depth], 0) };
            pmm::free_frame(frame);
            depth -= 1;
        }
        mmu::sfence_vma_page(va);
        Ok(pa)
    })
}

/// True when every entry of the table at `table` is absent.
fn table_is_empty(table: usize) -> bool {
    for index in 0..mmu::ENTRIES_PER_TABLE {
        // SAFETY: `table` is a live table of this address space (the caller is
        // inside the walk) and `index` is in range.
        if mmu::pte_is_valid(unsafe { PageTable::entry(table as *const PageTable, index) }) {
            return false;
        }
    }
    true
}

/// The physical address `va` translates to, or `None` when it is not mapped.
///
/// Read-only, but it takes the same lock and mask as the writers: a trap must not
/// be able to walk the tables in the middle of a leaf being installed, and the
/// lock is not reentrant.
pub fn translate(va: usize) -> Option<usize> {
    with_space(|space| walk(space, va).map(|(pa, _)| pa))
}

/// How `va` is mapped — the permission bits of its leaf entry — or `None` when it
/// is not mapped.
///
/// The companion to [`translate`]: one answers "where does this go", this one
/// "what may be done with it", and together they are what makes the map's W^X
/// claim checkable from outside the module instead of asserted in a comment.
pub fn flags(va: usize) -> Option<Flags> {
    with_space(|space| {
        walk(space, va).map(|(_, entry)| Flags::from_bits(mmu::pte_permissions(entry)))
    })
}

/// Walk the tables to the leaf covering `va`: `(physical address, entry bits)`.
fn walk(space: &Space, va: usize) -> Option<(usize, u64)> {
    if space.root == 0 {
        return None;
    }
    let mut table = space.root;
    for level in (1..mmu::LEVELS).rev() {
        // SAFETY: `table` is the live root or a table reached through a valid
        // entry; `vpn` keeps the index in range.
        let entry = unsafe { PageTable::entry(table as *const PageTable, mmu::vpn(va, level)) };
        if !mmu::pte_is_valid(entry) {
            return None;
        }
        if mmu::pte_is_leaf(entry) {
            // A superpage translates the bits below its level itself. This kernel
            // creates none, but reading one back is free, and reporting "unmapped"
            // for memory that is mapped would be a lie in the one function whose
            // job is to tell.
            let offset_bits = mmu::PAGE_SHIFT + level * mmu::INDEX_BITS;
            return Some((
                mmu::pte_address(entry) | (va & ((1usize << offset_bits) - 1)),
                entry,
            ));
        }
        table = mmu::pte_address(entry);
    }
    // SAFETY: as above, at the leaf level.
    let entry = unsafe { PageTable::entry(table as *const PageTable, mmu::vpn(va, 0)) };
    if !mmu::pte_is_leaf(entry) {
        return None;
    }
    Some((mmu::pte_address(entry) | (va & (FRAME_SIZE - 1)), entry))
}

/// True when this hart is translating through the map built here.
///
/// Reads `satp`, not this module's state: the question the report asks is what
/// the hardware is doing, and a flag in a struct cannot answer it.
pub fn active() -> bool {
    mmu::paging_enabled()
}

/// A snapshot of the address space, or `None` before [`init`].
pub fn snapshot() -> Option<Snapshot> {
    with_space(|space| {
        if space.root == 0 {
            return None;
        }
        Some(Snapshot {
            root: space.root,
            satp: mmu::satp_for(space.root),
            identity: space.identity,
            executable_below: space.executable_below,
            mappings: space.mappings,
            device_pages: space.device_pages,
        })
    })
}
