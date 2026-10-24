#!/usr/bin/env bash
# Boot pagh-riscv under QEMU's `virt` machine with the generic OpenSBI firmware.
#
#   ./run.sh              interactive serial console (Ctrl-A X to leave QEMU)
#   ./run.sh --headless   no display, serial on stdio (the same thing, minus a
#                         window: `virt` has no VGA console to draw into)
#
# QEMU ships the firmware as `-bios default`; a system copy of
# opensbi-riscv64-generic-fw_dynamic.bin is used when present, and OVMF-style
# external firmware can be forced with BIOS=/path/to/fw.bin.
set -euo pipefail
cd "$(dirname "$0")"

PROFILE=${PROFILE:-debug}
if [[ "$PROFILE" == release ]]; then
  cargo build --release
  KERNEL=target/riscv64gc-unknown-none-elf/release/pagh
else
  cargo build
  KERNEL=target/riscv64gc-unknown-none-elf/debug/pagh
fi

MEMORY=${MEMORY:-256M}
BIOS=${BIOS:-}
BIOS_ARGS=()
if [[ -n "$BIOS" ]]; then
  BIOS_ARGS=(-bios "$BIOS")
fi

exec qemu-system-riscv64 \
  -machine virt \
  "${BIOS_ARGS[@]}" \
  -m "$MEMORY" \
  -kernel "$KERNEL" \
  -nographic \
  -no-reboot
