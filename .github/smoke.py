#!/usr/bin/env python3
"""Headless QEMU smoke test for pagh-riscv.

Boots the kernel on QEMU's `virt` machine with the OpenSBI firmware QEMU ships,
waits for the boot banner, drives the shell over the serial line, and fails if
any expected line is missing.  Used by the `smoke` job in
`.github/workflows/ci.yml`; runnable locally with the same arguments:

    cargo build
    python3 .github/smoke.py target/riscv64gc-unknown-none-elf/debug/pagh             # serial only
    python3 .github/smoke.py target/riscv64gc-unknown-none-elf/debug/pagh --ramfb     # + screen

`--ramfb` attaches QEMU's ramfb device, so the run also covers the framebuffer
bring-up (`fw_cfg` -> `etc/ramfb` -> the console's second sink) while staying
headless: the screen is written, never displayed.  Without the device the same
kernel must stay a serial console, and the driver asserts that fallback message
instead.

Three details make the difference between a test that proves something and one
that passes by accident:

* **Input is sent only after the banner appears.**  `uart::init()` writes
  `FCR = 0x07` (clear both FIFOs), so bytes that reached the UART model before
  the kernel configured it are discarded: piping `help\\nhalt\\n` into QEMU at
  start-up produces a prompt that never answers.  The driver waits for
  ``Type 'help' for the command list.`` first.
* **`halt` is verified by the process exit code, not by a line.**  `halt` calls
  the SBI System Reset extension (`src/arch/riscv64/sbi.rs`), so QEMU powers off
  and exits 0; a machine that only parked its hart would stay alive until killed
  and is treated as a failure here.
* **Markers are checked in order.**  A string that appears earlier than it
  should (a stale banner, output from the wrong phase) is a failure, not a pass.

Exit status: 0 only when every marker was seen in order *and* QEMU exited 0.
The full serial log is printed either way, so a CI failure is readable.
"""

import os
import select
import subprocess
import sys
import time

# Markers that must appear in the serial log, in this order.  Every one of them
# is asserted by the workflow, so keep them stable strings the kernel really
# prints (see src/main.rs, src/shell.rs).
EXPECTED = [
    "   pagh OS — RISC-V (RV64)",
    "boot hart   0",
    "device tree 0x",
    "memory      0x80000000..0x90000000 (256 MiB)",
    "timebase    10000000 Hz",
    "console     NS16550 @ 0x10000000",
    # The frame pool line is asserted by prefix, not verbatim: the counts move with
    # `-m` and with whatever the kernel reserves, but the line must be there.
    "pmm         ",
    " frames free (",
    "Type 'help' for the command list.",
    "pagh:rv64> mem",
    "memory 0x80000000..0x90000000 (256 MiB)",
    "pagh:rv64> hart",
    "hart 0",
    "pagh:rv64> time",
    "ticks (~",
    "pagh:rv64> echo smoke ok",
    "smoke ok",
    "pagh:rv64> nosuchcmd",
    "nosuchcmd: unknown command (try 'help')",
    "pagh:rv64> help",
    "commands:",
    "  help          this list",
    "  mem           physical memory reported by the device tree",
    "  time          the `time` CSR (timebase ticks) and its frequency",
    "  hart          the hart this kernel is running on",
    "  echo <text>   print the text back",
    "  clear         clear the screen (ANSI)",
    "  halt          power the machine off (SBI SRST)",
    "pagh:rv64> halt",
    "halt: powering off",
]

# What the console prints about its screen, per mode.  The prefix of the ramfb
# line is asserted rather than the whole geometry: the size is the driver's
# business, that it came up is this test's.
SCREEN_MARKER = {
    False: "framebuffer  ramfb: no such fw_cfg item — serial console only",
    True: "framebuffer  ramfb ",
}

# Sent as one write, after the banner: the shell is line-based and its UART has
# a FIFO, so a burst is fine once the kernel is listening.
COMMANDS = b"mem\nhart\ntime\necho smoke ok\nnosuchcmd\nhelp\nhalt\n"

BANNER = "Type 'help' for the command list."
HALT = "halt: powering off"
QEMU = os.environ.get("QEMU", "qemu-system-riscv64")


def boot(kernel, ramfb=False, timeout=120.0):
    """Boot `kernel`, drive the shell, return (reached_halt, powered_off, exit_code, log)."""
    cmd = [
        QEMU, "-machine", "virt", "-m", "256M", "-kernel", kernel,
        "-display", "none", "-monitor", "none", "-serial", "stdio", "-no-reboot",
    ]
    if ramfb:
        cmd += ["-device", "ramfb"]
    proc = subprocess.Popen(
        cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT, bufsize=0,
    )
    log = bytearray()
    fd = proc.stdout.fileno()
    os.set_blocking(fd, False)
    deadline = time.time() + timeout

    def pump_until(marker):
        """Read the serial line until `marker` shows up or the deadline passes."""
        while time.time() < deadline:
            ready, _, _ = select.select([fd], [], [], 0.2)
            if ready:
                chunk = os.read(fd, 4096)
                if not chunk:
                    break  # QEMU closed the pipe
                log.extend(chunk)
                if marker in log.decode("utf-8", "replace"):
                    return True
            elif proc.poll() is not None:
                break
        return False

    reached = pump_until(BANNER)
    if reached:
        proc.stdin.write(COMMANDS)
        proc.stdin.flush()
        reached = pump_until(HALT)

    # `halt` must power the machine off; give QEMU a moment to exit on its own.
    exit_code = None
    for _ in range(50):
        exit_code = proc.poll()
        if exit_code is not None:
            break
        time.sleep(0.2)
    if exit_code is None:
        proc.kill()
        exit_code = proc.wait()
        powered_off = False
    else:
        powered_off = exit_code == 0
    # Drain whatever the guest wrote on its way out.
    while True:
        ready, _, _ = select.select([fd], [], [], 0)
        if not ready:
            break
        chunk = os.read(fd, 4096)
        if not chunk:
            break
        log.extend(chunk)

    return reached, powered_off, exit_code, log.decode("utf-8", "replace")


def main():
    args = sys.argv[1:]
    ramfb = "--ramfb" in args
    args = [a for a in args if a != "--ramfb"]
    if len(args) != 1:
        print("usage: smoke.py <path-to-kernel-elf> [--ramfb]", file=sys.stderr)
        return 2

    kernel = args[0]
    if not os.path.exists(kernel):
        print("SMOKE: no kernel at %s (run `cargo build` first)" % kernel, file=sys.stderr)
        return 2

    reached, powered_off, exit_code, log = boot(kernel, ramfb=ramfb)
    print(log, end="" if log.endswith("\n") else "\n")

    expected = [SCREEN_MARKER[ramfb]] + EXPECTED
    failures = []
    if not reached:
        failures.append("the guest never reached the halt marker (%r)" % HALT)
    if not powered_off:
        failures.append("`halt` did not power the machine off (QEMU exit code %s)" % exit_code)

    missing, pos = [], 0
    for want in expected:
        found = log.find(want, pos)
        if found < 0:
            missing.append(want)
        else:
            pos = found + len(want)
    for want in missing:
        failures.append("missing expected line: %r" % want)

    if failures:
        print("SMOKE: FAIL (%s)" % ("ramfb" if ramfb else "serial"), file=sys.stderr)
        for why in failures:
            print("  - %s" % why, file=sys.stderr)
        return 1
    print(
        "SMOKE: PASS (%s) — %d markers in order, QEMU exited 0 after halt"
        % ("ramfb" if ramfb else "serial", len(expected))
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())