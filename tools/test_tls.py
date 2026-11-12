#!/usr/bin/env python3
"""Live HTTPS acceptance for pagh-riscv: real TLS 1.3 + QEMU user net.

This is the evidence harness behind `docs/live-tls.txt`. It drives the kernel
exactly the way a human would (`tools/run.py` + the serial console) and checks
five things, all in one process, so a regression names itself:

1. **entropy** — `entropy` reports a seeded pool (32 bytes from
   `/chosen/rng-seed`). Without it `https` refuses before opening a socket.
2. **the fetch** — `https https://example.com/` completes a TLS 1.3 handshake
   against the real internet, verifies the certificate chain to the committed
   CA bundle, checks the SAN, and prints the 577-byte `Example Domain` body.
3. **the chain gate** — a *local* `openssl s_server` with a self-signed
   certificate for the requested name (`example.com`) must be rejected with
   `cause=chain`. Socket reachable, name right, certificate untrusted: the
   handshake still has to fail.
4. **the SAN gate** — a real server whose chain *does* anchor in the bundle
   (Google Trust Services, pinned as `GTS Root R1`) but whose certificate does
   not cover `example.com` must be rejected with `cause=hostname`. The script
   verifies that precondition from the host before asserting the guest's
   verdict, so a changed internet cannot silently turn this into a pass.
5. **resolution** — a name that does not resolve fails at `stage=resolve`
   rather than connecting somewhere.

Usage::

    cargo build --locked
    python3 tools/test_tls.py                 # needs QEMU + host internet access

The script writes the full transcript to `docs/live-tls.txt` (override with
``--transcript``). CI does not run it: the smoke job must not depend on the
runner's internet access, and check 4 depends on the live internet.
"""

from __future__ import annotations

import argparse
import pathlib
import shutil
import socket
import ssl
import subprocess
import sys
import tempfile
import time

ROOT = pathlib.Path(__file__).resolve().parents[1]
KERNEL_DEBUG = ROOT / "target/riscv64gc-unknown-none-elf/debug/pagh"
KERNEL_RELEASE = ROOT / "target/riscv64gc-unknown-none-elf/release/pagh"
PROOF_HOST = "example.com"
PROOF_MARKER = "Example Domain"
# The libpagh example that drives the same TLS syscalls from userland.
DEFAULT_EXAMPLE = (
    ROOT.parent
    / "pagh-api"
    / "target/riscv64gc-unknown-none-elf/release/examples/https_get"
)
# Host used for the SAN negative: its certificate chain anchors at a pinned
# root (GTS Root R1) while its SANs are Google's, not the requested name.
MISMATCH_HOST = "www.google.com"
# How the guest reaches the host's loopback under QEMU's user network.
HOST_FROM_GUEST = "10.0.2.2"


class Failure(RuntimeError):
    """A check failed; the caller prints the transcript and exits 1."""


def die(message: str, code: int = 2) -> None:
    print(f"test_tls: {message}", file=sys.stderr)
    raise SystemExit(code)


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


class LocalUntrustedServer:
    """`openssl s_server` with a self-signed certificate for `PROOF_HOST`."""

    def __init__(self, work: pathlib.Path) -> None:
        self.work = work
        self.port = free_port()
        self.cert = work / "selfsigned.pem"
        self.key = work / "selfsigned.key"
        self.process: subprocess.Popen | None = None

    def start(self) -> None:
        subprocess.run(
            [
                "openssl", "req", "-x509", "-newkey", "ec",
                "-pkeyopt", "ec_paramgen_curve:prime256v1",
                "-keyout", str(self.key), "-out", str(self.cert),
                "-days", "2", "-nodes",
                "-subj", f"/CN={PROOF_HOST}",
                "-addext", f"subjectAltName=DNS:{PROOF_HOST}",
            ],
            check=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        self.process = subprocess.Popen(
            [
                "openssl", "s_server",
                "-accept", f"127.0.0.1:{self.port}",
                "-cert", str(self.cert), "-key", str(self.key),
                "-tls1_3", "-www", "-quiet",
            ],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        # The listen socket is the readiness condition; a fixed sleep would be
        # a race on a loaded machine.
        deadline = time.time() + 10
        while time.time() < deadline:
            try:
                with socket.create_connection(("127.0.0.1", self.port), timeout=0.5):
                    return
            except OSError:
                time.sleep(0.1)
        raise Failure("the local openssl s_server never accepted a connection")

    def stop(self) -> None:
        if self.process is not None and self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()


def probe_mismatch_host() -> str:
    """Return the IPv4 of `MISMATCH_HOST` after checking the SAN precondition.

    The check opens a real TLS connection with SNI `example.com` and requires
    (a) the served certificate NOT to be valid for `example.com` while being
    otherwise trusted, and (b) the chain to contain Google Trust Services'
    `GTS Root R1` — the root this kernel pins. If the internet changes, this
    fails loudly instead of letting the SAN check pass for the wrong reason.
    """
    address = socket.getaddrinfo(MISMATCH_HOST, 443, socket.AF_INET)[0][4][0]

    # (a) The name must not match, and it must be a *name* failure — not an
    # "untrusted issuer" one, which would make the guest's verdict a chain
    # failure wearing a hostname label.
    trusting = ssl.create_default_context()
    try:
        with socket.create_connection((address, 443), timeout=20) as raw:
            with trusting.wrap_socket(raw, server_hostname=PROOF_HOST):
                raise Failure(
                    f"{MISMATCH_HOST} now presents a certificate valid for {PROOF_HOST}; "
                    "the SAN negative test needs a different host"
                )
    except ssl.SSLCertVerificationError as error:
        if "Hostname mismatch" not in str(error) and "not valid for" not in str(error):
            raise Failure(
                f"{MISMATCH_HOST} failed for the wrong reason: {error}"
            ) from error

    # Fetch the leaf for the transcript, and the chain for the anchor check.
    insecure = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    insecure.check_hostname = False
    insecure.verify_mode = ssl.CERT_NONE
    with socket.create_connection((address, 443), timeout=20) as raw:
        with insecure.wrap_socket(raw, server_hostname=PROOF_HOST) as tls:
            der = tls.getpeercert(binary_form=True)
    with tempfile.NamedTemporaryFile("w", suffix=".pem", delete=False) as handle:
        handle.write(ssl.DER_cert_to_PEM_cert(der))
        path = handle.name
    try:
        subject = subprocess.run(
            ["openssl", "x509", "-in", path, "-noout", "-subject"],
            capture_output=True, text=True, check=True,
        ).stdout.strip()
    finally:
        pathlib.Path(path).unlink(missing_ok=True)

    # (b) The chain must contain the root the kernel pins. `s_client` prints
    # every certificate the server sent, so the check needs no third-party
    # module and no trust in this host's store.
    chain = subprocess.run(
        ["openssl", "s_client", "-connect", f"{address}:443", "-servername", MISMATCH_HOST,
         "-showcerts"],
        input=b"", capture_output=True, timeout=30,
    ).stdout.decode("utf-8", "replace")
    # Both Google roots are pinned in `src/net/ca_bundle.rs`; Google picks R1
    # or R4 per SNI, and either is a chain the kernel trusts.
    anchors = [root for root in ("GTS Root R1", "GTS Root R4") if root in chain]
    if not anchors:
        raise Failure(
            f"{MISMATCH_HOST} no longer sends a chain containing a pinned GTS root; "
            "the SAN negative test would not isolate the hostname check"
        )

    print(
        f"test_tls: SAN-negative precondition ok — {MISMATCH_HOST} ({address}) "
        f"{subject} chain={anchors}"
    )
    return address


def run_qemu(
    kernel: pathlib.Path,
    *,
    keys: str,
    settle: float,
    work: pathlib.Path,
    tag: str,
    timeout: float = 120.0,
    attach: bool = False,
    keep: bool = False,
    tolerate: bool = False,
    disk: pathlib.Path | None = None,
) -> str:
    """Run `tools/run.py` once and return its serial transcript.

    With ``attach`` the tool talks to an instance booted earlier with
    ``keep``; the transcript then covers only this attach's output, so the
    caller concatenates the pieces.
    """
    serial_log = work / f"{tag}.log"
    command = [
        sys.executable, str(ROOT / "tools" / "run.py"),
        "--kernel", str(kernel),
        "--net",
        "--shot", str(work / f"{tag}.png"),
        "--serial", str(work / "guest.serial.sock"),
        "--monitor", str(work / "guest.monitor.sock"),
        "--serial-log", str(serial_log),
        "--settle", str(settle),
        "--timeout", str(timeout),
    ]
    if disk is not None:
        command += ["--disk", str(disk)]
    if attach:
        command.append("--attach")
    else:
        command.append("--keep" if keep else "--net")
    if keys:
        command += ["--keys", keys]
    print("+", " ".join(command))
    result = subprocess.run(
        command, cwd=ROOT, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
        text=True, timeout=timeout + settle + 120,
    )
    transcript = serial_log.read_text(encoding="utf-8", errors="replace") if serial_log.exists() else ""
    # `tolerate` is for the LAST command only: `halt` powers the machine off
    # (SBI SRST), so QEMU is gone before run.py can reach its monitor socket —
    # a non-zero exit there is the proof the guest halted, not a failure.
    if result.returncode != 0 and not tolerate:
        raise Failure(
            f"tools/run.py failed with status {result.returncode}\n"
            f"--- run.py output ---\n{result.stdout}\n--- serial ---\n{transcript}"
        )
    return transcript


def run_guest(
    kernel: pathlib.Path,
    commands: list[tuple[str, float]],
    work: pathlib.Path,
    timeout: float,
    disk: pathlib.Path | None = None,
    tag: str = "boot",
) -> str:
    """Boot once, then type one command per attach.

    WHY NOT ONE `--keys` STRING: the guest's console is a 16-byte UART FIFO and
    the shell is busy while a command runs, so typing the whole session at once
    overruns it (the host socket write then blocks and `run.py` times out). One
    command per attach keeps the guest idle and reading between commands, which
    is also how a human drives it.
    """
    transcript = run_qemu(
        kernel, keys="", settle=1.0, work=work, tag=tag, timeout=timeout, keep=True,
        disk=disk,
    )
    for index, (command, settle) in enumerate(commands):
        transcript += run_qemu(
            kernel, keys=command + "\r", settle=settle, work=work,
            tag=f"{tag}-cmd{index}", timeout=timeout, attach=True,
            tolerate=command == "halt", disk=disk,
        )
    return transcript


def make_image(work: pathlib.Path, elf: pathlib.Path) -> pathlib.Path:
    """Format a 64 MiB ext2 image with `mke2fs -d`, the example inside.

    No first boot is needed to format: the image already has a filesystem and
    the kernel's format policy mounts an existing ext2 rather than erasing it.
    """
    mke2fs = shutil.which("mke2fs")
    if mke2fs is None:
        die("mke2fs (e2fsprogs) is required to build the example image")
    root = work / "root"
    root.mkdir(exist_ok=True)
    target = root / "https_get"
    shutil.copyfile(elf, target)
    target.chmod(0o755)
    disk = work / "disk.img"
    with disk.open("wb") as file:
        file.truncate(64 * 1024 * 1024)
    command = [
        mke2fs, "-q", "-t", "ext2", "-b", "1024", "-F", "-d", str(root), str(disk),
    ]
    print("+", " ".join(command))
    result = subprocess.run(command, capture_output=True, text=True)
    if result.returncode != 0:
        die(f"mke2fs failed: {result.stderr.strip()}\n{result.stdout.strip()}")
    return disk


def shut_down(work: pathlib.Path) -> None:
    """Ask a still-running guest to quit through its monitor socket."""
    monitor = work / "guest.monitor.sock"
    if not monitor.exists():
        return
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
            sock.settimeout(2)
            sock.connect(str(monitor))
            sock.sendall(b"quit\n")
            time.sleep(0.5)
    except OSError:
        pass


def check(condition: bool, what: str, transcript: str) -> None:
    if not condition:
        raise Failure(f"{what}\n--- serial transcript ---\n{transcript}")


def check_no_stack_overflow(transcript: str, label: str) -> None:
    """The kernel must not report a task stack overflow for a TLS path.

    The handshake and the chain verifier run on the caller's kernel stack, so
    this is the deepest path in the kernel. The scheduler only reports the
    damage when the thread's stack is reclaimed: `[task] STACK OVERFLOW:
    guard/canary at … was overwritten`. A release build can complete the
    handshake while its stack has already written into the guard frame, so a
    PASS verdict without this check is not enough (0.21.2 sizing: 24 KiB
    release / 96 KiB debug, measured peaks ~17 / ~70 KiB).
    """
    check(
        "[task] STACK OVERFLOW" not in transcript,
        f"{label}: the kernel reported a task stack overflow — the TLS path "
        "needs a bigger kernel stack than STACK_SIZE (guard/canary was "
        "overwritten; see docs/tls.md 'Стек: самый глубокий путь ядра')",
        transcript,
    )


def section(transcript: str, command: str) -> str:
    """The part of a transcript from `command` to the next prompt."""
    index = transcript.find(command)
    if index < 0:
        return ""
    return transcript[index:]


def run(args: argparse.Namespace) -> int:
    kernel = pathlib.Path(args.kernel).resolve()
    if not kernel.is_file():
        die(f"kernel {kernel} not found — run `cargo build --locked`")
    if kernel.stat().st_size == 0:
        die(f"kernel {kernel} is empty")

    work = pathlib.Path(tempfile.mkdtemp(prefix="pagh-tls-live-"))
    mismatch_ip = probe_mismatch_host()
    server = LocalUntrustedServer(work)

    # Each command runs in its own attach; see `run_guest` for why.
    commands = [
        ("entropy", 4.0),
        (f"https https://{PROOF_HOST}/", args.settle),
        (f"https --ip {HOST_FROM_GUEST} https://{PROOF_HOST}:{server.port}/", 20.0),
        (f"https --ip {mismatch_ip} https://{PROOF_HOST}/", args.settle),
        ("https https://no-such-host.invalid/", 20.0),
        ("halt", 5.0),
    ]

    server.start()
    try:
        transcript = run_guest(kernel, commands, work, args.timeout)
    finally:
        server.stop()
        shut_down(work)

    check_no_stack_overflow(transcript, "shell https")

    # ── 1. entropy ────────────────────────────────────────────────────────
    check(
        "entropy: pool available - seed 32 bytes from /chosen/rng-seed" in transcript,
        "the entropy pool is not seeded (TLS would refuse, or is drawing from nothing)",
        transcript,
    )
    check(
        "entropy     32 bytes from /chosen/rng-seed" in transcript,
        "the boot log does not report the absorbed seed",
        transcript,
    )

    # ── 2. the verified fetch ─────────────────────────────────────────────
    fetch = section(transcript, f"https https://{PROOF_HOST}/")
    check(f"tls: fetch host={PROOF_HOST} port=443 path=/" in fetch, "no fetch line", transcript)
    check("tls: tcp connected to " in fetch, "the TCP connection did not come up", transcript)
    check("tls: verified leaf=sha256:" in fetch, "no certificate was verified", transcript)
    check("chain=" in fetch and "anchors=" in fetch, "the chain/anchor counts are missing", transcript)
    check(
        "tls: CertificateVerify OK" in fetch,
        "the server did not prove possession of the leaf key",
        transcript,
    )
    check("tls: handshake OK" in fetch, "the handshake did not complete", transcript)
    check("tls: fetch OK body=" in fetch, "the fetch did not report a body", transcript)
    check(f"https: OK - " in fetch, "the shell command did not report success", transcript)
    check(PROOF_MARKER in fetch, f"the response body does not contain {PROOF_MARKER!r}", transcript)

    # ── 3. the chain gate ─────────────────────────────────────────────────
    chain = section(transcript, f"https --ip {HOST_FROM_GUEST} ")
    check("tls: tcp connected to 10.0.2.2:" in chain, "the local server was not reached", transcript)
    check(
        "tls: verify FAILED cause=chain" in chain,
        "an untrusted self-signed certificate for the right name was NOT rejected",
        transcript,
    )
    check(
        "https: FAILED: certificate rejected (chain)" in chain,
        "the shell did not name the chain as the failing gate",
        transcript,
    )

    # ── 4. the SAN gate ───────────────────────────────────────────────────
    san = section(transcript, f"https --ip {mismatch_ip} ")
    check("tls: tcp connected to " in san, "the mismatch server was not reached", transcript)
    check(
        "tls: verify FAILED cause=hostname" in san,
        "a certificate that does not cover the requested host was NOT rejected",
        transcript,
    )
    check(
        "tls: verify FAILED cause=chain" not in san,
        f"the SAN test host ({mismatch_ip}) no longer chains to a pinned anchor — "
        "the verdict is not about the SAN check",
        transcript,
    )

    # ── 5. resolution ─────────────────────────────────────────────────────
    resolve = section(transcript, "https https://no-such-host.invalid/")
    check("tls: FAILED stage=resolve" in resolve, "an unresolvable name did not fail at resolve", transcript)
    check("tls: tcp connected" not in resolve, "a socket was opened for a name that cannot resolve", transcript)

    # ── 6. the same handshake from userland (libpagh, no shell help) ──────
    #
    # This is the phase that proves the TLS syscalls are *reachable*: a normal
    # program resolves the name itself, connects a socket, hands it to the
    # kernel for the handshake and reads the body through the descriptor. The
    # certificate decision is the kernel's — the transcript has to show the
    # verifier's own line for the userland session, not just the shell's.
    userland = ""
    if not args.no_userland:
        elf = pathlib.Path(args.example).resolve()
        if not elf.is_file():
            die(
                f"{elf} not found — build it with\n"
                "  (cd ../pagh-api && cargo build --release --example https_get \\"
                "\n      --target riscv64gc-unknown-none-elf)"
            )
        disk = make_image(work, elf)
        userland = run_guest(
            kernel,
            [(f"run /mnt/https_get {PROOF_HOST} /", args.settle), ("halt", 5.0)],
            work,
            args.timeout,
            disk=disk,
            tag="user",
        )
        check_no_stack_overflow(userland, "userland https_get")
        transcript += userland

    if not args.no_userland:
        guest = section(userland, "run /mnt/https_get")
        check(
            "tls: verified leaf=sha256:" in guest,
            "the userland handshake did not verify a certificate",
            userland,
        )
        check(
            "tls: user session 0 open tcp=" in guest,
            "the kernel did not publish a userland TLS session",
            userland,
        )
        check(
            "https_get: TLS 1.3 session up (server authenticated by the kernel)" in guest,
            "the example did not report an authenticated session",
            userland,
        )
        check("https_get: OK" in guest, "the example did not finish", userland)
        check(PROOF_MARKER in guest, f"the userland body does not contain {PROOF_MARKER!r}", userland)
        check(
            "run: /mnt/https_get exited with code 0" in guest,
            "the example did not exit 0",
            userland,
        )

    body = (
        "# Live HTTPS (TLS 1.3) acceptance for pagh-riscv\n"
        f"# kernel: {kernel} ({kernel.stat().st_size} bytes)\n"
        f"# proof host: https://{PROOF_HOST}/ (chain -> SSL.com TLS ECC Root CA 2022)\n"
        f"# chain-negative: local openssl s_server on 127.0.0.1:{server.port} "
        f"(guest 10.0.2.2:{server.port}), self-signed cert for {PROOF_HOST}\n"
        f"# san-negative: {MISMATCH_HOST} ({mismatch_ip}) with SNI {PROOF_HOST}: "
        "chain anchors at GTS Root R1, SAN does not cover the name\n"
        "\n== guest transcript ==\n"
        f"{transcript}\n"
        "RESULT: PASS — verified HTTPS fetch, untrusted chain refused, "
        "mismatched SAN refused, unresolvable name refused"
        + (
            ", and the libpagh https_get example performed the same verified\n"
            "fetch from userland through the TLS syscalls\n"
            if not args.no_userland
            else "\n"
        )
    )
    text = "\n".join(line.rstrip() for line in body.splitlines()) + "\n"
    path = pathlib.Path(args.transcript)
    if not path.is_absolute():
        path = ROOT / path
    path.write_text(text, encoding="utf-8")
    print(f"test_tls: PASS — {path} ({len(text)} bytes)")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--kernel", default=str(KERNEL_DEBUG), help="kernel ELF to boot")
    parser.add_argument("--release", action="store_true", help=f"use {KERNEL_RELEASE}")
    parser.add_argument("--settle", type=float, default=90.0, help="seconds to let the guest run the commands")
    parser.add_argument("--timeout", type=float, default=120.0, help="seconds to wait for the shell prompt")
    parser.add_argument("--transcript", default="docs/live-tls.txt", help="where to write the evidence transcript")
    parser.add_argument(
        "--example",
        default=str(DEFAULT_EXAMPLE),
        help="libpagh https_get ELF for the userland phase",
    )
    parser.add_argument(
        "--no-userland",
        action="store_true",
        help="skip the libpagh example phase (no disk image is built)",
    )
    args = parser.parse_args()
    if args.release and args.kernel == str(KERNEL_DEBUG):
        args.kernel = str(KERNEL_RELEASE)
    try:
        return run(args)
    except Failure as failure:
        print(f"test_tls: FAIL\n{failure}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
