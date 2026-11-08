#!/usr/bin/env python3
"""Live UDP acceptance for pagh-riscv: libpagh fixture + QEMU user net + ext2.

This is the evidence harness behind `docs/live-udp.txt`. It deliberately uses
the same two tools a human would:

1. `tools/run.py` boots the kernel once with a blank raw image attached, so the
   kernel formats and mounts ext2 at `/mnt`;
2. `debugfs` injects the libpagh `udp_echo` fixture at `/udp_echo`;
3. a host UDP echo server binds `127.0.0.1:<port>` (QEMU's user network maps the
   guest's `10.0.2.2:<port>` to that address);
4. `tools/run.py` boots the same image with `--net` and types
   `run /mnt/udp_echo 10.0.2.2 <port> <local-port>`;
5. the transcript must contain the bind/send/receive markers, the host server
   must have received and echoed the payload, and the fixture must exit 0.

Usage::

    cargo build --locked
    (cd ../pagh-api && cargo build --release --example udp_echo --target riscv64gc-unknown-none-elf)
    python3 tools/test_udp.py

The script writes the full transcript to `docs/live-udp.txt` (override with
``--transcript``). It is a manual acceptance run: CI has no host UDP server.
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
    / "target/riscv64gc-unknown-none-elf/release/examples/udp_echo"
)
PAYLOAD = b"pagh udp echo"


class Failure(RuntimeError):
    """A check failed; the caller prints the transcript and exits 1."""


def die(message: str, code: int = 2) -> None:
    print(f"test_udp: {message}", file=sys.stderr)
    raise SystemExit(code)


class EchoServer:
    """One-shot host UDP echo server on 127.0.0.1:0 (an ephemeral port)."""

    def __init__(self) -> None:
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.sock.bind(("127.0.0.1", 0))
        self.sock.settimeout(120)
        self.port = self.sock.getsockname()[1]
        self.received: list[tuple[bytes, tuple[str, int]]] = []
        self.error: str | None = None
        self.thread = threading.Thread(target=self._serve, daemon=True)

    def _serve(self) -> None:
        try:
            data, peer = self.sock.recvfrom(4096)
            self.received.append((data, peer))
            self.sock.sendto(data, peer)
        except OSError as error:
            self.error = str(error)
        finally:
            self.sock.close()

    def start(self) -> None:
        self.thread.start()

    def join(self, timeout: float = 120) -> None:
        self.thread.join(timeout)

    @property
    def alive(self) -> bool:
        return self.thread.is_alive()


def run_qemu(
    kernel: pathlib.Path,
    disk: pathlib.Path,
    *,
    net: bool,
    keys: str,
    settle: float,
    work: pathlib.Path,
    tag: str,
    timeout: float = 180.0,
) -> str:
    """Run tools/run.py once and return its serial transcript."""
    serial_log = work / f"{tag}.log"
    command = [
        sys.executable,
        str(ROOT / "tools" / "run.py"),
        "--kernel",
        str(kernel),
        "--disk",
        str(disk),
        "--shot",
        str(work / f"{tag}.png"),
        "--serial",
        str(work / f"{tag}.serial.sock"),
        "--monitor",
        str(work / f"{tag}.monitor.sock"),
        "--serial-log",
        str(serial_log),
        "--settle",
        str(settle),
        "--timeout",
        str(timeout),
    ]
    if net:
        command.append("--net")
    if keys:
        command += ["--keys", keys]
    print("+", " ".join(command))
    result = subprocess.run(
        command,
        cwd=ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        timeout=timeout + 60,
    )
    transcript = serial_log.read_text(encoding="utf-8", errors="replace") if serial_log.exists() else ""
    if result.returncode != 0:
        raise Failure(
            f"tools/run.py failed with status {result.returncode}\n"
            f"--- run.py output ---\n{result.stdout}\n--- serial ---\n{transcript}"
        )
    return transcript


def inject(disk: pathlib.Path, elf: pathlib.Path) -> None:
    debugfs = shutil.which("debugfs")
    if debugfs is None:
        die("debugfs (e2fsprogs) is required to inject the fixture")
    for command in (
        f"write {elf} /udp_echo",
        "sif /udp_echo mode 0100755",
    ):
        result = subprocess.run(
            [debugfs, "-w", "-R", command, str(disk)],
            capture_output=True,
            text=True,
        )
        if result.returncode != 0:
            die(f"debugfs {command!r} failed: {result.stderr.strip()}")


def check(condition: bool, what: str, transcript: str) -> None:
    if not condition:
        raise Failure(f"{what}\n--- serial transcript ---\n{transcript}")


def run(args: argparse.Namespace) -> int:
    elf = args.elf.resolve()
    kernel = args.kernel.resolve()
    if not elf.is_file():
        die(f"udp_echo ELF {elf} not found — build it in ../pagh-api")
    if not kernel.is_file():
        die(f"kernel {kernel} not found — run `cargo build --locked`")
    if not (1000 < elf.stat().st_size < 1 << 20):
        die(f"{elf} is {elf.stat().st_size} bytes, outside the loader's 1 MiB window")

    work = pathlib.Path(tempfile.mkdtemp(prefix="pagh-udp-live-"))
    disk = work / "disk.img"
    subprocess.run(
        ["qemu-img", "create", "-f", "raw", str(disk), "64M"],
        check=True,
        stdout=subprocess.DEVNULL,
    )

    # First boot: let the kernel format the blank image. No keys are typed;
    # `run.py` still waits for the prompt, which proves the mount came up.
    format_log = run_qemu(kernel, disk, net=False, keys="", settle=1.0, work=work, tag="format")
    check(
        "fs          ext2 on /mnt" in format_log,
        "the format boot did not mount ext2 on /mnt",
        format_log,
    )
    inject(disk, elf)

    server = EchoServer()
    server.start()
    local_port = 45678
    keys = f"run /mnt/udp_echo 10.0.2.2 {server.port} {local_port}\n"
    live_log = run_qemu(
        kernel,
        disk,
        net=True,
        keys=keys,
        settle=max(args.settle, 2.0),
        work=work,
        tag="live",
    )

    check("net         DHCP ip 10.0.2.15/" in live_log, "the guest did not get a DHCP lease", live_log)
    check(f"udp: bound 0.0.0.0:{local_port}" in live_log, "the fixture did not bind", live_log)
    check(
        f"udp: sent {len(PAYLOAD)} bytes to 10.0.2.2:{server.port}" in live_log,
        "the fixture did not report its send",
        live_log,
    )
    check(
        f"udp: recv {len(PAYLOAD)} bytes from 10.0.2.2:{server.port}" in live_log,
        "the fixture did not report its receive",
        live_log,
    )
    check("udp: ok" in live_log, "the fixture did not print its success marker", live_log)
    check(
        "run: /mnt/udp_echo exited with code 0" in live_log,
        "the fixture did not exit 0",
        live_log,
    )
    server.join(timeout=10)
    check(not server.alive, "the host UDP echo server never finished", live_log)
    check(server.error is None, f"host UDP server error: {server.error}", live_log)
    check(len(server.received) == 1, "the host server did not receive exactly one datagram", live_log)
    check(server.received[0][0] == PAYLOAD, f"host payload was {server.received[0][0]!r}", live_log)

    transcript = (
        "# Live UDP acceptance for pagh-riscv\n"
        f"# kernel: {kernel}\n"
        f"# fixture: {elf} ({elf.stat().st_size} bytes)\n"
        f"# host echo server: 127.0.0.1:{server.port} (guest reachable as 10.0.2.2:{server.port})\n"
        "\n== format boot ==\n"
        f"{format_log}\n"
        "\n== live run ==\n"
        f"{live_log}\n"
        "\n== host UDP server ==\n"
        f"received {len(server.received[0][0])} bytes from "
        f"{server.received[0][1][0]}:{server.received[0][1][1]}: {server.received[0][0]!r}\n"
        f"echoed {len(PAYLOAD)} bytes\n"
        "\nRESULT: PASS — bind/send/recv over QEMU user net, fixture exit 0\n"
    )
    transcript_path = args.transcript
    if not transcript_path.is_absolute():
        transcript_path = ROOT / transcript_path
    transcript_path.write_text(transcript, encoding="utf-8")
    print(f"test_udp: PASS — {transcript_path} ({len(transcript)} bytes)")
    print(
        f"test_udp: host received {server.received[0][0]!r} from "
        f"{server.received[0][1][0]}:{server.received[0][1][1]}, echoed {len(PAYLOAD)} bytes"
    )
    if not args.keep:
        shutil.rmtree(work, ignore_errors=True)
    else:
        print(f"test_udp: workdir {work}")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description="Live UDP acceptance for pagh-riscv")
    parser.add_argument("--elf", type=pathlib.Path, default=DEFAULT_ELF, help=f"udp_echo ELF (default {DEFAULT_ELF})")
    parser.add_argument("--kernel", type=pathlib.Path, default=KERNEL_DEBUG, help=f"kernel ELF (default {KERNEL_DEBUG})")
    parser.add_argument("--release", action="store_true", help=f"use {KERNEL_RELEASE}")
    parser.add_argument("--settle", type=float, default=6.0, help="seconds to drain after typing (default 6)")
    parser.add_argument(
        "--transcript",
        type=pathlib.Path,
        default=ROOT / "docs/live-udp.txt",
        help="where to write the transcript (default docs/live-udp.txt)",
    )
    parser.add_argument("--keep", action="store_true", help="keep the temporary disk/workdir")
    args = parser.parse_args()
    if args.release and args.kernel == KERNEL_DEBUG:
        args.kernel = KERNEL_RELEASE
    try:
        return run(args)
    except Failure as failure:
        print(f"test_udp: FAIL — {failure}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
