//! Sv39: the page-table format, and the two instructions that use it.
//!
//! RISC-V names its paging schemes after their virtual-address width: Sv39
//! translates 39-bit virtual addresses (512 GiB of them) through three levels of
//! 512-entry tables, with 4 KiB at the leaf. This file owns the *encoding* —
//! entry bits, the address split, `satp`, `sfence.vma` — and nothing else. Which
//! tables exist and what lives in them is a property of this kernel, not of the
//! ISA, and it lives one layer up in `memory::paging`; keeping the two apart is
//! what stops an architecture file from growing a frame allocator of its own.
//!
//! Three details that are easy to get subtly wrong:
//!
//! * **A and D are set when a mapping is created.** A leaf whose accessed or
//!   dirty bit is clear may fault on hardware that does not maintain those bits
//!   itself (the Svade extension), and the kernel would then have to fault, read
//!   `stval` and retry to learn something it already knew. Setting them up front
//!   costs nothing and is correct on both kinds of machine.
//! * **`G` is set on every supervisor mapping.** These mappings are identical in
//!   every address space this kernel will ever have, so marking them global is
//!   what a future user-space `satp` switch needs in order not to re-populate the
//!   kernel's half of the map on every switch.
//! * **Entry access is volatile, and `sfence.vma` is not `nomem`.** Page tables
//!   are read by the hardware walker, not by this program: a compiler that can
//!   prove nothing here reads an entry would be free to drop the store that
//!   fills it, and one that believes `sfence.vma` does not touch memory would be
//!   free to sink a page-table write past the fence that publishes it.
//!
//! Phase note, because the crate is a binary and unused items warn: this module
//! is the encoding half of M2b, and `memory::paging` — which lands with the boot
//! phase that turns the map on — is its only caller. The `allow` below exists
//! for that window alone and comes off with the caller; it is the only
//! `allow(dead_code)` in the tree.

#![allow(dead_code)]

use core::arch::asm;

use crate::arch::riscv64::csr;

/// The smallest unit Sv39 translates, and therefore the frame size.
pub const PAGE_SIZE: usize = 4096;

/// `log2(PAGE_SIZE)`: the address bits a leaf entry leaves untranslated.
pub const PAGE_SHIFT: usize = 12;

/// Index bits per level: 512 entries per table.
pub const INDEX_BITS: usize = 9;

/// Entries in one page table.
pub const ENTRIES_PER_TABLE: usize = 1 << INDEX_BITS;

/// Mask selecting one level's index out of a virtual address.
pub const INDEX_MASK: usize = ENTRIES_PER_TABLE - 1;

/// Levels in an Sv39 walk: root (level 2), middle (level 1), leaf (level 0).
pub const LEVELS: usize = 3;

/// Width of a Sv39 virtual address. Bits 63..39 must repeat bit 38.
pub const VA_BITS: usize = 39;

/// The `satp.MODE` field value that selects Sv39.
pub const SATP_MODE_SV39: usize = 8;

/// Physical address bits an entry's PPN field carries (44 bits, 16 TiB).
pub const PPN_BITS: usize = 44;

// Page-table entry bits, in the order the privileged spec lists them.
/// Valid: without it the entry is not there at all.
pub const PTE_V: u64 = 1 << 0;
/// Readable.
pub const PTE_R: u64 = 1 << 1;
/// Writable (and, without `PTE_R`, *only* writable — a valid but unusual leaf).
pub const PTE_W: u64 = 1 << 2;
/// Executable.
pub const PTE_X: u64 = 1 << 3;
/// Accessible from U-mode.
pub const PTE_U: u64 = 1 << 4;
/// Global: survives a `satp` switch that changes the ASID.
pub const PTE_G: u64 = 1 << 5;
/// Accessed; see the module docs for why this kernel sets it itself.
pub const PTE_A: u64 = 1 << 6;
/// Dirty; as `PTE_A`.
pub const PTE_D: u64 = 1 << 7;

/// Where the PPN starts inside an entry.
const PTE_PPN_SHIFT: u64 = 10;

/// The PPN field, once shifted down to bit 0.
const PTE_PPN_MASK: u64 = (1u64 << PPN_BITS) - 1;

/// How a mapping may be used, as page-table entry bits.
///
/// A newtype over `u64` rather than a struct of booleans because the encoding
/// *is* the interface to the hardware, and the constructors below are the only
/// mapping policies this kernel has. A page the kernel does not own (a user page,
/// a read-only page) is built with [`Flags::from_bits`] at the point where its
/// policy is decided, so the choice stays visible in the caller.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Flags(u64);

impl Flags {
    /// Supervisor read/write/execute, global: the kernel image and the firmware
    /// under it.
    ///
    /// Execute *and* write on the same page is not a statement about W^X — the
    /// kernel's text and its data share pages inside one image, and splitting
    /// them needs section symbols the linker script does not export yet. It is
    /// the honest description of "everything below `__kernel_end`".
    pub const fn kernel_rwx() -> Self {
        Flags(PTE_V | PTE_R | PTE_W | PTE_X | PTE_G | PTE_A | PTE_D)
    }

    /// Supervisor read/write, global, **not executable**: every frame the
    /// allocator can hand out.
    ///
    /// This is the W^X half of the kernel map that *is* cheap: no page the PMM
    /// gives to the heap, to a page table or to a future driver buffer can be
    /// executed.
    pub const fn kernel_rw() -> Self {
        Flags(PTE_V | PTE_R | PTE_W | PTE_G | PTE_A | PTE_D)
    }

    /// Device MMIO: read/write, global, never executable.
    ///
    /// A device register file has no business being an instruction, and the
    /// console UART keeps working after the switch only because its page is
    /// mapped — `virt` puts it at `0x1000_0000`, far outside RAM.
    pub const fn device_rw() -> Self {
        Flags(PTE_V | PTE_R | PTE_W | PTE_G | PTE_A | PTE_D)
    }

    /// Raw bits, for a mapping whose policy the constructors above do not cover.
    pub const fn from_bits(bits: u64) -> Self {
        Flags(bits)
    }

    /// The bits as the hardware wants them.
    pub const fn bits(self) -> u64 {
        self.0
    }
}

/// One Sv39 table: 512 eight-byte entries, one 4 KiB page.
///
/// The type is only ever a *view* of a frame. The kernel never constructs one by
/// value (a 4 KiB local would be a stack problem) and never allocates one from
/// the heap: a table is a frame from the frame allocator, and
/// [`PageTable::zeroed_at`] is the one bridge from a physical address to it. That
/// is also why the type carries no `Default` and no constructor.
#[repr(C, align(4096))]
pub struct PageTable {
    entries: [u64; ENTRIES_PER_TABLE],
}

impl PageTable {
    /// View `frame` as a page table and clear every entry.
    ///
    /// # Safety
    /// `frame` must be the physical address of a 4 KiB frame that this kernel
    /// owns for as long as any mapping refers to it, and no other reference to it
    /// may exist — zeroing it declares whatever was there dead.
    pub unsafe fn zeroed_at(frame: usize) -> *mut PageTable {
        let table = frame as *mut PageTable;
        // SAFETY: the caller guarantees the frame is ours and 4 KiB aligned, so
        // the 512 stores below stay inside it. Volatile because the reader is the
        // hardware walker: an entry that must be absent has to be absent in
        // memory, and a compiler that can prove nothing in this process reads
        // those words would otherwise be free to drop the stores.
        unsafe {
            let entries = core::ptr::addr_of_mut!((*table).entries).cast::<u64>();
            for index in 0..ENTRIES_PER_TABLE {
                core::ptr::write_volatile(entries.add(index), 0);
            }
        }
        table
    }

    /// Read entry `index`.
    ///
    /// # Safety
    /// `table` must be a live table pointer from [`PageTable::zeroed_at`], or one
    /// reached through a valid table entry, and `index` must be
    /// `< ENTRIES_PER_TABLE`.
    pub unsafe fn entry(table: *const PageTable, index: usize) -> u64 {
        // SAFETY: the caller's contract is exactly this read: `table` points at a
        // live 512-entry table and `index` is in range. Volatile for the reason
        // in `zeroed_at` — the walker updates A/D behind this program's back, and
        // a cached entry could be a stale one.
        unsafe {
            core::ptr::read_volatile(
                core::ptr::addr_of!((*table).entries)
                    .cast::<u64>()
                    .add(index),
            )
        }
    }

    /// Write entry `index`.
    ///
    /// # Safety
    /// As [`PageTable::entry`], plus: the caller must own the walk that reaches
    /// this table — it is the only writer, and the frame is not shared.
    pub unsafe fn set_entry(table: *mut PageTable, index: usize, entry: u64) {
        // SAFETY: as `entry`, with a mutable pointer the caller owns. Volatile so
        // the store cannot be reordered with the `sfence.vma` that publishes it
        // to the walker.
        unsafe {
            core::ptr::write_volatile(
                core::ptr::addr_of_mut!((*table).entries)
                    .cast::<u64>()
                    .add(index),
                entry,
            )
        }
    }
}

/// The index Sv39 takes from `va` at `level` (0 = leaf, 2 = root).
pub const fn vpn(va: usize, level: usize) -> usize {
    (va >> (PAGE_SHIFT + level * INDEX_BITS)) & INDEX_MASK
}

/// A leaf entry mapping the 4 KiB page at `pa`.
pub const fn pte_leaf(pa: usize, flags: Flags) -> u64 {
    (((pa as u64) >> PAGE_SHIFT) << PTE_PPN_SHIFT) | flags.bits()
}

/// A non-leaf entry pointing at the next table, which lives at `pa`.
///
/// Only `V` is set: an entry that points at a table *and* has R/W/X set would be
/// a leaf — a 2 MiB or 1 GiB superpage — which is a different promise than the
/// one this kernel makes.
pub const fn pte_table(pa: usize) -> u64 {
    (((pa as u64) >> PAGE_SHIFT) << PTE_PPN_SHIFT) | PTE_V
}

/// The physical address an entry points at (the page or superpage base).
pub const fn pte_address(pte: u64) -> usize {
    (((pte >> PTE_PPN_SHIFT) & PTE_PPN_MASK) as usize) << PAGE_SHIFT
}

/// True when the entry is present.
pub const fn pte_is_valid(pte: u64) -> bool {
    pte & PTE_V != 0
}

/// True when the entry maps memory (a leaf) rather than pointing at a table.
///
/// The spec's rule is "R, W or X is set"; `W` without `R` is reserved, but it is
/// still a leaf and treating it as one is the only honest reading.
pub const fn pte_is_leaf(pte: u64) -> bool {
    pte & PTE_V != 0 && pte & (PTE_R | PTE_W | PTE_X) != 0
}

/// True when `va` is a canonical Sv39 address.
///
/// Sv39 defines 39 bits of address, so bits 63..39 must repeat bit 38: the
/// "hole" between the two halves is not address space, and a table walk started
/// on one of those values would translate an address the program cannot mean.
pub const fn is_canonical(va: usize) -> bool {
    let upper = va >> (VA_BITS - 1);
    upper == 0 || upper == (1usize << (64 - VA_BITS + 1)) - 1
}

/// The `satp` value that selects Sv39 with `root` as its root table.
///
/// MODE in bits 63..60, ASID in 59..44, PPN in 43..0. The ASID stays 0: this
/// kernel has one address space, and a nonzero ASID would promise the hardware a
/// tag that nothing maintains yet.
pub const fn satp_for(root: usize) -> usize {
    (SATP_MODE_SV39 << 60) | ((root >> PAGE_SHIFT) & ((1usize << PPN_BITS) - 1))
}

/// Read `satp`.
pub fn read_satp() -> usize {
    let value: usize;
    // SAFETY: reading a CSR cannot fault from S-mode and has no memory side
    // effects; `satp` exists on every RV64 that has Sv39.
    unsafe { asm!("csrr {}, satp", out(reg) value, options(nomem, nostack)) };
    value
}

/// True when this hart is translating through `satp` (Sv39 mode), not bare.
///
/// The honest way to ask: it reads the CSR the hardware actually uses, so it
/// stays true no matter who wrote `satp` last.
pub fn paging_enabled() -> bool {
    read_satp() >> 60 == SATP_MODE_SV39
}

/// Install `root` as the active page table and drop what the walker cached.
///
/// The flush is not optional: translations cached before the write would keep
/// answering from the old table, and on the boot path that old table is not a
/// table at all.
pub fn activate(root: usize) {
    csr::write_satp(satp_for(root));
    sfence_vma_all();
}

/// Flush every address translation this hart has cached.
pub fn sfence_vma_all() {
    // SAFETY: `sfence.vma` is a fence, not a memory access — it cannot fault and
    // needs no state. It is deliberately *not* `nomem`: the point of writing it
    // here is to order the page-table stores that precede it against the walker
    // that reads them, and `nomem` would invite the compiler to move those stores
    // across it.
    unsafe { asm!("sfence.vma", options(nostack)) };
}

/// Flush the translation of one page, for every ASID.
pub fn sfence_vma_page(va: usize) {
    // SAFETY: as `sfence_vma_all`. The one-operand form is the assembler's
    // shorthand for `sfence.vma va, x0` — "this address, any ASID" — which is
    // what unmap wants: this kernel never leaves a stale global entry behind.
    unsafe { asm!("sfence.vma {}", in(reg) va, options(nostack)) };
}

// The encoding is a contract with the hardware, so the contract is checked here,
// at compile time: a field that moves or a shift that is off by one breaks the
// build instead of the first mapping. Every number below was derived by hand from
// the privileged spec and is stated as a literal on the right.
const _: () = {
    assert!(core::mem::size_of::<PageTable>() == PAGE_SIZE);
    assert!(core::mem::align_of::<PageTable>() == PAGE_SIZE);
    assert!(ENTRIES_PER_TABLE * core::mem::size_of::<u64>() == PAGE_SIZE);

    // 0x0000_003f_c000_0000 = 255 * 2^30: root 0xff, middle 0, leaf 0.
    assert!(vpn(0x3f_c000_0000, 2) == 0xff);
    assert!(vpn(0x3f_c000_0000, 1) == 0);
    assert!(vpn(0x3f_c000_1000, 0) == 1);
    assert!(vpn(0x3f_c020_0000, 1) == 1);

    // A leaf round-trips through the PPN field, and carries the policy bits.
    let leaf = pte_leaf(0x8020_0000, Flags::kernel_rwx());
    assert!(pte_address(leaf) == 0x8020_0000);
    assert!(pte_is_valid(leaf));
    assert!(pte_is_leaf(leaf));
    assert!(leaf & PTE_X != 0 && leaf & PTE_W != 0);

    // A table pointer is valid but not a leaf: that distinction is exactly
    // "descend" versus "this is memory".
    let table = pte_table(0x8020_1000);
    assert!(pte_is_valid(table));
    assert!(!pte_is_leaf(table));
    assert!(pte_address(table) == 0x8020_1000);
    assert!(table & (PTE_R | PTE_W | PTE_X) == 0);

    // satp: mode 8 in bits 63..60, the root's PPN in 43..0, ASID 0.
    assert!(satp_for(0x8020_0000) == 0x8000_0000_0008_0200);

    // Canonicality: the lower half ends at 2^38, the upper half begins at
    // 2^64 - 2^38, and the bit between them is not an address.
    assert!(is_canonical(0));
    assert!(is_canonical(0x3f_ffff_ffff));
    assert!(!is_canonical(0x40_0000_0000));
    assert!(is_canonical(0xffff_ffc0_0000_0000));
};
