#!/usr/bin/env python3
"""Live evidence for F-r2-rv-1 (0.24.2): a non-canonical user pointer.

`user_range_valid` walks the page tables with `mmu::vpn`, which keeps nine bits
per level, so a value in the Sv39 hole — `0xdead_0000_0000_0000 | path` —
validated against the mapped `path` page while the copy then dereferenced the
full 64-bit value. That is an S-mode load page fault (`SPP = 1`), which
`trap_dispatch` treats as a kernel bug: it prints `[EXC] load page fault` and
parks the hart. One unprivileged `ecall` killed the machine.

This harness boots a real kernel on QEMU `virt` + OpenSBI with a 16 MiB ext2
disk holding `userland/noncanon.elf` and drives the shell over the serial line:

* **fixed tree** — `run /mnt/noncanon.elf` prints the canonical control
  (`C: canonical openat fd=3`), then `-EFAULT` for the aliased pointer
  (`N: non-canonical openat ret=-14`) and the shell prints
  `exited with code 4294967282`. The shell must still answer the next command,
  `selftest` must reach `SELFTEST SUMMARY (user): 16/16 passed` (the ABI fixture
  now carries the reject matrix), and `halt` must power QEMU off with status 0.
  No `[EXC]` and no `[PANIC]` may appear in the transcript.
* **`--baseline KERNEL`** — the same run against a pre-fix kernel. The serial
  log must show `[EXC] load page fault` with `stval=0xdead…`, then
  `[EXC] halting`, and the shell must never come back: that is the vulnerable
  behaviour, recorded for the PR.

Usage::

    cargo build --locked
    python3 tools/test_noncanon.py --transcript docs/live-noncanon.txt
    # Pre-fix reproduction (the vulnerable behaviour, for the record):
    git worktree add /tmp/pagh-rv-stock origin/main
    (cd /tmp/pagh-rv-stock && cargo build --locked)
    python3 tools/test_noncanon.py --baseline /tmp/pagh-rv-stock/target/riscv64gc-unknown-none-elf/debug/pagh

Exit status: 0 only when every assertion held.
"""

from __future__ import annotations

import argparse
import pathlib
import shutil
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools"))

from test_pkg import Failure  # noqa: E402
from test_security import (  # noqa: E402
    KERNEL_DEBUG,
    KERNEL_RELEASE,
    Guest,
    check,
    note,
    require_tool,
    transcript_of,
)

NONCANON_ELF = ROOT / "userland/noncanon.elf"
HELLO_ELF = ROOT / "userland/hello.elf"
DISK_SIZE = "16M"

# The shell prints `exited with code` as the u32 form of the wait status:
# -EFAULT (-14) becomes 4294967282.
EFAULT_LINE = "run: /mnt/noncanon.elf exited with code 4294967282"
FIXED_MARKERS = [
    "C: canonical openat fd=3",
    "F-r2-rv-1: openat(non-canonical path)",
    "N: non-canonical openat ret=-14",
    EFAULT_LINE,
    "f-r2-rv-1 survived",
    "abi: canonical pointers ok",
    "SELFTEST user-abi-syscalls PASS",
    "SELFTEST SUMMARY (user): 16/16 passed",
    "SELFTEST SUMMARY (net): 12/12 passed",
]
BASELINE_MARKERS = [
    "F-r2-rv-1: openat(non-canonical path)",
    "[EXC] load page fault",
    "stval=0xdead",
    "[EXC] halting",
]


def die(message: str, code: int = 2) -> None:
    print(f"test_noncanon: {message}", file=sys.stderr)
    raise SystemExit(code)


def build_disk(work: pathlib.Path) -> pathlib.Path:
    """A 16 MiB ext2 image with the F-r2-rv-1 fixture and the hello control."""
    disk = work / "noncanon.img"
    subprocess.run(
        ["qemu-img", "create", "-f", "raw", str(disk), DISK_SIZE],
        check=True,
        stdout=subprocess.DEVNULL,
    )
    subprocess.run(
        [
            "mkfs.ext2",
            "-q",
            "-t",
            "ext2",
            "-b",
            "1024",
            "-I",
            "128",
            "-O",
            "^resize_inode,^dir_index,^ext_attr",
            "-F",
            str(disk),
        ],
        check=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    for guest_path, host_path in (
        ("/hello.elf", HELLO_ELF),
        ("/noncanon.elf", NONCANON_ELF),
    ):
        subprocess.run(
            ["debugfs", "-w", "-R", f"write {host_path} {guest_path}", str(disk)],
            check=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
    note(f"disk: {disk} (16 MiB ext2, /noncanon.elf + /hello.elf)")
    return disk


def run_fixed(guest: Guest) -> None:
    ok, segment = guest.command("run /mnt/noncanon.elf", expect=EFAULT_LINE)
    check(ok, "non-canonical openat answered -EFAULT", segment)
    for marker in FIXED_MARKERS[:4]:
        check(marker in segment, f"the fixture printed {marker!r}", segment)

    # The machine must still be there for the next command: that is the whole
    # point of the fix (the vulnerable build never prints a prompt again).
    ok, segment = guest.command("echo f-r2-rv-1 survived", expect="f-r2-rv-1 survived")
    check(ok, "the shell survived the non-canonical pointer", segment)

    # The in-guest reject matrix: the ABI fixture ORs the same prefixes over
    # real user addresses across every copy-helper shape and requires -EFAULT.
    ok, segment = guest.command(
        "selftest", expect="SELFTEST SUMMARY (net): 12/12 passed", timeout=300.0
    )
    check(ok, "selftest reached its last summary", segment)
    for marker in FIXED_MARKERS[5:]:
        check(marker in segment, f"selftest printed {marker!r}", segment)

    exit_code = guest.halt()
    check(exit_code == 0, f"QEMU exited {exit_code} after halt")
    transcript = transcript_of(guest)
    check("[EXC]" not in transcript, "no unhandled exception in the transcript")
    check("[PANIC]" not in transcript, "no panic in the transcript")


def run_baseline(guest: Guest) -> None:
    """The pre-fix behaviour, for the record: the guest halts and never returns."""
    assert guest.serial is not None
    guest.serial.send("run /mnt/noncanon.elf\r")
    if not guest.serial.wait_for("[EXC] halting", 60):
        raise Failure("baseline did not halt on the non-canonical pointer")
    guest.serial.pump(1.0)
    transcript = transcript_of(guest)
    for marker in BASELINE_MARKERS:
        check(marker in transcript, f"baseline transcript shows {marker!r}")
    check(
        "halt: powering off" not in transcript,
        "the baseline guest never reached a clean halt",
    )


def write_transcript(
    path: pathlib.Path,
    guest: Guest,
    kernel: pathlib.Path,
    baseline: bool,
) -> None:
    text = transcript_of(guest)
    header = [
        "# Live security acceptance for pagh-riscv F-r2-rv-1 (0.24.2)",
        f"# kernel: {kernel}",
        f"# mode: {'pre-fix baseline (origin/main a3f3a70)' if baseline else 'fixed tree'}",
        "# disk: 16 MiB ext2 — /noncanon.elf (one canonical control openat, then",
        "#       openat(0xdead_0000_0000_0000 | path)), /hello.elf",
        "# harness: tools/test_noncanon.py (QEMU virt + OpenSBI, serial shell)",
        "#",
        "# Checks: -EFAULT for the non-canonical pointer with the shell alive,",
        "# selftest user 16/16 + net 12/12 (the ABI fixture carries the reject",
        "# matrix), clean halt; baseline halts with [EXC] load page fault.",
        "",
    ]
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("\n".join(header) + text)
    note(f"transcript: {path}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    kernel_group = parser.add_mutually_exclusive_group()
    kernel_group.add_argument("--release", action="store_true", help="use the release kernel")
    kernel_group.add_argument("--kernel", type=pathlib.Path, help="explicit kernel image")
    parser.add_argument(
        "--baseline",
        type=pathlib.Path,
        metavar="KERNEL",
        help="reproduce the pre-fix behaviour with this (origin/main) kernel",
    )
    parser.add_argument(
        "--transcript",
        type=pathlib.Path,
        help="write the serial transcript here (e.g. docs/live-noncanon.txt)",
    )
    parser.add_argument("--keep", action="store_true", help="keep the temporary work dir")
    args = parser.parse_args()

    require_tool("qemu-system-riscv64")
    require_tool("qemu-img")
    require_tool("mkfs.ext2")
    require_tool("debugfs")
    if not NONCANON_ELF.is_file():
        die(f"{NONCANON_ELF} is missing (run userland/build.sh)")

    if args.baseline is not None:
        kernel = args.baseline.resolve()
        baseline = True
    elif args.kernel is not None:
        kernel = args.kernel.resolve()
        baseline = False
    else:
        kernel = (KERNEL_RELEASE if args.release else KERNEL_DEBUG).resolve()
        baseline = False
    if not kernel.is_file():
        die(f"kernel not built: {kernel} (run `cargo build --locked`)")

    work = pathlib.Path(tempfile.mkdtemp(prefix="pagh-noncanon-"))
    disk = build_disk(work)
    guest = Guest(kernel, disk, work)
    try:
        guest.boot()
        if baseline:
            run_baseline(guest)
            if args.transcript:
                write_transcript(args.transcript, guest, kernel, baseline=True)
        else:
            run_fixed(guest)
            if args.transcript:
                write_transcript(args.transcript, guest, kernel, baseline=False)
        note("PASS")
        return 0
    except Failure as failure:
        note(f"FAIL: {failure}")
        if guest.serial is not None:
            tail = transcript_of(guest)[-4000:]
            print(f"--- serial tail ---\n{tail}\n--- end ---", file=sys.stderr)
        return 1
    finally:
        guest.shutdown()
        if args.keep:
            note(f"work dir kept: {work}")
        else:
            shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())