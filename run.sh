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
#
# DISK=/path/to/image attaches a virtio-mmio block device (M5). A missing image
# is created blank (64 MiB); the kernel formats it on boot only if it is blank,
# mounts it at /mnt, and from then on the image is a normal ext2 filesystem.
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

# A virtio-mmio disk, when DISK names one. `force-legacy=false` is what makes
# QEMU expose the modern (version 2) transport this kernel speaks; without it the
# transport is legacy and the kernel logs "legacy virtio-mmio version 1".
DISK_ARGS=()
if [[ -n "${DISK:-}" ]]; then
  if [[ ! -f "$DISK" ]]; then
    qemu-img create -f raw "$DISK" 64M >/dev/null
  fi
  DISK_ARGS=(
    -drive "file=$DISK,format=raw,if=none,id=hd0"
    -device virtio-blk-device,drive=hd0
  )
fi

# A virtio-mmio NIC on QEMU's user-mode network: DHCP, ping, DNS and the TCP
# client work out of the box (10.0.2.15/24, gateway 10.0.2.2, DNS 10.0.2.3).
# NO_NET=1 boots a machine without a network card, which is the absent path the
# driver logs (and the smoke test's no-NIC modes rely on).
NET_ARGS=()
if [[ "${NO_NET:-0}" != "1" ]]; then
  NET_ARGS=(
    -netdev user,id=net0
    -device virtio-net-device,netdev=net0
  )
fi

exec qemu-system-riscv64 \
  -machine virt \
  "${BIOS_ARGS[@]}" \
  -m "$MEMORY" \
  -kernel "$KERNEL" \
  -device ramfb \
  -global virtio-mmio.force-legacy=false \
  "${DISK_ARGS[@]}" \
  "${NET_ARGS[@]}" \
  "${DISPLAY_ARGS[@]}" \
  -serial mon:stdio \
  -no-reboot
