// pagh OS — a 64-bit RISC-V (RV64) kernel in Rust (#![no_std])
//
// Boot path: QEMU's `virt` machine runs the generic OpenSBI firmware, which
// drops us into S-mode at the ELF entry point with the hart id in `a0` and the
// flattened device tree in `a1`. Nothing else is set up: there is no MMU
// configuration, no allocator and no console until `kmain` builds them, which is
// why the boot sequence below is written as a list of phases that depend on each
// other in one direction only.

#![no_std]
#![no_main]

// `alloc` is not in the 2018+ prelude, and the kernel does not link liballoc's
// sibling `std`: the collection types are usable only because the crate now has a
// global allocator (see `ALLOCATOR` below) and the frame pool behind it.
extern crate alloc;

mod arch;
mod console;
mod drivers;
mod fdt;
mod memory;
mod shell;
mod sync;

use core::panic::PanicInfo;

/// The global allocator: the heap that `alloc::vec::Vec` and friends get their
/// memory from, wired to the frame pool by `memory::heap`.
///
/// It lives here, in the crate root, because a `#[global_allocator]` is a claim
/// about the whole binary — not a property of the heap module. Nothing calls it yet
/// except the `selftest` command, which is the point: the allocator is exercised by
/// a test that can be repeated, not by the boot path.
#[global_allocator]
static ALLOCATOR: memory::heap::Heap = memory::heap::Heap::new();

/// `(bytes in use, bytes mapped from the frame allocator, chunks held)`.
///
/// The shell asks the kernel for its allocator's numbers; the alternative — a
/// second handle to the same state — would let the two disagree.
pub fn heap_stats() -> (usize, usize, usize) {
    ALLOCATOR.stats()
}

/// The kernel entry point called from `_start`.
///
/// `hart_id` is the hart OpenSBI booted us on, `dtb` the physical address of the
/// device tree. Only hart 0 does anything: the others are parked in `_start`
/// before Rust is reached, so this function never runs concurrently.
#[no_mangle]
pub extern "C" fn kmain(hart_id: usize, dtb: usize) -> ! {
    console::init();

    crate::kprintln!();
    crate::kprintln!("========================================");
    crate::kprintln!("   pagh OS — RISC-V (RV64)");
    crate::kprintln!("========================================");
    crate::kprintln!("boot hart   {}", hart_id);
    crate::kprintln!("device tree {:#x}", dtb);

    // The device tree is the machine description: memory size, timebase
    // frequency and where the UART lives all come from it. A kernel that cannot
    // read it is a kernel that guesses, so a parse failure is fatal and loud.
    let tree = match fdt::Fdt::new(dtb) {
        Ok(t) => t,
        Err(e) => {
            crate::kprintln!("fdt: {e}");
            crate::kprintln!("fdt: refusing to guess the machine layout");
            arch::riscv64::park_forever()
        }
    };
    let memory = tree.memory().unwrap_or((0, 0));
    crate::kprintln!(
        "memory      {:#x}..{:#x} ({} MiB)",
        memory.0,
        memory.1,
        (memory.1 - memory.0) / (1024 * 1024)
    );
    crate::kprintln!("timebase    {} Hz", tree.timebase_frequency().unwrap_or(0));
    crate::kprintln!("console     NS16550 @ {:#x}", drivers::uart::BASE);

    // The frame allocator needs the two things the boot path already has: the
    // `/memory` range, and the list of physical ranges the kernel must never hand
    // out. It has to be up before the first allocation — which, now that `alloc`
    // exists, includes any `Vec` a driver or a shell command builds later.
    let reserved = [
        // Everything below `__kernel_end`: OpenSBI's runtime, the kernel image
        // and its `.bss` (the boot stack and the frame bitmap live there too).
        (0, memory::kernel_end()),
        // The boot stack, named explicitly: a linker script that ever moves it
        // out of `.bss` must not silently hand it to the allocator.
        memory::boot_stack(),
        // Firmware's device tree blob, whose size is in its own header.
        memory::fdt_span(dtb),
    ];
    memory::init(memory, &reserved);
    let (free_frames, total_frames) = memory::stats();
    let (heap_used, _, heap_chunks) = heap_stats();
    crate::kprintln!(
        "pmm         {}/{} frames free ({} MiB tracked), heap {} KiB in {} chunk(s)",
        free_frames,
        total_frames,
        total_frames * memory::FRAME_SIZE / (1024 * 1024),
        heap_used / 1024,
        heap_chunks
    );

    // The address space, before anything can trip over it. It is built from the
    // frames above (page tables are PMM frames) and switched on here, so that
    // every phase after this one runs on a map the kernel chose: RAM identity-
    // mapped — RWX below `__kernel_end`, RW above it, which is where W^X holds,
    // because no frame the allocator can hand out is executable — plus the
    // console UART's page, which sits outside RAM and would otherwise fault the
    // first `kprintln!` after the switch (`src/memory/paging.rs`).
    let devices = [(drivers::uart::BASE, 0x100)];
    if let Err(e) = memory::paging::init(memory, memory::kernel_end(), &devices) {
        crate::kprintln!("paging: {e}");
        crate::kprintln!("paging: refusing to run on an address space that is not there");
        arch::riscv64::park_forever()
    }
    memory::report_paging();

    // Traps before anything that can trap: a bad address in the shell must
    // report itself instead of resetting the machine silently.
    arch::riscv64::traps::init();

    shell::run(tree);
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // A panic must leave a trace: `panic = "abort"` means there is no unwinding
    // and no return path, so the message on the serial line is the only evidence
    // of what happened.
    crate::kprintln!();
    crate::kprintln!("[PANIC] {}", info);
    if let Some(loc) = info.location() {
        crate::kprintln!("[PANIC] at {}:{}", loc.file(), loc.line());
    }
    arch::riscv64::park_forever()
}
