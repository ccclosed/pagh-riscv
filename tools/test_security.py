#!/usr/bin/env python3
"""Live evidence for the 0.24.1 security fixes (F-rv-1, F-rv-2, F-rv-3).

This is the harness behind `docs/live-security.txt`. It boots a real kernel on
QEMU `virt` + OpenSBI with a crafted ext2 image and drives the shell over the
serial line, so every claim the fixes make is checked in the guest instead of
in a comment:

1. **F-rv-1 — crafted ext2 symlink.** A slow symlink inode is patched on disk to
   `i_size = 0xFFFFFF00`. `cat /mnt/biglink` must answer
   `cat: /mnt/biglink: filesystem is corrupt` and the shell must keep running;
   before the fix the same command sized a 4 GiB allocation, the alloc-error
   handler panicked and the hart parked (`panic = "abort"`).
2. **F-rv-2 — overlapping `PT_LOAD` permissions.** Two crafted ELFs are built
   from the checked-in `userland/hello.elf`: one header asks for `PF_R|PF_W|PF_X`,
   and one extra writable `PT_LOAD` covers the text page. Both must be refused
   with the loader's W^X message, and the unmodified `hello.elf` must still run
   (the fix does not break normal images). On origin/main `aa8f1a9` the same
   files ran normally — `--baseline KERNEL` reproduces that and records it.
3. **F-rv-3 — `mprotect` and the active root.** The shell's `selftest` runs the
   `sv39-map-roundtrip` check, which now proves `paging::protect_in(root, …)`
   rewrites the named root and not the active one, and the `user-abi-syscalls`
   check, which drives `mprotect` through the syscall path the fix changed.
   `SELFTEST SUMMARY (user): 16/16 passed` with `/mnt` mounted is the gate.

Requirements: `qemu-system-riscv64` with the OpenSBI image QEMU ships,
`mkfs.ext2` and `debugfs` from e2fsprogs, `qemu-img`, and a built kernel
(`cargo build --locked`, or `--release`). The script is a manual acceptance run
like `tools/test_pkg.py` / `tools/test_sockets.py`: CI has no e2fsprogs.

Usage::

    cargo build --locked
    python3 tools/test_security.py --transcript docs/live-security.txt
    python3 tools/test_security.py --release
    # Pre-fix reproduction (the vulnerable behaviour, for the record):
    git worktree add /tmp/pagh-rv-baseline origin/main
    (cd /tmp/pagh-rv-baseline && cargo build --locked)
    python3 tools/test_security.py --baseline /tmp/pagh-rv-baseline/target/.../debug/pagh

Exit status: 0 only when every assertion held (and the guest powered off).
"""

from __future__ import annotations

import argparse
import os
import pathlib
import shutil
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools"))

from test_pkg import BANNER, PROMPT, Failure, Serial  # noqa: E402
import craft_symlink  # noqa: E402
import craft_wx_elf  # noqa: E402

KERNEL_DEBUG = ROOT / "target/riscv64gc-unknown-none-elf/debug/pagh"
KERNEL_RELEASE = ROOT / "target/riscv64gc-unknown-none-elf/release/pagh"
HELLO_ELF = ROOT / "userland/hello.elf"
DISK_SIZE = "16M"

# The W^X diagnostic the loader prints for both crafted files. Kept as one
# string so the transcript line and this assertion cannot drift apart.
WX_MESSAGE = "PT_LOAD permissions would create a write-execute page (W^X)"
CORRUPT_MESSAGE = "cat: /mnt/biglink: filesystem is corrupt"

# The selftest gates for the three fixes.
SELFTEST_MARKERS = [
    "SELFTEST sv39-map-roundtrip PASS",
    "SELFTEST SUMMARY: 7/7 passed",
    "SELFTEST ext2-corruption PASS",
    "SELFTEST ext2 summary: 11/11 passed",
    "SELFTEST user-elf-reject PASS",
    "SELFTEST user-abi-syscalls PASS",
    "SELFTEST SUMMARY (user): 16/16 passed",
    "SELFTEST SUMMARY (net): 12/12 passed",
]


def die(message: str, code: int = 2) -> None:
    print(f"test_security: {message}", file=sys.stderr)
    raise SystemExit(code)


def note(message: str) -> None:
    print(f"test_security: {message}")


def require_tool(name: str) -> str:
    found = shutil.which(name)
    if found is None:
        die(f"{name} is required (Debian/Ubuntu: e2fsprogs/qemu-utils)")
    return found


def find_bios() -> str:
    """The OpenSBI image QEMU boots, from the environment or a known path."""
    configured = (os.environ.get("PAGH_BIOS") or "").strip()
    if configured:
        return configured
    for candidate in (
        "/usr/share/qemu/opensbi-riscv64-generic-fw_dynamic.bin",
        "/usr/lib/riscv64-linux-gnu/opensbi/generic/fw_dynamic.bin",
    ):
        if os.path.isfile(candidate):
            return candidate
    return "default"


# ── crafted disk ───────────────────────────────────────────────────────────


def build_disk(work: pathlib.Path) -> pathlib.Path:
    """A 16 MiB ext2 image with the hello fixture, two W^X ELFs and the link."""
    disk = work / "security.img"
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

    crafted: dict[str, pathlib.Path] = {}
    for mode in ("overlap", "segment"):
        path = work / f"wx-{mode}.elf"
        with HELLO_ELF.open("rb") as handle:
            data = bytearray(handle.read())
        path.write_bytes(craft_wx_elf.craft(data, mode))
        crafted[mode] = path

    def debugfs_write(guest_path: str, host_path: pathlib.Path) -> None:
        subprocess.run(
            ["debugfs", "-w", "-R", f"write {host_path} {guest_path}", str(disk)],
            check=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )

    debugfs_write("/hello.elf", HELLO_ELF)
    debugfs_write("/wx-overlap.elf", crafted["overlap"])
    debugfs_write("/wx-segment.elf", crafted["segment"])

    # The symlink craft re-runs debugfs on a copy and patches the inode; do it
    # last so no later debugfs write touches the damaged inode table.
    crafted_image = work / "security-crafted.img"
    facts = craft_symlink.craft(str(disk), str(crafted_image))
    note(
        "crafted symlink: inode %(inode)d, i_size %(before_size)d -> 0xFFFFFF00"
        % facts
    )
    note(f"disk: {crafted_image} (16 MiB ext2, 1024 B blocks)")
    return crafted_image


# ── QEMU/serial driver ─────────────────────────────────────────────────────


class Guest:
    """One booted kernel and its shell, over the serial unix socket."""

    def __init__(self, kernel: pathlib.Path, disk: pathlib.Path, work: pathlib.Path):
        self.kernel = kernel
        self.disk = disk
        self.work = work
        self.socket_path = work / "serial.sock"
        self.process: subprocess.Popen | None = None
        self.serial: Serial | None = None

    def boot(self, timeout: float = 120.0) -> None:
        qemu = os.environ.get("QEMU", "qemu-system-riscv64")
        qemu = shutil.which(qemu) if not os.path.isabs(qemu) else qemu
        if qemu is None:
            die("qemu-system-riscv64 is not on PATH")
        command = [
            qemu,
            "-machine",
            "virt",
            "-m",
            "256M",
            "-bios",
            find_bios(),
            "-kernel",
            str(self.kernel),
            "-display",
            "none",
            "-no-reboot",
            "-serial",
            f"unix:{self.socket_path},server=on,wait=on",
            "-monitor",
            "none",
            "-global",
            "virtio-mmio.force-legacy=false",
            "-device",
            "virtio-tablet-device",
            "-device",
            "ramfb",
            "-drive",
            f"file={self.disk},format=raw,if=none,id=hd0",
            "-device",
            "virtio-blk-device,drive=hd0",
        ]
        note("+ " + " ".join(command))
        self.process = subprocess.Popen(
            command, stdout=subprocess.DEVNULL, stderr=subprocess.STDOUT
        )
        self.serial = Serial(self.socket_path, self.process)
        if not self.serial.wait_for(BANNER, timeout):
            raise Failure(f"boot banner missing within {timeout:.0f}s")
        if not self.serial.wait_for(PROMPT, timeout):
            raise Failure("shell prompt missing")

    def command(
        self, line: str, expect: str | None = None, timeout: float = 90.0
    ) -> tuple[bool, str]:
        assert self.serial is not None
        start = self.serial.mark
        self.serial.send(line + "\r")
        found = True if expect is None else self.serial.wait_for(expect, timeout)
        prompted = self.serial.wait_for(PROMPT, timeout)
        return found and prompted, self.serial.segment(start)

    def halt(self) -> int:
        assert self.serial is not None and self.process is not None
        self.serial.send("halt\r")
        self.serial.wait_for("halt: powering off", 20)
        try:
            return self.process.wait(timeout=20)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait(timeout=5)
            return -1

    def shutdown(self) -> None:
        if self.serial is not None:
            self.serial.close()
            self.serial = None
        if self.process is not None and self.process.poll() is None:
            self.process.kill()
            self.process.wait(timeout=5)


def transcript_of(guest: Guest) -> str:
    assert guest.serial is not None
    return guest.serial.text


# ── the fixed-kernel scenario ──────────────────────────────────────────────


def run_fixed(guest: Guest) -> None:
    # F-rv-1: a path walk that follows the crafted link is a clean error, and
    # the machine is still there for the next command.
    ok, segment = guest.command("ls /mnt", expect="biglink")
    check(ok, "ls /mnt", segment)
    check("hello.elf" in segment and "wx-overlap.elf" in segment, "ls /mnt listing", segment)
    ok, segment = guest.command("cat /mnt/biglink", expect=CORRUPT_MESSAGE)
    check(ok, f"cat /mnt/biglink answered {CORRUPT_MESSAGE!r}", segment)
    ok, segment = guest.command("echo f-rv-1 survived", expect="f-rv-1 survived")
    check(ok, "the shell survived the crafted symlink", segment)

    # F-rv-2: both crafted ELFs are refused by name; the untouched fixture still
    # loads and runs. A fix that rejected every image would fail the last case.
    for name in ("wx-overlap.elf", "wx-segment.elf"):
        ok, segment = guest.command(f"run /mnt/{name}", expect=WX_MESSAGE)
        check(ok, f"{name} refused with the W^X diagnostic", segment)
    ok, segment = guest.command(
        "run /mnt/hello.elf", expect="run: /mnt/hello.elf exited with code 42"
    )
    check(ok, "the unmodified hello fixture still runs", segment)
    check("hello from user mode" in segment, "hello wrote its greeting", segment)

    # F-rv-3 and the regressions above, in-QEMU: the memory root-target check,
    # the crafted-symlink corruption check and the W^X reject matrix.
    ok, segment = guest.command("selftest", expect=SELFTEST_MARKERS[-1], timeout=300.0)
    check(ok, "selftest reached its last summary", segment)
    for marker in SELFTEST_MARKERS:
        check(marker in segment, f"selftest printed {marker!r}", segment)


def run_baseline(guest: Guest) -> None:
    """The pre-fix behaviour, for the record: crafted images are accepted."""
    for name in ("wx-overlap.elf", "wx-segment.elf", "hello.elf"):
        ok, segment = guest.command(
            f"run /mnt/{name}",
            expect=f"run: /mnt/{name} exited with code 42",
        )
        check(ok, f"baseline accepted {name} (the vulnerable behaviour)", segment)
        check("hello from user mode" in segment, f"baseline ran {name}", segment)
    guest.serial.send("cat /mnt/biglink\r")
    guest.serial.wait_for("memory allocation of", 30)
    guest.serial.pump(1.0)
    check(
        "[PANIC]" in transcript_of(guest),
        "baseline panicked on the crafted symlink (the vulnerability)",
        "",
    )


# ── assertions and transcript ─────────────────────────────────────────────


def check(condition: bool, what: str, segment: str = "") -> None:
    if condition:
        note(f"ok: {what}")
        return
    if segment:
        print(f"--- command transcript ---\n{segment}\n--- end ---", file=sys.stderr)
    raise Failure(what)


def write_transcript(
    path: pathlib.Path,
    guest: Guest,
    kernel: pathlib.Path,
    baseline: bool,
) -> None:
    text = transcript_of(guest)
    header = [
        "# Live security acceptance for pagh-riscv (0.24.1)",
        f"# kernel: {kernel}",
        f"# mode: {'pre-fix baseline (origin/main aa8f1a9)' if baseline else 'fixed tree'}",
        f"# disk: crafted 16 MiB ext2 — /biglink (i_size=0xFFFFFF00), "
        f"/wx-overlap.elf, /wx-segment.elf, /hello.elf",
        "# harness: tools/test_security.py (QEMU virt + OpenSBI, serial shell)",
        "#",
        "# Checks: F-rv-1 clean Corrupt + live prompt; F-rv-2 W^X refusal with a",
        "# normal image still loading; F-rv-3 selftest 7/7 + ext2 11/11 + user 16/16.",
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
        help="write the serial transcript here (e.g. docs/live-security.txt)",
    )
    parser.add_argument("--keep", action="store_true", help="keep the temporary work dir")
    args = parser.parse_args()

    require_tool("qemu-system-riscv64")
    require_tool("qemu-img")
    require_tool("mkfs.ext2")
    require_tool("debugfs")

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

    work = pathlib.Path(tempfile.mkdtemp(prefix="pagh-security-"))
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
            exit_code = guest.halt()
            check(exit_code == 0, f"QEMU exited {exit_code} after halt")
            transcript = transcript_of(guest)
            check("[PANIC]" not in transcript, "no panic on the crafted inputs")
            check("[EXC]" not in transcript, "no unhandled exception on the crafted inputs")
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