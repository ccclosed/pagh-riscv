#!/usr/bin/env python3
"""End-to-end test for the kernel's native `pkg` installer.

This is the M9 acceptance harness for `src/pkg/`: it builds a real `.ppkg`
around the checked-in `userland/hello.elf`, publishes it through a host HTTP
server (QEMU's user network exposes the host as 10.0.2.2), boots the kernel with
a fresh disk, and drives the shell over a Unix serial socket.

What is proven, in order:

1. `pkg index` fetches the flat index over plain HTTP and parses one entry;
2. `pkg install hello` downloads, verifies size + SHA-256, unpacks the payload
   and writes the manifest;
3. `ls /mnt/bin` shows the installed program and `run` executes it, printing
   `hello from user mode` and exiting 42 through the real user-mode path;
4. a copy of the package with one byte flipped is refused with a SHA-256 error
   and leaves nothing under `/mnt/bin` (the index itself is unchanged);
5. `pkg remove hello` removes the program, the cached `.ppkg` and the manifest.

Everything is stdlib: `http.server`, `socket`, `subprocess`, `struct`. The
kernel is expected to build first (`cargo build --locked`); pass `--release` to
use the release image.

Usage::

    python3 tools/test_pkg.py
    python3 tools/test_pkg.py --release
    python3 tools/test_pkg.py --port 8123 --kernel target/.../pagh

The serial transcript, the HTTP server log, the built package and the disk image
stay in a temporary directory that is printed at the end, so a failure can be
read after the fact.
"""

from __future__ import annotations

import argparse
import hashlib
import http.server
import os
import pathlib
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time

ROOT = pathlib.Path(__file__).resolve().parents[1]
KERNEL_DEBUG = ROOT / "target/riscv64gc-unknown-none-elf/debug/pagh"
KERNEL_RELEASE = ROOT / "target/riscv64gc-unknown-none-elf/release/pagh"
HELLO_ELF = ROOT / "userland/hello.elf"

PROMPT = "pagh:rv64>"
BANNER = "Type 'help' for the command list."
GATEWAY = "10.0.2.2"
PACKAGE_NAME = "hello"
PACKAGE_VERSION = "1.0.0"
PACKAGE_URL = f"pkg/{PACKAGE_NAME}-{PACKAGE_VERSION}.ppkg"
DISK_SIZE = "64M"


class Failure(Exception):
    """A test assertion failed; the caller prints the transcript tail."""


def die(message: str, code: int = 2) -> None:
    print(f"test_pkg: {message}", file=sys.stderr)
    raise SystemExit(code)


# ── the package and the repository ─────────────────────────────────────────


def build_package(root: pathlib.Path) -> dict[str, object]:
    """Write `pkg/hello-1.0.0.ppkg` and `index` under `root`.

    The manifest carries every key of the format (the kernel reads five; the
    rest are informational and deliberately present so the fixture looks like a
    published package, not a test stub). The index's `size` and `sha256` cover
    the whole `.ppkg`, which is what `tools/publish.py` checks in pagh-repo.
    """
    payload = HELLO_ELF.read_bytes()
    payload_sha = hashlib.sha256(payload).hexdigest()
    manifest = (
        f"name={PACKAGE_NAME}\n"
        f"version={PACKAGE_VERSION}\n"
        "arch=riscv64\n"
        "kind=app\n"
        f"commands={PACKAGE_NAME}\n"
        f"entry={PACKAGE_NAME}\n"
        "description=prints one line and exits 42\n"
        f"sha256={payload_sha}\n"
    ).encode()
    package = (
        b"PPKG0001"
        + struct.pack("<II", len(manifest), len(payload))
        + manifest
        + payload
    )
    digest = hashlib.sha256(package).hexdigest()

    pkg_dir = root / "pkg"
    pkg_dir.mkdir(parents=True, exist_ok=True)
    package_path = root / PACKAGE_URL
    package_path.write_bytes(package)
    index = (
        "PPKGINDEX 1\n"
        f"{PACKAGE_NAME}\t{PACKAGE_VERSION}\t{digest}\t{len(package)}"
        f"\tapp\t{PACKAGE_NAME}\t{PACKAGE_URL}\n"
    )
    (root / "index").write_text(index)
    return {
        "path": package_path,
        "digest": digest,
        "size": len(package),
        "payload": len(payload),
        "bytes": package,
    }


class LoggingHandler(http.server.SimpleHTTPRequestHandler):
    """A quiet file server that records each request for the log file."""

    def __init__(self, *args, directory=None, log_lines=None, **kwargs):
        self.log_lines = log_lines
        super().__init__(*args, directory=directory, **kwargs)

    def log_message(self, fmt: str, *args) -> None:
        if self.log_lines is not None:
            self.log_lines.append(fmt % args)

    def log_error(self, fmt: str, *args) -> None:
        self.log_message("ERROR " + fmt, *args)


def start_server(root: pathlib.Path, port: int) -> tuple[http.server.ThreadingHTTPServer, list[str], threading.Thread]:
    lines: list[str] = []
    handler = lambda *args, **kwargs: LoggingHandler(  # noqa: E731
        *args, directory=str(root), log_lines=lines, **kwargs
    )
    try:
        server = http.server.ThreadingHTTPServer(("127.0.0.1", port), handler)
    except OSError as error:
        die(f"cannot listen on 127.0.0.1:{port}: {error} (pass --port)")
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server, lines, thread


# ── QEMU and the serial line ───────────────────────────────────────────────


class Serial:
    """The guest's serial line: output comes back, keystrokes go in."""

    def __init__(self, path: pathlib.Path, process: subprocess.Popen, timeout: float = 20.0):
        self.path = path
        self.process = process
        self.data = bytearray()
        self.mark = 0
        self.socket = self._connect(timeout)

    def _connect(self, timeout: float) -> socket.socket:
        deadline = time.time() + timeout
        while time.time() < deadline:
            if self.process.poll() is not None:
                raise Failure("QEMU exited before the serial socket appeared")
            sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            try:
                sock.connect(str(self.path))
            except (FileNotFoundError, ConnectionRefusedError):
                sock.close()
                time.sleep(0.05)
                continue
            sock.setblocking(False)
            return sock
        raise Failure(f"cannot connect to the serial socket {self.path}")

    @property
    def text(self) -> str:
        return self.data.decode("utf-8", "replace")

    def pump(self, seconds: float) -> None:
        deadline = time.time() + seconds
        while True:
            remain = deadline - time.time()
            if remain <= 0:
                return
            import select

            readable, _, _ = select.select([self.socket], [], [], remain)
            if not readable:
                return
            try:
                chunk = self.socket.recv(65536)
            except BlockingIOError:
                continue
            if not chunk:
                return
            self.data.extend(chunk)

    def wait_for(self, marker: str, timeout: float) -> bool:
        deadline = time.time() + timeout
        while time.time() < deadline:
            self.pump(0.1)
            position = self.text.find(marker, self.mark)
            if position >= 0:
                self.mark = position + len(marker)
                return True
            if self.process.poll() is not None:
                self.pump(0.2)
                position = self.text.find(marker, self.mark)
                if position >= 0:
                    self.mark = position + len(marker)
                    return True
                return False
        return False

    def send(self, text: str) -> None:
        self.socket.sendall(text.encode())

    def segment(self, start: int) -> str:
        return self.text[start : self.mark]

    def close(self) -> None:
        try:
            self.socket.close()
        except OSError:
            pass


class Guest:
    """One booted kernel and its shell."""

    def __init__(self, args: argparse.Namespace, work: pathlib.Path):
        self.args = args
        self.work = work
        self.disk = work / "disk.img"
        self.log = work / "serial.log"
        self.qemu_log = work / "qemu.log"
        self.pcap = work / "network.pcap"
        self.socket_path = work / "serial.sock"
        self.process: subprocess.Popen | None = None
        self.serial: Serial | None = None

    def boot(self, timeout: float = 120.0) -> None:
        subprocess.run(
            ["qemu-img", "create", "-f", "raw", str(self.disk), DISK_SIZE],
            check=True,
            stdout=subprocess.DEVNULL,
        )
        qemu = os.environ.get("QEMU", "qemu-system-riscv64")
        if not os.path.isabs(qemu):
            found = shutil.which(qemu)
            if found is None:
                die(f"{qemu} is not on PATH")
            qemu = found
        bios = (os.environ.get("PAGH_BIOS") or "").strip() or "default"
        command = [
            qemu,
            "-machine",
            "virt",
            "-m",
            "256M",
            "-bios",
            bios,
            "-kernel",
            str(self.args.kernel),
            "-display",
            "none",
            "-no-reboot",
            "-serial",
            f"unix:{self.socket_path},server=on,wait=on",
            "-monitor",
            "none",
            # The kernel speaks modern virtio only.
            "-global",
            "virtio-mmio.force-legacy=false",
            "-device",
            "virtio-tablet-device",
            # QEMU's user network: the guest gets 10.0.2.15 and reaches the
            # host's loopback as 10.0.2.2.
            "-netdev",
            "user,id=net0",
            "-device",
            "virtio-net-device,netdev=net0",
            # A pcap of the guest's only conversations (index + package) makes a
            # stalled TCP download diagnosable after the fact.
            "-object",
            f"filter-dump,id=pkgdump,netdev=net0,file={self.pcap}",
            "-drive",
            f"file={self.disk},format=raw,if=none,id=hd0",
            "-device",
            "virtio-blk-device,drive=hd0",
        ]
        print("+", " ".join(command))
        with self.qemu_log.open("wb") as qemu_output:
            self.process = subprocess.Popen(
                command, stdout=qemu_output, stderr=subprocess.STDOUT
            )
        self.serial = Serial(self.socket_path, self.process)
        if not self.serial.wait_for(BANNER, timeout):
            raise Failure(f"the boot banner did not appear within {timeout:.0f}s")
        # DHCP runs before the shell (and before the banner), so the lease is
        # already in the transcript by now; check it there rather than waiting
        # for a line that has passed. A missing lease must not be mistaken for
        # an installer bug.
        if "net         DHCP ip 10.0.2.15/" not in self.serial.text:
            raise Failure("the guest did not get a DHCP lease")
        if not self.serial.wait_for(PROMPT, timeout):
            raise Failure("the shell prompt did not appear")

    def command(self, command: str, expect: str | None = None, timeout: float = 90.0) -> tuple[bool, str]:
        assert self.serial is not None
        start = self.serial.mark
        self.serial.send(command + "\r")
        found = True if expect is None else self.serial.wait_for(expect, timeout)
        prompted = self.serial.wait_for(PROMPT, timeout)
        return found and prompted, self.serial.segment(start)

    def halt(self) -> int:
        assert self.process is not None
        if self.serial is not None:
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
            try:
                self.serial.send("halt\r")
            except OSError:
                pass
            self.serial.close()
            self.serial = None
        if self.process is not None and self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)

    def save_logs(self) -> None:
        if self.serial is not None:
            self.log.write_text(self.serial.text)


# ── assertions ─────────────────────────────────────────────────────────────


def lines_of(segment: str) -> list[str]:
    return [line.strip() for line in segment.replace("\r", "\n").split("\n")]


def has_line(segment: str, wanted: str) -> bool:
    return any(line == wanted for line in lines_of(segment))


def check(condition: bool, what: str, segment: str = "") -> None:
    if condition:
        print(f"  ok   {what}")
        return
    print(f"  FAIL {what}", file=sys.stderr)
    if segment:
        print("  --- command output ---", file=sys.stderr)
        print(segment, file=sys.stderr)
    raise Failure(what)


def expect_ok(guest: Guest, command: str, expect: str, timeout: float = 90.0) -> str:
    ok, segment = guest.command(command, expect, timeout)
    check(ok, f"{command!r} produced {expect!r}", segment)
    return segment


def run_case(args: argparse.Namespace, work: pathlib.Path, package: dict[str, object]) -> None:
    guest = Guest(args, work)
    try:
        guest.boot()
        print("boot: disk formatted, DHCP lease, shell ready")

        segment = expect_ok(
            guest,
            "pkg repo http://10.0.2.2:%d/" % args.port,
            "pkg: repo http://10.0.2.2",
        )
        expect_ok(guest, "pkg index", "pkg: index 1 package(s)")
        segment = expect_ok(guest, "pkg list", f"{PACKAGE_NAME} {PACKAGE_VERSION}")
        check("[installed]" not in segment, "a fresh disk lists the package as not installed")

        expect_ok(guest, f"pkg install {PACKAGE_NAME}", f"pkg: installed {PACKAGE_NAME} {PACKAGE_VERSION}")
        segment = expect_ok(guest, "ls /mnt/bin", PACKAGE_NAME)
        check(has_line(segment, PACKAGE_NAME), f"/mnt/bin contains {PACKAGE_NAME}", segment)
        segment = expect_ok(guest, f"pkg list", "[installed]")
        check("[installed]" in segment, "pkg list marks the installed package")

        segment = expect_ok(guest, "run /mnt/bin/hello", "hello from user mode")
        check(
            "run: /mnt/bin/hello exited with code 42" in segment,
            "run /mnt/bin/hello exited with code 42",
            segment,
        )
        segment = expect_ok(guest, "pkg installed", "pkg: hello 1.0.0")
        check("pkg: 1 package(s) installed" in segment, "pkg installed counts one package", segment)

        # ── the negative case: same index, one host byte flipped ──────────
        package_path = package["path"]
        assert isinstance(package_path, pathlib.Path)
        corrupted = bytearray(package_path.read_bytes())
        corrupted[len(corrupted) // 2] ^= 0x01
        package_path.write_bytes(corrupted)
        check(len(corrupted) == package["size"], "the corrupted copy has the same size")

        expect_ok(guest, f"pkg remove {PACKAGE_NAME}", f"pkg: removed {PACKAGE_NAME}")
        segment = expect_ok(guest, "ls /mnt/bin", None)
        check(not has_line(segment, PACKAGE_NAME), "pkg remove removed /mnt/bin/hello", segment)

        segment = expect_ok(
            guest,
            f"pkg install {PACKAGE_NAME}",
            "pkg: install: SHA-256 does not match the index",
        )
        check("pkg: installed" not in segment, "the corrupted package was not installed", segment)
        segment = expect_ok(guest, "ls /mnt/bin", None)
        check(not has_line(segment, PACKAGE_NAME), "no /mnt/bin/hello after the refusal", segment)
        segment = expect_ok(guest, "ls /mnt/pkg", None)
        check(not has_line(segment, f"{PACKAGE_NAME}.ppkg"), "the failed download was removed", segment)
        segment = expect_ok(guest, "pkg installed", "pkg: 0 package(s) installed")

        if guest.halt() != 0:
            raise Failure("halt did not power QEMU off with status 0")
        guest.save_logs()
        print(f"serial log: {guest.log}")
        guest.serial = None
    finally:
        if guest.serial is not None:
            guest.save_logs()
        guest.shutdown()


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Build a .ppkg, serve it over HTTP and drive the kernel's pkg installer in QEMU"
    )
    parser.add_argument("--kernel", type=pathlib.Path, default=KERNEL_DEBUG, help=f"kernel ELF (default {KERNEL_DEBUG})")
    parser.add_argument("--release", action="store_true", help=f"use {KERNEL_RELEASE}")
    parser.add_argument("--port", type=int, default=8000, help="host HTTP port (default 8000)")
    parser.add_argument("--keep", action="store_true", help="keep the work directory (it is always printed)")
    args = parser.parse_args()
    if args.release:
        args.kernel = KERNEL_RELEASE
    args.kernel = args.kernel.resolve()
    if not args.kernel.is_file():
        die(f"kernel {args.kernel} not found — run `cargo build --locked`")
    if not HELLO_ELF.is_file():
        die(f"fixture {HELLO_ELF} not found")

    work = pathlib.Path(tempfile.mkdtemp(prefix="pagh-pkg-test-"))
    repo = work / "repo"
    package = build_package(repo)
    server, http_lines, _thread = start_server(repo, args.port)
    print(f"test_pkg: repository {repo} ({package['size']} bytes, sha256 {package['digest'][:16]}…)")
    try:
        run_case(args, work, package)
    except Failure as failure:
        print(f"test_pkg: FAILED: {failure}", file=sys.stderr)
        if (work / "serial.log").is_file():
            print("--- serial tail ---", file=sys.stderr)
            print("\n".join((work / "serial.log").read_text().splitlines()[-40:]), file=sys.stderr)
        print(f"test_pkg: work directory {work}", file=sys.stderr)
        return 1
    finally:
        server.shutdown()
        (work / "http.log").write_text("\n".join(http_lines) + "\n")
        if not args.keep:
            # The logs are the point; the 64 MiB disk image is not.
            disk = work / "disk.img"
            if disk.exists():
                disk.unlink()
    print(f"test_pkg: PASS (logs in {work})")
    return 0


if __name__ == "__main__":
    sys.exit(main())