//! Physical memory: the frame allocator, the Sv39 map, the kernel heap and their
//! self-tests.
//!
//! Three layers, in the order boot builds them, because each is the next one's
//! precondition:
//!
//! 1. [`pmm`] hands out 4 KiB physical frames, built from the device tree's
//!    `/memory` range with the kernel image, the boot stack and the device tree
//!    reserved.
//! 2. [`paging`] builds the kernel's Sv39 identity map out of those frames and
//!    switches `satp` to it. It runs second because it *spends* frames, and it
//!    must run before anything reaches memory through a frame address: the heap
//!    hands out physical addresses as pointers, which is only sound because RAM
//!    is mapped at its physical address.
//! 3. [`heap`] is the `GlobalAlloc` behind `alloc::vec::Vec`/`String`, growing in
//!    chunks taken from the PMM when a request does not fit.
//!
//! Boot-order contract: [`init`] runs after the device tree has been parsed (it
//! needs `/memory` to know what RAM exists) and before the first allocation. The
//! pool is empty until `init` runs, so an early allocation fails loudly instead
//! of handing out the kernel's own image.

pub mod heap;
pub mod paging;
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

/// One boot line describing the address space, printed right after
/// [`paging::init`].
///
/// The shell's `mem` command prints its own version of the same numbers, shaped
/// to that command's column: the facts come from [`paging::snapshot`], the layout
/// belongs to the caller. The boot line exists so the serial log — the record the
/// E2E gates grep — states that the CPU is translating through a map this kernel
/// built, and which one.
pub fn report_paging() {
    match paging::snapshot() {
        Some(state) => crate::kprintln!(
            "sv39        {}, satp={:#x}, identity {:#x}..{:#x} (RWX below {:#x}, RW above), {} pages, {} device",
            if paging::active() { "on" } else { "off" },
            state.satp,
            state.identity.0,
            state.identity.1,
            state.executable_below,
            state.mappings,
            state.device_pages
        ),
        None => crate::kprintln!("sv39        no address space (paging has not been built)"),
    }
}
