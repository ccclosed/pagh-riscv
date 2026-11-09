#!/usr/bin/env python3
"""Live BSD-socket acceptance for pagh-riscv (host HTTP server + QEMU user net).

This is the evidence harness behind `docs/live-sockets.txt`. It is deliberately
outside the in-QEMU `selftest` suite: it needs a host process listening on
`127.0.0.1:8000` and a disk image the host can populate.

What it does, in order:

1. builds a blank disk, boots the kernel once so the kernel formats ext2, and
   halts; `debugfs` then injects the libpagh `http_get` example at `/http_get`
   (which the guest sees as `/mnt/http_get`);
2. starts a host HTTP/1.0 server on `127.0.0.1:8000` that answers `GET /` with a
   fixed body (`10.0.2.2:8000` inside QEMU's user network is the host loopback);
3. boots the same disk again, verifies the mount and the file, runs the shell's
   `selftest` — the ABI fixture's connected-socket half then exercises
   `connect`/`send`/`recv`/`shutdown`/EOF/`EPIPE` against the live server — and
   finally runs `/mnt/http_get`, checking the HTTP response body and exit code;
4. halts QEMU and writes a transcript.

Usage::

    cargo build --locked
    python3 tools/test_sockets.py
    python3 tools/test_sockets.py --elf /path/to/http_get --port 8000
    python3 tools/test_sockets.py --release

The `http_get` ELF is built from the sibling `pagh-api` checkout:

    cd ../pagh-api
    cargo build --release --example http_get --target riscv64gc-unknown-none-elf

The script is not wired into CI, for the same reason `tools/test_pkg.py` is
not: CI has no host HTTP server. It is a manual acceptance run.
"""

from __future__ import annotations

import argparse
import http.server
import os
import pathlib
import shutil
import subprocess
import sys
import tempfile
import threading
import time

ROOT = pathlib.Path(__file__).resolve().parents[1]
# Reuse the QEMU serial driver from the package E2E: one booted shell, one
# Unix-socket serial line, the same failure semantics.
sys.path.insert(0, str(ROOT / "tools"))
from test_pkg import BANNER, PROMPT, Failure, Serial  # noqa: E402

KERNEL_DEBUG = ROOT / "target/riscv64gc-unknown-none-elf/debug/pagh"
KERNEL_RELEASE = ROOT / "target/riscv64gc-unknown-none-elf/release/pagh"
DEFAULT_ELF = (
    ROOT.parent
    / "pagh-api"
    / "target/riscv64gc-unknown-none-elf/release/examples/http_get"
)
DISK_SIZE = "64M"
BODY = b"pagh sockets live: GET / ok\n"


def die(message: str, code: int = 2) -> None:
    print(f"test_sockets: {message}", file=sys.stderr)
    raise SystemExit(code)


# ── the host HTTP server ──────────────────────────────────────────────────


def start_http_server(port: int, requests: list[str]):
    """Serve one fixed `GET /` response on the host loopback.

    HTTP/1.0's default is connection-close, which is what the guest's
    `read_to_end` relies on for EOF.
    """

    class Handler(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.0"

        def do_GET(self) -> None:  # noqa: N802 (stdlib spelling)
            requests.append(f"{self.command} {self.path} from {self.client_address[0]}")
            self.send_response(200)
            self.send_header("Content-Type", "text/plain")
            self.send_header("Content-Length", str(len(BODY)))
            self.end_headers()
            self.wfile.write(BODY)

        def log_message(self, fmt: str, *args) -> None:
            requests.append(fmt % args)

    server = http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server, thread


# ── one booted kernel ─────────────────────────────────────────────────────


class Kernel:
    """A QEMU `virt` guest with virtio-net, virtio-blk and a serial socket."""

    def __init__(self, kernel: pathlib.Path, work: pathlib.Path):
        self.kernel = kernel
        self.work = work
        self.disk = work / "disk.img"
        self.log = work / "serial.log"
        self.qemu_log = work / "qemu.log"
        self.pcap = work / "network.pcap"
        self.socket_path = work / "serial.sock"
        self.process: subprocess.Popen | None = None
        self.serial: Serial | None = None

    def boot(self, timeout: float = 120.0) -> None:
        if not self.disk.exists():
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
            "-netdev",
            "user,id=net0",
            "-device",
            "virtio-net-device,netdev=net0",
            "-object",
            f"filter-dump,id=sockdump,netdev=net0,file={self.pcap}",
            "-drive",
            f"file={self.disk},format=raw,if=none,id=hd0",
            "-device",
            "virtio-blk-device,drive=hd0",
        ]
        print("+", " ".join(command))
        with self.qemu_log.open("wb") as output:
            self.process = subprocess.Popen(command, stdout=output, stderr=subprocess.STDOUT)
        self.serial = Serial(self.socket_path, self.process)
        if not self.serial.wait_for(BANNER, timeout):
            raise Failure(f"the boot banner did not appear within {timeout:.0f}s")
        if "net         DHCP ip 10.0.2.15/" not in self.serial.text:
            raise Failure("the guest did not get a DHCP lease")
        if not self.serial.wait_for(PROMPT, timeout):
            raise Failure("the shell prompt did not appear")

    def command(self, command: str, expect: str | None = None, timeout: float = 90.0) -> str:
        assert self.serial is not None
        start = self.serial.mark
        self.serial.send(command + "\r")
        found = True if expect is None else self.serial.wait_for(expect, timeout)
        prompted = self.serial.wait_for(PROMPT, timeout)
        segment = self.serial.segment(start)
        if not (found and prompted):
            raise Failure(f"{command!r} did not produce {expect!r}; transcript:\n{segment}")
        return segment

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

    def save_log(self) -> str:
        text = self.serial.text if self.serial is not None else ""
        self.log.write_text(text, encoding="utf-8", errors="replace")
        return text

    def close(self) -> None:
        if self.serial is not None:
            self.serial.close()
            self.serial = None
        if self.process is not None and self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)


# ── assertions ────────────────────────────────────────────────────────────


def check(condition: bool, what: str, transcript: str = "") -> None:
    if not condition:
        raise Failure(f"{what}\n--- transcript ---\n{transcript}")


def inject_file(disk: pathlib.Path, source: pathlib.Path) -> None:
    debugfs = shutil.which("debugfs")
    if debugfs is None:
        die("debugfs (e2fsprogs) is required to inject the example")
    result = subprocess.run(
        [debugfs, "-w", "-R", f"write {source} /http_get", str(disk)],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        die(f"debugfs write failed: {result.stderr.strip()}")
    result = subprocess.run(
        [debugfs, "-w", "-R", "sif /http_get mode 0100755", str(disk)],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        die(f"debugfs sif failed: {result.stderr.strip()}")


# ── the run ───────────────────────────────────────────────────────────────


def run(args: argparse.Namespace) -> None:
    elf = args.elf.resolve()
    if not elf.is_file():
        die(f"http_get ELF {elf} not found — build it in ../pagh-api")
    kernel = args.kernel.resolve()
    if not kernel.is_file():
        die(f"kernel {kernel} not found — run `cargo build --locked`")
    if not (1000 < elf.stat().st_size < 1 << 20):
        die(f"{elf} is {elf.stat().st_size} bytes, outside the loader's window")

    work = pathlib.Path(tempfile.mkdtemp(prefix="pagh-sock-live-"))
    requests: list[str] = []
    server, thread = start_http_server(args.port, requests)
    transcript: list[str] = [
        "# Live BSD-socket acceptance for pagh-riscv",
        f"# kernel: {kernel}",
        f"# http_get: {elf} ({elf.stat().st_size} bytes)",
        f"# host server: 127.0.0.1:{args.port} (guest reachable as 10.0.2.2:{args.port})",
        "",
    ]
    guest: Kernel | None = None
    try:
        # 1. Format pass: the kernel is the one that creates the ext2 image.
        print("boot 1/2: format the disk")
        guest = Kernel(kernel, work)
        guest.boot()
        guest.halt()
        transcript.append("== format boot ==")
        transcript.append(guest.save_log())
        guest.close()
        guest = None

        inject_file(work / "disk.img", elf)
        print(f"injected {elf.name} into {work / 'disk.img'} at /http_get")

        # 2. Main pass: the live server is up, so the ABI fixture's connected
        #    socket half runs instead of skipping.
        print("boot 2/2: selftest + run /mnt/http_get")
        guest = Kernel(kernel, work)
        guest.boot()
        listing = guest.command("ls /mnt", expect="http_get")
        transcript.append("== main boot ==")
        transcript.append(listing)

        selftest = guest.command(
            "selftest",
            expect="SELFTEST SUMMARY (user): 10/10 passed",
            timeout=240,
        )
        transcript.append(selftest)
        check("SELFTEST user-abi-syscalls PASS" in selftest, "the ABI fixture failed", selftest)
        check("abi: socket connected" in selftest, "the fixture did not connect", selftest)
        check(
            "abi: socket shutdown/EOF ok" in selftest,
            "the fixture did not reach shutdown/EOF",
            selftest,
        )
        check("abi: socket SKIP" not in selftest, "the fixture still skipped the socket", selftest)

        response = guest.command(
            "run /mnt/http_get",
            expect="http_get: done",
            timeout=90,
        )
        transcript.append(response)
        check(
            "run: /mnt/http_get exited with code 0" in response,
            "http_get did not exit 0",
            response,
        )
        check(
            BODY.decode().strip() in response,
            "the HTTP response body is missing",
            response,
        )
        check(
            "http_get: connected, sending GET /" in response,
            "http_get did not report its request",
            response,
        )

        status = guest.halt()
        check(status == 0, f"halt exited {status}", response)
        transcript.append("")
        transcript.append("== host HTTP requests ==")
        transcript.extend(requests)
        transcript.append("")
        transcript.append("RESULT: PASS — selftest 10/10 with a live socket, http_get exit 0")

        args.transcript.parent.mkdir(parents=True, exist_ok=True)
        # Serial text carries the guest's CR characters; the committed
        # transcript uses plain LF so `git diff --check` stays quiet.
        text = ("\n".join(transcript) + "\n").replace("\r\n", "\n").replace("\r", "")
        args.transcript.write_text(text, encoding="utf-8")
        print(f"transcript: {args.transcript}")
        print("LIVE SOCKETS: PASS")
    finally:
        if guest is not None:
            guest.save_log()
            guest.close()
        server.shutdown()
        thread.join(timeout=5)
        server.server_close()
        if not args.keep:
            shutil.rmtree(work, ignore_errors=True)
        else:
            print(f"work directory kept: {work}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kernel", type=pathlib.Path, default=KERNEL_DEBUG)
    parser.add_argument("--release", action="store_true")
    parser.add_argument("--elf", type=pathlib.Path, default=DEFAULT_ELF)
    parser.add_argument("--port", type=int, default=8000, help="host port (must be free)")
    parser.add_argument(
        "--transcript",
        type=pathlib.Path,
        default=ROOT / "docs/live-sockets.txt",
        help="transcript path (default docs/live-sockets.txt)",
    )
    parser.add_argument("--keep", action="store_true", help="keep the work directory")
    args = parser.parse_args()
    if args.release:
        args.kernel = KERNEL_RELEASE
    try:
        run(args)
    except Failure as error:
        print(f"test_sockets: FAIL: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())