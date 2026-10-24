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

mod arch;
mod console;
mod drivers;
mod fdt;
mod shell;
mod sync;

use core::panic::PanicInfo;

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
    crate::kprintln!(
        "timebase    {} Hz",
        tree.timebase_frequency().unwrap_or(0)
    );
    crate::kprintln!("console     NS16550 @ {:#x}", drivers::uart::BASE);

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
