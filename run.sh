#!/usr/bin/env bash
# Boot pagh-riscv under QEMU's `virt` machine with the generic OpenSBI firmware.
#
#   ./run.sh              serial console in this terminal; the screen is rendered
#                         off-screen by QEMU (ramfb), so `screendump` and the
#                         screenshot tools work even here
#   ./run.sh --window     the same machine with a QEMU window showing pagh's
#                         framebuffer (needs a display; Ctrl-A X quits)
#   ./run.sh --headless   explicit form of the default: no window
#
# The serial line is the keyboard: `virt` has no PS/2 controller, so typing here
# is typing into the guest's shell.
#
# QEMU ships the firmware as `-bios default`; a system copy of
# opensbi-riscv64-generic-fw_dynamic.bin is used when present, and an external
# firmware can be forced with BIOS=/path/to/fw.bin.
set -euo pipefail
cd "$(dirname "$0")"

WINDOW=0
for arg in "$@"; do
  case "$arg" in
    --window) WINDOW=1 ;;
    --headless) WINDOW=0 ;;
    *) echo "run.sh: unknown argument '$arg' (want --window or --headless)" >&2; exit 2 ;;
  esac
done

PROFILE=${PROFILE:-debug}
if [[ "$PROFILE" == release ]]; then
  cargo build --release
  KERNEL=target/riscv64gc-unknown-none-elf/release/pagh
else
  cargo build
  KERNEL=target/riscv64gc-unknown-none-elf/debug/pagh
fi

MEMORY=${MEMORY:-256M}
# BIOS: a firmware image. Empty means QEMU's own `default` (its built-in OpenSBI
# search path); PAGH_BIOS is the fallback the CI workflow sets when a runner image
# keeps the file somewhere QEMU does not look.
BIOS=${BIOS:-${PAGH_BIOS:-}}
BIOS_ARGS=()
if [[ -n "$BIOS" ]]; then
  BIOS_ARGS=(-bios "$BIOS")
fi

# `-device ramfb` is what gives the machine a framebuffer at all: the generic
# `virt` machine has no VGA, and the kernel finds the device through fw_cfg. With
# `--window` QEMU also displays it; headless it still renders it, which is what
# lets `tools/run.py --shot` capture the screen without a display server.
DISPLAY_ARGS=(-display none)
if ((WINDOW)); then
  DISPLAY_ARGS=(-display gtk)
fi

exec qemu-system-riscv64 \
  -machine virt \
  "${BIOS_ARGS[@]}" \
  -m "$MEMORY" \
  -kernel "$KERNEL" \
  -device ramfb \
  "${DISPLAY_ARGS[@]}" \
  -serial mon:stdio \
  -no-reboot
