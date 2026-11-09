#!/usr/bin/env python3
"""Live TCP server acceptance for pagh-riscv: libpagh fixture + QEMU user net.

This is the evidence harness behind `docs/live-tcp-server.txt`. It uses the
native server fixture end to end:

1. `mke2fs -d` writes a 64 MiB ext2 image directly, with the libpagh
   `tcp_echo` ELF at `/tcp_echo` (the guest sees `/mnt/tcp_echo`; no first boot
   is needed to format the disk);
2. `tools/run.py` boots the kernel with `--net` and a QEMU `hostfwd` that maps
   a host loopback port to the guest's port 45679;
3. a host client connects through `127.0.0.1:<host-port>`, writes a payload,
   shuts down its write side and reads the echo until EOF;
4. the guest shell runs `run /mnt/tcp_echo 45679`; the fixture binds
   `0.0.0.0:45679`, listens with backlog 4, accepts one connection, reads to
   EOF, echoes the bytes and exits 0;
5. the serial transcript must contain every fixture marker and the run's exit
   line, and the host must have received exactly the payload.

Usage::

    cargo build --locked
    (cd ../pagh-api && cargo build --release --example tcp_echo \\
        --target riscv64gc-unknown-none-elf)
    python3 tools/test_tcp_server.py

The script is a manual acceptance run (like `test_udp.py`): it needs a host
process and QEMU's user network, which CI does not provide.
"""

from __future__ import annotations

import argparse
import pathlib
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time

ROOT = pathlib.Path(__file__).resolve().parents[1]
KERNEL_DEBUG = ROOT / "target/riscv64gc-unknown-none-elf/debug/pagh"
KERNEL_RELEASE = ROOT / "target/riscv64gc-unknown-none-elf/release/pagh"
DEFAULT_ELF = (
    ROOT.parent
    / "pagh-api"
    / "target/riscv64gc-unknown-none-elf/release/examples/tcp_echo"
)
GUEST_PORT = 45679
DISK_SIZE = "64M"
# More than one MSS, so the host's write is split into several TCP segments
# (and the guest's ACKs/FIN can arrive in one virtio-net completion burst).
PAYLOAD = b"pagh tcp server live test\n" * 40
TIMEOUT = 180.0


class Failure(RuntimeError):
    """A check failed; the caller prints the transcript and exits 1."""


def die(message: str, code: int = 2) -> None:
    print(f"test_tcp_server: {message}", file=sys.stderr)
    raise SystemExit(code)


def free_host_port() -> int:
    """Reserve an ephemeral loopback port, then release it for QEMU hostfwd."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    try:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]
    finally:
        sock.close()


class EchoClient:
    """One host client: connect (retrying until QEMU is up), send, half-close,
    read the echo to EOF."""

    def __init__(self, port: int, payload: bytes, timeout: float = TIMEOUT):
        self.port = port
        self.payload = payload
        self.timeout = timeout
        self.received = b""
        self.error: str | None = None
        self.connected_at: float | None = None
        self.thread = threading.Thread(target=self._run, daemon=True)

    def _run(self) -> None:
        deadline = time.time() + self.timeout
        client = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        try:
            while time.time() < deadline:
                try:
                    client.connect(("127.0.0.1", self.port))
                    break
                except OSError:
                    time.sleep(0.1)
            else:
                self.error = "host client could not connect before the timeout"
                return
            self.connected_at = time.time()
            client.settimeout(self.timeout)
            client.sendall(self.payload)
            client.shutdown(socket.SHUT_WR)
            chunks = []
            while True:
                chunk = client.recv(4096)
                if not chunk:
                    break
                chunks.append(chunk)
            self.received = b"".join(chunks)
        except OSError as error:
            self.error = str(error)
        finally:
            client.close()

    def start(self) -> None:
        self.thread.start()

    def join(self, timeout: float = 30.0) -> None:
        self.thread.join(timeout)

    @property
    def alive(self) -> bool:
        return self.thread.is_alive()


def make_image(work: pathlib.Path, elf: pathlib.Path) -> pathlib.Path:
    """Format an ext2 image with `mke2fs -d`, fixture included, and no boot."""
    mke2fs = shutil.which("mke2fs")
    if mke2fs is None:
        die("mke2fs (e2fsprogs) is required to build the fixture image")
    root = work / "root"
    root.mkdir()
    target = root / "tcp_echo"
    shutil.copyfile(elf, target)
    target.chmod(0o755)
    disk = work / "disk.img"
    with disk.open("wb") as file:
        file.truncate(64 * 1024 * 1024)
    command = [
        mke2fs,
        "-q",
        "-t",
        "ext2",
        "-b",
        "1024",
        "-F",
        "-d",
        str(root),
        str(disk),
    ]
    print("+", " ".join(command))
    result = subprocess.run(command, capture_output=True, text=True)
    if result.returncode != 0:
        die(f"mke2fs failed: {result.stderr.strip()}\n{result.stdout.strip()}")
    return disk


def run_guest(
    kernel: pathlib.Path,
    disk: pathlib.Path,
    host_port: int,
    work: pathlib.Path,
    settle: float,
) -> str:
    serial_log = work / "serial.log"
    command = [
        sys.executable,
        str(ROOT / "tools" / "run.py"),
        "--kernel",
        str(kernel),
        "--disk",
        str(disk),
        "--net",
        "--hostfwd",
        f"{host_port}:{GUEST_PORT}",
        "--keys",
        f"run /mnt/tcp_echo {GUEST_PORT}\n",
        "--serial",
        str(work / "serial.sock"),
        "--monitor",
        str(work / "monitor.sock"),
        "--serial-log",
        str(serial_log),
        "--shot",
        str(work / "screen.png"),
        "--settle",
        str(settle),
        "--timeout",
        "120",
    ]
    print("+", " ".join(command))
    result = subprocess.run(
        command,
        cwd=ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        timeout=TIMEOUT,
    )
    transcript = (
        serial_log.read_text(encoding="utf-8", errors="replace")
        if serial_log.exists()
        else ""
    )
    if result.returncode != 0:
        raise Failure(
            f"tools/run.py failed with status {result.returncode}\n"
            f"--- run.py output ---\n{result.stdout}\n--- serial ---\n{transcript}"
        )
    return transcript


def check(condition: bool, what: str, transcript: str) -> None:
    if not condition:
        raise Failure(f"{what}\n--- serial transcript ---\n{transcript}")


def run(args: argparse.Namespace) -> int:
    elf = args.elf.resolve()
    kernel = args.kernel.resolve()
    if not elf.is_file():
        die(f"tcp_echo ELF {elf} not found — build it in ../pagh-api")
    if not kernel.is_file():
        die(f"kernel {kernel} not found — run `cargo build --locked`")
    if not (1000 < elf.stat().st_size < 1 << 20):
        die(f"{elf} is {elf.stat().st_size} bytes, outside the loader's 1 MiB window")

    work = pathlib.Path(tempfile.mkdtemp(prefix="pagh-tcp-server-live-"))
    disk = make_image(work, elf)

    host_port = free_host_port()
    client = EchoClient(host_port, PAYLOAD)
    client.start()
    live_log = run_guest(kernel, disk, host_port, work, args.settle)

    check("net         DHCP ip 10.0.2.15/" in live_log, "the guest did not get a DHCP lease", live_log)
    check(
        f"tcp: bound 0.0.0.0:{GUEST_PORT} backlog 4" in live_log,
        "the fixture did not bind/listen",
        live_log,
    )
    check("tcp: listening" in live_log, "the fixture did not report its listening state", live_log)
    check("tcp: accept from 10.0.2.2:" in live_log, "the fixture did not accept a host connection", live_log)
    check(
        f"tcp: recv {len(PAYLOAD)} bytes" in live_log,
        f"the fixture did not receive {len(PAYLOAD)} bytes",
        live_log,
    )
    check(
        f"tcp: echoed {len(PAYLOAD)} bytes" in live_log,
        "the fixture did not echo the payload",
        live_log,
    )
    check("tcp: ok" in live_log, "the fixture did not print its success marker", live_log)
    check(
        "run: /mnt/tcp_echo exited with code 0" in live_log,
        "the fixture did not exit 0",
        live_log,
    )

    client.join(timeout=30)
    check(not client.alive, "the host client never finished", live_log)
    check(client.error is None, f"host client error: {client.error}", live_log)
    check(
        client.received == PAYLOAD,
        f"host received {len(client.received)} bytes, expected {len(PAYLOAD)}",
        live_log,
    )

    transcript = (
        "# Live TCP server acceptance for pagh-riscv\n"
        f"# kernel: {kernel}\n"
        f"# fixture: {elf} ({elf.stat().st_size} bytes)\n"
        f"# image: mke2fs -t ext2 -b 1024 -d <root with tcp_echo> disk.img\n"
        f"# host client: 127.0.0.1:{host_port} -> QEMU hostfwd -> guest "
        f"10.0.2.15:{GUEST_PORT}\n"
        f"# payload: {len(PAYLOAD)} bytes\n"
        "\n== creation ==\n"
        "mke2fs -q -t ext2 -b 1024 -F -d root disk.img   # root/tcp_echo = libpagh tcp_echo\n"
        "\n== guest boot and run ==\n"
        f"{live_log}\n"
        "\n== host client ==\n"
        f"connected at {client.connected_at}\n"
        f"sent {len(PAYLOAD)} bytes, shutdown(SHUT_WR)\n"
        f"received {len(client.received)} bytes: {client.received!r}\n"
        "\nRESULT: PASS — bind/listen/accept/recv/echo/close over QEMU user net, "
        "fixture exit 0\n"
    )
    transcript_path = args.transcript
    if not transcript_path.is_absolute():
        transcript_path = ROOT / transcript_path
    # Save the canonical LF form (the serial stream carries CR); the fixture
    # output itself has no trailing spaces.
    text = "\n".join(line.rstrip() for line in transcript.splitlines()) + "\n"
    transcript_path.write_text(text, encoding="utf-8")
    print(f"test_tcp_server: PASS — {transcript_path} ({len(transcript)} bytes)")
    print(
        f"test_tcp_server: host received {len(client.received)} bytes from "
        f"127.0.0.1:{host_port} -> guest {GUEST_PORT}"
    )
    if not args.keep:
        shutil.rmtree(work, ignore_errors=True)
    else:
        print(f"test_tcp_server: workdir {work}")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description="Live TCP server acceptance for pagh-riscv")
    parser.add_argument(
        "--elf",
        type=pathlib.Path,
        default=DEFAULT_ELF,
        help=f"tcp_echo ELF (default {DEFAULT_ELF})",
    )
    parser.add_argument(
        "--kernel",
        type=pathlib.Path,
        default=KERNEL_DEBUG,
        help=f"kernel ELF (default {KERNEL_DEBUG})",
    )
    parser.add_argument("--release", action="store_true", help=f"use {KERNEL_RELEASE}")
    parser.add_argument(
        "--settle",
        type=float,
        default=6.0,
        help="seconds to drain after typing (default 6)",
    )
    parser.add_argument(
        "--transcript",
        type=pathlib.Path,
        default=ROOT / "docs/live-tcp-server.txt",
        help="where to write the transcript (default docs/live-tcp-server.txt)",
    )
    parser.add_argument("--keep", action="store_true", help="keep the temporary workdir")
    args = parser.parse_args()
    if args.release and args.kernel == KERNEL_DEBUG:
        args.kernel = KERNEL_RELEASE
    try:
        return run(args)
    except Failure as failure:
        print(f"test_tcp_server: FAIL — {failure}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
