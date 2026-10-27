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
mod fs;
mod memory;
mod shell;
mod sync;
mod vfs;

use alloc::sync::Arc;
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

    // The virtio-mmio transports this machine declares. The block driver probes
    // windows after paging is on, so each one has to be part of the address space
    // first; enumerating once here keeps the number and the addresses a
    // device-tree fact instead of a constant (QEMU `virt` reports eight), and it
    // is the same list the driver will probe.
    let mut virtio_mmio = [fdt::VirtioMmio::NONE; fdt::MAX_VIRTIO_MMIO];
    let virtio_mmio_count = tree.virtio_mmio(&mut virtio_mmio);
    // Every MMIO window the kernel touches after the switch: the console UART,
    // which prints through it, and each transport window the tree just named. A
    // window the map does not cover is not an unmapped device — it is a trap on
    // the first register read — so the list comes from the device tree rather
    // than from `0x1000_1000`; a machine without virtio adds nothing.
    let mut device_windows = [(0usize, 0usize); 1 + fdt::MAX_VIRTIO_MMIO];
    device_windows[0] = (drivers::uart::BASE, 0x100);
    for (entry, window) in device_windows[1..]
        .iter_mut()
        .zip(&virtio_mmio[..virtio_mmio_count])
    {
        *entry = (window.base, window.length);
    }
    let devices = &device_windows[..1 + virtio_mmio_count];

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
    // console UART's page and the virtio-mmio windows, which sit outside RAM and
    // would otherwise fault the first `kprintln!`/register read after the switch
    // (`src/memory/paging.rs`).
    if let Err(e) = memory::paging::init(memory, memory::kernel_end(), devices) {
        crate::kprintln!("paging: {e}");
        crate::kprintln!("paging: refusing to run on an address space that is not there");
        arch::riscv64::park_forever()
    }
    memory::report_paging();

    // Traps before anything that can trap: a bad address in the shell must
    // report itself instead of resetting the machine silently.
    arch::riscv64::traps::init();

    // The block driver first: it probes every transport window, so it needs the
    // map (a read through an unmapped window would fault) and the trap handler
    // above (a broken window must report itself, not reset the machine). Absence
    // is logged, not fatal: a headless CI boot has no disk attached, and the
    // kernel is still a kernel without one.
    drivers::virtio_mmio::init(&tree);

    // The VFS root (a synthetic, kernel-owned directory) and then the mounted
    // filesystem. Phase order matters: the driver above has registered its
    // device, the heap is up (mounting allocates), and the shell's file commands
    // assume `/mnt` is either mounted or cleanly reported as absent. With no
    // block device this logs one line and continues — CI boots without a disk.
    vfs::init();
    init_fs();

    shell::run(tree);
}

/// Phase M5: format a blank block device (only a blank one) and mount it at
/// `/mnt`.
///
/// The decision sequence is the safety property, not an implementation detail:
///
/// 1. no block device → one clear log line, shell keeps working;
/// 2. a mountable ext2 image → mount it, never write anything first;
/// 3. a failed mount → ask [`fs::format_policy`] what the device looks like. Only
///    a **blank** device is formatted (a partition table, foreign data or a
///    damaged ext2 superblock is refused by name), and the log says so;
/// 4. format then mount again; a failure at either step leaves `/mnt` unmounted
///    and the machine running.
fn init_fs() {
    use crate::fs::ext2::Ext2Fs;
    use crate::fs::format_policy;

    let Some(dev) = drivers::block::get() else {
        crate::kprintln!("fs          no block device (/mnt not mounted)");
        return;
    };
    let sectors = dev.sector_count();
    crate::kprintln!(
        "fs          {} ({} sectors, {} KiB)",
        dev.name(),
        sectors,
        sectors.saturating_mul(512) / 1024
    );

    let root = match Ext2Fs::mount(Arc::clone(&dev)) {
        Ok(root) => root,
        Err(first_error) => {
            let has_superblock = Ext2Fs::has_valid_superblock(&*dev);
            let layout = format_policy::probe_device(&*dev);
            match format_policy::format_allowed(layout, has_superblock) {
                Ok(()) => {
                    crate::kprintln!("fs          {} is blank — formatting ext2", dev.name());
                    if let Err(e) = Ext2Fs::format(Arc::clone(&dev)) {
                        crate::kprintln!(
                            "fs          format failed: {}; /mnt not mounted",
                            e.as_str()
                        );
                        return;
                    }
                    match Ext2Fs::mount(Arc::clone(&dev)) {
                        Ok(root) => root,
                        Err(e) => {
                            crate::kprintln!(
                                "fs          mount after format failed: {}; /mnt not mounted",
                                e.as_str()
                            );
                            return;
                        }
                    }
                }
                Err(refusal) => {
                    crate::kprintln!(
                        "fs          mount failed ({}); refusing to format {}: {}",
                        first_error.as_str(),
                        dev.name(),
                        refusal.reason()
                    );
                    crate::kprintln!(
                        "fs          {} left untouched (/mnt not mounted)",
                        dev.name()
                    );
                    return;
                }
            }
        }
    };

    match vfs::mount_at("/mnt", root) {
        Ok(()) => match vfs::lookup_path("/mnt") {
            Ok(node) => match node.fs_stat() {
                Some(stat) => crate::kprintln!(
                    "fs          ext2 on /mnt, {} B blocks, {}/{} blocks free, {}/{} inodes free",
                    stat.block_size,
                    stat.blocks_free,
                    stat.blocks_total,
                    stat.inodes_free,
                    stat.inodes_total
                ),
                None => crate::kprintln!("fs          mounted at /mnt"),
            },
            Err(_) => crate::kprintln!("fs          mounted at /mnt"),
        },
        Err(e) => crate::kprintln!("fs          mount_at(/mnt) failed: {}", e.as_str()),
    }
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
