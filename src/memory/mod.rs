//! Physical memory: the frame allocator, the kernel heap and their self-tests.
//!
//! There is no paging in this milestone. The kernel runs on the mapping OpenSBI
//! leaves behind — bare S-mode, `satp` untouched — so a physical address *is* a
//! pointer, and the whole job of this module is to hand out physical addresses
//! nothing else is using. Sv39 comes next; frames stay 4 KiB and stay physical
//! when it does, so the vmm will sit on top of this rather than replace it.
//!
//! Boot-order contract: [`init`] runs after the device tree has been parsed (it
//! needs `/memory` to know what RAM exists) and before the first allocation.
//! Nothing has to enforce that yet — the pool is empty until `init` runs, so an
//! early allocation fails loudly instead of handing out the kernel's own image.

pub mod heap;
pub mod pmm;
pub mod selftest;

/// The allocator's unit: 4 KiB, the smallest page Sv39 can map and the granule
/// every later subsystem (paging, virtio, ELF loading) is built from.
pub const FRAME_SIZE: usize = 4096;

extern "C" {
    // Linker-defined symbols (`linker.ld` and the stack in `arch/riscv64/boot.rs`).
    // They mark addresses, not objects; only their addresses are taken.
    static __kernel_end: u8;
    static __stack_bottom: u8;
    static __stack_top: u8;
}

/// First address past the kernel image.
///
/// OpenSBI's runtime, `.text`, `.rodata`, `.data` and `.bss` all live below it —
/// including the boot stack and the frame allocator's own bitmap, which is why
/// one range is enough to keep every byte the kernel occupies out of the pool.
pub fn kernel_end() -> usize {
    // `addr_of!` never dereferences the symbol; it is the *address* of a
    // linker-defined marker that is not an object, so no reference is ever formed
    // to it. That is also why this needs no `unsafe`.
    core::ptr::addr_of!(__kernel_end) as usize
}

/// `(bottom, top)` of the boot stack `_start` installs.
pub fn boot_stack() -> (usize, usize) {
    // Both are addresses, never read through — see `kernel_end`.
    (
        core::ptr::addr_of!(__stack_bottom) as usize,
        core::ptr::addr_of!(__stack_top) as usize,
    )
}

/// Page span of the device tree blob at `dtb`.
///
/// Only the header's `totalsize` field is read here: `fdt::Fdt::new` has already
/// validated the blob, and reserving exactly what firmware handed us beats
/// reserving a guessed window ("a device tree is never bigger than 2 MiB") that
/// would quietly waste pool frames on every boot.
pub fn fdt_span(dtb: usize) -> (usize, usize) {
    // SAFETY: `dtb` is the address firmware passed in `a1`; `Fdt::new` read the
    // same header successfully before this runs. The offset of `totalsize` is
    // fixed by the FDT specification (magic at 0, totalsize at 4) and the field is
    // big-endian on every machine.
    let header = unsafe { core::slice::from_raw_parts(dtb as *const u8, 8) };
    let total = u32::from_be_bytes([header[4], header[5], header[6], header[7]]) as usize;
    // Rounded outward: a partial frame at either end is still a frame the
    // allocator must not hand out.
    let start = dtb & !(FRAME_SIZE - 1);
    let end = (dtb + total + FRAME_SIZE - 1) & !(FRAME_SIZE - 1);
    (start, end)
}

/// Build the frame pool: `memory` is the device tree's `/memory` range, and every
/// range in `reserved` is marked unusable.
///
/// See [`pmm::init`] for what each reserved range is for.
pub fn init(memory: (usize, usize), reserved: &[(usize, usize)]) {
    pmm::init(memory, reserved)
}

/// `(free, total)` frames, as reported by the boot line and the `mem` command.
pub fn stats() -> (usize, usize) {
    pmm::stats()
}

/// Frame-aligned `(start, end)` of the range the pool was built from.
pub fn pool_range() -> (usize, usize) {
    pmm::pool_range()
}
