# pagh OS — RISC-V (RV64)

The x86-64 kernel in [ccclosed/Pagh-OS](https://github.com/ccclosed/Pagh-OS) ported
to 64-bit RISC-V: same idea (a small `#![no_std]` kernel that boots straight into a
usable console), a machine that says what it is through a device tree instead of
through firmware tables.

```sh
rustup target add riscv64gc-unknown-none-elf   # pinned nightly, see rust-toolchain.toml
cargo build
./run.sh                                       # QEMU virt + OpenSBI, serial console
```

## What works today

* **Boots under OpenSBI** on QEMU's `virt` machine: `_start` sets a stack, parks the
  secondary harts, and calls `kmain` with the hart id and the device tree.
* **Reads the machine description** from the flattened device tree — memory range,
  timebase frequency — and refuses to guess when it cannot (`src/fdt.rs`).
* **Traps have an entry point**: `stvec` → `trap_entry` → a Rust handler that names
  the cause (`illegal instruction`, page faults, interrupts) instead of resetting
  silently (`src/arch/riscv64/traps.rs`).
* **An interactive console** over the NS16550 UART: line editing, `help`, `mem`,
  `time`, `hart`, `echo`, `clear`, `halt` (`src/shell.rs`).

## Roadmap

| Milestone | Contents |
|---|---|
| M1 (done) | Boot under OpenSBI, DTB parsing, trap entry, UART console + shell |
| M2 | Physical frame allocator, Sv39 paging, kernel heap, `alloc` |
| M3 | CLINT timer via SBI, PLIC, interrupt-driven console |
| M4 | `ramfb` framebuffer + the Fira Code coverage font, screenshot pipeline |
| M5 | virtio-mmio block device, ext2, VFS |
| M6 | Tasks, syscalls, and the Linux-compat userland work from the x86 branch |

## Layout

```text
src/main.rs                 boot sequence (phases, in order)
src/arch/riscv64/           entry asm, CSRs, trap frame + handler
src/drivers/uart.rs         NS16550 console
src/fdt.rs                  flattened device tree reader
src/shell.rs                the interactive console
```

## Honest limitations

Everything not listed under "What works today" is not implemented: there is no
allocator, no paging (the kernel runs on the firmware's identity-ish mapping), no
timer, no block device and no scheduler. `memory/` is an empty placeholder waiting
for M2.
