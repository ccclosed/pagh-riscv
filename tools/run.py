#!/usr/bin/env python3
"""Boot the RISC-V kernel headless, drive it, and photograph the screen.

The x86 tree's ``tools/qemu_shot.py`` drives a guest through QEMU's monitor:
``screendump`` to capture the framebuffer, ``sendkey`` to type. On RISC-V
``virt`` neither half is the same shape, and this tool exists for the two
differences:

* **the serial port is the keyboard.** There is no PS/2 controller in the
  machine, so the shell reads the same NS16550 the log comes out of; input is
  bytes written to the serial chardev, not ``sendkey`` scancodes. One socket
  therefore carries both directions — connect, read the boot log, type into it;
* **the display is ``ramfb``**, configured by the kernel through fw_cfg, and
  ``screendump`` reads *that* surface. Booting without ``-device ramfb`` still
  works (the kernel says so on the serial line and stays usable), but there is
  nothing to photograph.

Examples
--------

Boot, wait for the shell prompt and capture it::

    python3 tools/run.py --shot /tmp/riscv-shell.png

Type a command first and capture the result::

    python3 tools/run.py --keys "help\\n" --shot /tmp/riscv-help.png

Capture *and* verify that the pixels are the committed font — the check runs
``tools/font_check.py`` against the PNG just taken::

    python3 tools/run.py --keys "help\\n" --shot /tmp/riscv-help.png \\
        --check-font --scan-all

Reuse an instance that is already running with the sockets from the README::

    python3 tools/run.py --attach --keys "mem\\n" --shot /tmp/riscv-mem.png

The serial transcript is always saved (``--serial-log``), because when a
screenshot looks wrong the log is what says whether the guest ever got there.
"""

from __future__ import annotations

import argparse
import os
import pathlib
import socket
import subprocess
import sys
import time

ROOT = pathlib.Path(__file__).resolve().parents[1]

# Socket paths live under /tmp/pagh-drv* by convention: a QEMU instance is a
# machine-wide singleton, and two of them sharing a monitor socket is a bug that
# looks like a flaky test.
SERIAL_DEFAULT = "/tmp/pagh-drv.sock"
MONITOR_DEFAULT = "/tmp/pagh-drv-mon.sock"
SERIAL_LOG_DEFAULT = "/tmp/pagh-drv-serial.log"
SHOT_DEFAULT = "/tmp/pagh-riscv.png"

# Where the kernel is built. `run.sh` builds into the same two paths.
KERNEL_DEBUG = "target/riscv64gc-unknown-none-elf/debug/pagh"
KERNEL_RELEASE = "target/riscv64gc-unknown-none-elf/release/pagh"

# The shell's prompt: it is printed by `shell::run` before the first command and
# it is the last thing a successful boot prints, so it is the marker that says
# "the guest is ready to be typed at".
PROMPT_MARKER = "pagh:rv64>"
# What `--check-font` looks for by default. The greeting is printed on every
# boot, so it is present in a screenshot that typed nothing and cannot be
# confused with what a command produced.
EXPECT_DEFAULT = "Type 'help'"


def die(message: str) -> None:
    """Exit 2 for usage/IO failures, so a caller can tell them from a check."""
    print(f"run: {message}", file=sys.stderr)
    raise SystemExit(2)


def decode_keys(text: str) -> str:
    """Interpret the escapes a shell leaves alone in ``--keys``.

    ``--keys "help\\n"`` arrives as a literal backslash + ``n`` (bash does not
    expand it inside double quotes), so the common ones are decoded here — the
    same service ``qemu_shot.py`` performs on the x86 side. ``\\b`` is the
    shell's backspace (0x7f, what a real terminal sends), kept because the
    destructive-backspace path is the one piece of line editing the screen has
    to reproduce byte for byte.
    """
    return (
        text.replace("\\n", "\r")
        .replace("\\r", "\r")
        .replace("\\t", "\t")
        .replace("\\b", "\x7f")
    )


class Serial:
    """The guest's serial line: output in one direction, keystrokes in the other."""

    def __init__(self, path: str, timeout: float = 15.0):
        self.path = path
        self.transcript = ""
        self.sock = self._connect(path, timeout)

    @staticmethod
    def _connect(path: str, timeout: float) -> socket.socket:
        """Wait for QEMU's serial socket to exist and accept us.

        QEMU creates a ``server=on`` unix socket before the guest starts (and
        with ``wait=on`` does not start the guest until we connect), so a short
        retry loop here is what makes the boot log complete rather than
        whatever was printed after we happened to arrive.
        """
        deadline = time.time() + timeout
        while True:
            sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            try:
                sock.connect(path)
                return sock
            except OSError:
                sock.close()
                if time.time() > deadline:
                    die(f"cannot connect to the serial socket {path!r} (is QEMU running?)")
                time.sleep(0.05)

    def read_until(self, marker: str, timeout: float) -> bool:
        """Read until `marker` shows up; returns whether it did."""
        self.sock.settimeout(0.25)
        deadline = time.time() + timeout
        while time.time() < deadline:
            try:
                chunk = self.sock.recv(65536)
            except socket.timeout:
                continue
            except OSError:
                break
            if not chunk:
                break
            self.transcript += chunk.decode("utf-8", "replace")
            if marker in self.transcript:
                return True
        return marker in self.transcript

    def drain(self, seconds: float) -> None:
        """Collect whatever arrives for `seconds` more (no marker to wait for)."""
        self.sock.settimeout(0.25)
        deadline = time.time() + seconds
        while time.time() < deadline:
            try:
                chunk = self.sock.recv(65536)
            except socket.timeout:
                continue
            except OSError:
                return
            if not chunk:
                return
            self.transcript += chunk.decode("utf-8", "replace")

    def send(self, text: str, delay: float = 0.02) -> None:
        """Type `text` at the guest, one byte at a time.

        The delay is not superstition: the guest polls a 16-byte FIFO, and a
        whole line written at once can outrun it. Twenty milliseconds per byte
        costs nothing at these line lengths.
        """
        for byte in text.encode("utf-8"):
            self.sock.sendall(bytes([byte]))
            time.sleep(delay)

    def close(self) -> None:
        try:
            self.sock.close()
        except OSError:
            pass


class Monitor:
    """Minimal QEMU monitor client (unix socket or ``host:port``)."""

    def __init__(self, target: str, timeout: float = 15.0):
        self.target = target
        self.sock = self._connect(target, timeout)
        self._read_reply(timeout=2.0)  # the banner and the first "(qemu) " prompt

    @staticmethod
    def _connect(target: str, timeout: float) -> socket.socket:
        spec = target[4:] if target.startswith("tcp:") else target
        if ":" in spec and not spec.startswith("/"):
            host, _, port = spec.rpartition(":")
            if not host or not port.isdigit():
                die(f"--monitor wants a socket path or host:port, got {target!r}")
            family, address = socket.AF_INET, (host, int(port))
        else:
            family, address = socket.AF_UNIX, spec
        deadline = time.time() + timeout
        while True:
            sock = socket.socket(family, socket.SOCK_STREAM)
            try:
                sock.connect(address)
                return sock
            except OSError:
                sock.close()
                if time.time() > deadline:
                    die(
                        f"cannot connect to the QEMU monitor {target!r} "
                        "(is QEMU running with -monitor unix:…,server=on?)"
                    )
                time.sleep(0.05)

    def _read_reply(self, timeout: float) -> str:
        """Read until the monitor's prompt; that is what "done" looks like."""
        self.sock.settimeout(0.25)
        out = ""
        deadline = time.time() + timeout
        while time.time() < deadline:
            try:
                chunk = self.sock.recv(65536)
            except socket.timeout:
                continue
            except OSError:
                break
            if not chunk:
                break
            out += chunk.decode("utf-8", "replace")
            if out.rstrip().endswith("(qemu)"):
                break
        return out

    def command(self, cmd: str, timeout: float = 10.0) -> str:
        """Send one monitor command and return its reply."""
        self.sock.sendall(f"{cmd}\n".encode())
        reply = self._read_reply(timeout)
        lowered = reply.lower()
        if "error" in lowered or "unknown command" in lowered or "invalid" in lowered:
            die(f"QEMU rejected {cmd!r}: {reply.strip()}")
        return reply

    def screendump(self, path: pathlib.Path) -> None:
        """Capture the guest framebuffer to `path` (PNG).

        Written to a temporary file first and renamed into place: an existing
        `path` is then only ever replaced by a dump that actually succeeded, so a
        rejected `screendump` cannot destroy the screenshot it was asked to
        overwrite.
        """
        path.parent.mkdir(parents=True, exist_ok=True)
        tmp = path.with_name(f".{path.name}.run-tmp")
        if tmp.exists():
            tmp.unlink()
        self.command(f"screendump {tmp} -f png")
        deadline = time.time() + 20
        while time.time() < deadline:
            if tmp.exists() and tmp.stat().st_size > 0:
                time.sleep(0.2)  # let QEMU finish flushing the file
                tmp.replace(path)
                return
            time.sleep(0.1)
        die(f"screendump produced no file at {path}")

    def quit(self) -> None:
        """Ask QEMU to exit (best effort; the runner also terminates it)."""
        try:
            self.sock.sendall(b"quit\n")
            time.sleep(0.2)
        except OSError:
            pass

    def close(self) -> None:
        try:
            self.sock.close()
        except OSError:
            pass


def boot(args: argparse.Namespace, serial_path: str, monitor_path: str) -> subprocess.Popen:
    """Start QEMU headless, with the serial and monitor sockets this tool uses."""
    kernel = pathlib.Path(args.kernel)
    if not kernel.is_absolute():
        kernel = ROOT / kernel
    if not kernel.is_file():
        die(f"kernel {kernel} not found — run `cargo build` (or pass --kernel)")

    # A stale socket file makes QEMU refuse to start ("Address already in use");
    # removing it is safe because the instance that owned it is gone.
    for path in (serial_path, monitor_path):
        if os.path.exists(path):
            os.unlink(path)

    cmd = [
        "qemu-system-riscv64",
        "-machine", "virt",
        "-bios", "default",
        "-m", args.memory,
        "-device", "ramfb",
        "-kernel", str(kernel),
        # Serial is the console *and* the keyboard (see the module doc), and
        # `wait=on` holds the guest until we are connected, so no boot output is
        # lost between QEMU starting and this tool arriving.
        "-serial", f"unix:{serial_path},server=on,wait=on",
        "-monitor", f"unix:{monitor_path},server=on,wait=off",
        "-display", "none",
        "-no-reboot",
    ]
    if getattr(args, "disk", None):
        disk = pathlib.Path(args.disk)
        if not disk.exists():
            subprocess.run(["qemu-img", "create", "-f", "raw", str(disk), "64M"], check=True)
        cmd += [
            "-drive", f"file={disk},format=raw,if=none,id=hd0",
            "-device", "virtio-blk-device,drive=hd0",
        ]
    if getattr(args, "net", False):
        cmd += [
            "-netdev", "user,id=net0",
            "-device", "virtio-net-device,netdev=net0",
        ]
    if getattr(args, "disk", None) or getattr(args, "net", False):
        cmd += ["-global", "virtio-mmio.force-legacy=false"]
    print("+", " ".join(cmd), file=sys.stderr)
    return subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def stop(proc: subprocess.Popen | None) -> None:
    """Take the guest down; never leave a booted QEMU behind a failed run."""
    if proc is None:
        return
    if proc.poll() is None:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=5)


def check_font(args: argparse.Namespace, shot: pathlib.Path) -> int:
    """Run `tools/font_check.py` against the screenshot just taken.

    The checker is the arbiter of "the console is showing the committed font":
    it rebuilds the expected pixels from `assets/font8x18.bin` with the kernel's
    own blend formula and fails on one differing pixel. This wrapper only
    supplies the paths and the expected text.
    """
    cmd = [
        sys.executable,
        str(ROOT / "tools" / "font_check.py"),
        "--screenshot", str(shot),
        "--find",
        "--expect", args.expect,
    ]
    if args.scan_all:
        cmd.append("--scan-all")
    print("+", " ".join(cmd), file=sys.stderr)
    return subprocess.call(cmd)


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Boot the RISC-V kernel headless, type at it and screenshot it",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=(
            "examples:\n"
            "  python3 tools/run.py --shot /tmp/riscv-shell.png\n"
            '  python3 tools/run.py --keys "help\\n" --shot /tmp/riscv-help.png\n'
            '  python3 tools/run.py --keys "clear\\n" --shot /tmp/riscv-clear.png --check-font\n'
        ),
    )
    parser.add_argument("--shot", default=SHOT_DEFAULT, help=f"screenshot path (default {SHOT_DEFAULT})")
    parser.add_argument("--keys", default="", help="text to type before the screenshot ('\\n' is Enter)")
    parser.add_argument(
        "--wait",
        default=PROMPT_MARKER,
        help=f"serial marker that means the guest is ready (default {PROMPT_MARKER!r})",
    )
    parser.add_argument("--timeout", type=float, default=60.0, help="seconds to wait for the marker")
    parser.add_argument("--settle", type=float, default=0.5, help="seconds between typing and the dump")
    parser.add_argument(
        "--check-font",
        action="store_true",
        help="verify the screenshot against assets/font8x18.bin with tools/font_check.py",
    )
    parser.add_argument(
        "--expect",
        default=EXPECT_DEFAULT,
        help=f"exact text the screenshot should contain (with --check-font; default {EXPECT_DEFAULT!r})",
    )
    parser.add_argument(
        "--scan-all",
        action="store_true",
        help="with --check-font, check every console cell against the asset, not just --expect",
    )
    parser.add_argument("--kernel", default=KERNEL_DEBUG, help=f"kernel ELF (default {KERNEL_DEBUG})")
    parser.add_argument("--release", action="store_true", help=f"use {KERNEL_RELEASE}")
    parser.add_argument("--memory", default="256M", help="guest RAM (default 256M, as run.sh)")
    parser.add_argument(
        "--net",
        action="store_true",
        help="attach a virtio-net NIC on QEMU's user network (DHCP: 10.0.2.15/24, gw 10.0.2.2)",
    )
    parser.add_argument(
        "--disk",
        default="",
        help="attach a virtio-mmio block device at PATH (M5); created blank when missing",
    )
    parser.add_argument("--serial", default=SERIAL_DEFAULT, help=f"serial socket (default {SERIAL_DEFAULT})")
    parser.add_argument(
        "--monitor", default=MONITOR_DEFAULT, help=f"monitor socket (default {MONITOR_DEFAULT})"
    )
    parser.add_argument(
        "--serial-log",
        default=SERIAL_LOG_DEFAULT,
        help=f"where to save the serial transcript (default {SERIAL_LOG_DEFAULT})",
    )
    parser.add_argument(
        "--attach",
        action="store_true",
        help="do not boot QEMU; use the --serial/--monitor sockets of a running instance",
    )
    parser.add_argument("--keep", action="store_true", help="leave a booted instance running afterwards")
    args = parser.parse_args()
    if args.release and args.kernel == KERNEL_DEBUG:
        args.kernel = KERNEL_RELEASE

    serial_path = os.path.abspath(args.serial)
    monitor_path = os.path.abspath(args.monitor)
    shot = pathlib.Path(args.shot)
    if not shot.is_absolute():
        shot = ROOT / shot

    proc: subprocess.Popen | None = None
    serial: Serial | None = None
    monitor: Monitor | None = None
    verdict = 0
    try:
        if not args.attach:
            proc = boot(args, serial_path, monitor_path)
        serial = Serial(serial_path, timeout=args.timeout)
        if args.attach:
            # Attached instances may have printed their prompt *before* we
            # connected — a socket with no client buffers nothing (QEMU drops
            # the bytes) — so wait only briefly, then ask for a fresh prompt: an
            # empty line makes the shell print `pagh:rv64> ` again. A fresh boot
            # is not nudged (it uses `wait=on` and must come up in its own time).
            ready = serial.read_until(args.wait, min(args.timeout, 3.0))
            if not ready:
                print("run: prompt not seen; asking for a fresh one", file=sys.stderr)
                serial.send("\r")
                ready = serial.read_until(args.wait, max(5.0, args.timeout))
        else:
            ready = serial.read_until(args.wait, args.timeout)
        if not ready:
            print(
                f"run: the marker {args.wait!r} did not appear within {args.timeout:.0f}s",
                file=sys.stderr,
            )
            verdict = 1
        else:
            print(f"run: saw {args.wait!r}", file=sys.stderr)

        if args.keys:
            keys = decode_keys(args.keys)
            print(f"run: typing {keys!r}", file=sys.stderr)
            serial.send(keys)
            serial.drain(args.settle)
        elif args.settle:
            serial.drain(args.settle)

        monitor = Monitor(monitor_path, timeout=args.timeout)
        monitor.screendump(shot)
        print(f"run: screenshot {shot} ({shot.stat().st_size} bytes)")

        if args.check_font:
            verdict = max(verdict, check_font(args, shot))
    finally:
        if serial is not None:
            pathlib.Path(args.serial_log).write_text(serial.transcript)
            print(f"run: serial log {args.serial_log}", file=sys.stderr)
            serial.close()
        if monitor is not None:
            monitor.close()
        if proc is not None and not args.keep:
            stop(proc)
        # A unix socket left behind would break the next run; the instance that
        # owned it is either gone or still listening (in which case QEMU unlinks
        # it itself on exit, and removing ours is a no-op).
        if proc is not None and not args.keep:
            for path in (serial_path, monitor_path):
                try:
                    os.unlink(path)
                except OSError:
                    pass
    return verdict


if __name__ == "__main__":
    raise SystemExit(main())