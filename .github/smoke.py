#!/usr/bin/env python3
"""Headless QEMU smoke test for pagh-riscv.

Boots the kernel on QEMU's `virt` machine with the OpenSBI firmware QEMU ships,
waits for the boot banner, drives the shell over the serial line, and fails if
any expected line is missing.  Used by the `smoke` job in
`.github/workflows/ci.yml`; runnable locally with the same arguments:

    cargo build
    python3 .github/smoke.py target/riscv64gc-unknown-none-elf/debug/pagh             # serial only
    python3 .github/smoke.py target/riscv64gc-unknown-none-elf/debug/pagh --ramfb     # + screen
    # M5: format a blank image, then (second run, same image) mount it and read back
    qemu-img create -f raw /tmp/ci.img 64M
    python3 .github/smoke.py <elf> --disk /tmp/ci.img --fresh
    python3 .github/smoke.py <elf> --disk /tmp/ci.img
    python3 .github/smoke.py <elf> --net        # virtio-net: DHCP + ping gateway
    python3 .github/smoke.py <elf> --keyboard   # virtio-keyboard: type help/halt via QMP

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
import socket
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
    # The pointer device (virtio-tablet) is attached by every smoke mode; the
    # line proves the input driver found it before the shell starts.
    "virtio-input: tablet at 0x",
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
    # `run` with arguments: the fixtures keep argc=1 by contract, so trailing
    # words must be accepted and ignored rather than changing the entry frame.
    "pagh:rv64> run demo alpha beta",
    "hello from user mode",
    "run: demo exited with code 42",
    # `/proc` is mounted by `vfs::init`, so it exists on this serial-only boot
    # too (no disk, no NIC). The shell reads it with the ordinary file commands;
    # the listing is asserted as one block so order and completeness are checked,
    # not just the presence of a name. No user process can be live while the
    # shell runs (a foreground `run` owns the console), so no numeric `/proc`
    # entries appear here; the `run proc` fixture below checks that namespace
    # from inside a process.
    "pagh:rv64> cat /proc/version",
    "pagh 0.24.2 (riscv64) #1",
    "pagh:rv64> ls /proc",
    "cpuinfo\nkthreads/\nmeminfo\nmounts\nnet/\nself\ntasks\nuptime\nversion\n",
    "pagh:rv64> cat /proc/meminfo",
    "MemTotal:",
    "MemFree:",
    "pagh:rv64> cat /proc/tasks",
    # The live-count line is asserted by prefix: `live_tasks` counts slots that
    # held a thread, exited ones included, and the `run demo` above leaves one.
    "tasks: ",
    "  id  name       state",
    "  0   shell",
    "pagh:rv64> stat /proc/meminfo",
    "type:  regular file",
    "mode:  0644",
    # The per-pid half, seen from the shell: `/proc/self` is a symlink into the
    # kernel-thread namespace (the shell is not a process), `/proc/<pid>` is a
    # process id and a pid that never existed is -ENOENT.
    "pagh:rv64> cat /proc/self/status",
    "Name:\tshell",
    "State:\tR (running)",
    "Tid:\t0",
    "Pid:\t0",
    "Threads:\t1",
    "Stack:\tboot",
    "pagh:rv64> cat /proc/self/stat",
    "0 (shell) R 0 1 ",
    "pagh:rv64> stat /proc/self",
    "type:  symbolic link",
    "target: kthreads/0",
    "pagh:rv64> ls /proc/kthreads",
    "0/\n1/\n",
    "pagh:rv64> cat /proc/0/status",
    "cat: /proc/0/status: not found",
    # The userland half of the same contract: a static program reads /proc
    # through openat/read/getdents64 — including `/proc/self`, a live peer by
    # pid, and a reaped child that must be -ENOENT again — and prints the
    # markers itself.
    "pagh:rv64> run proc",
    "proc: self ok",
    "proc: fork ok",
    "proc: done",
    "run: proc exited with code 0",
    # `statfs(43)` on a boot with no disk: no filesystem to describe, and the
    # fixture says so instead of inventing capacity.
    "pagh:rv64> run statfs",
    "statfs: no /mnt",
    "run: statfs exited with code 0",
    "pagh:rv64> help",
    "commands:",
    "  help          this list",
    "  mem           device tree range, frame pool, heap and the page table",
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
COMMANDS = (b"mem\nhart\ntime\necho smoke ok\nnosuchcmd\n"
            b"run demo alpha beta\n"
            b"cat /proc/version\nls /proc\ncat /proc/meminfo\ncat /proc/tasks\n"
            b"stat /proc/meminfo\ncat /proc/self/status\ncat /proc/self/stat\n"
            b"stat /proc/self\nls /proc/kthreads\ncat /proc/0/status\nrun proc\n"
            b"run statfs\nhelp\nhalt\n")

# ── the disk mode (M5) ─────────────────────────────────────────────────────
# `--disk PATH` attaches a virtio-mmio block device and then behaves differently
# on purpose: a fresh image must be formatted by the kernel, an existing one must
# mount without touching it. The two runs together prove format-if-blank,
# mount, write-through and persistence across a reboot — the M5 acceptance
# criteria — while the no-disk mode above keeps proving the machine works
# without any block device at all.
DISK_BOOT_PREFIX = [
    "virtio-blk: virtio-blk0 at 0x",
    "fs          virtio-blk0 (",
]
DISK_FRESH_BOOT = ["fs          virtio-blk0 is blank — formatting ext2"]
DISK_MOUNTED = ["fs          ext2 on /mnt,"]
DISK_COMMANDS_FRESH = (
    b"disk\ncat /proc/mounts\nls /mnt\nmkdir /mnt/ci\n"
    b"write /mnt/ci/marker.txt hello-from-ci\n"
    b"cat /mnt/ci/marker.txt\nstat /mnt/ci/marker.txt\n"
    b"run statfs\nselftest\nhalt\n"
)
DISK_COMMANDS_EXISTING = b"disk\ncat /mnt/ci/marker.txt\nrun statfs\nselftest\nhalt\n"
DISK_EXPECTED_FRESH = [
    "pagh:rv64> disk",
    "disk: virtio-blk0",
    "sector 0 readable",
    # The mount table as `/proc/mounts` reports it: the synthetic root's two
    # mounts, the built-in procfs and the ext2 disk that `init_fs` just mounted.
    "pagh:rv64> cat /proc/mounts",
    "/proc /proc procfs rw 0 0",
    "/mnt /mnt ext2 rw 0 0",
    "pagh:rv64> ls /mnt",
    "lost+found/",
    "write: wrote",
    "hello-from-ci",
    "size:",
    # The native statfs/fstatfs pair, driven from userland: the ext2 record with
    # real block/inode counts, a procfs record with zero capacity, then the
    # selftest's `user-statfs` check.
    "pagh:rv64> run statfs",
    "statfs: /mnt ext2 ",
    "statfs: free-space ok",
    "statfs: fstatfs ok",
    "statfs: procfs ok",
    "statfs: errors ok",
    "statfs: done",
    "run: statfs exited with code 0",
    "SELFTEST SUMMARY: 7/7 passed",
    "SELFTEST ext2 summary: 11/11 passed",
    # The entropy + TLS surface, exercised in-guest by `userland/tls.elf`:
    # `getrandom(278)` and the fail-closed answer of every TLS syscall
    # (1100-1103) on a descriptor that is not a session, with the PMM/heap
    # ledger unchanged -- no server, no extra boot mode. The net suite's
    # session-table gate is the kernel-side view of the same table.
    "SELFTEST user-tls-abi PASS",
    "SELFTEST user-proc-fs PASS",
    "SELFTEST user-statfs PASS",
    "SELFTEST SUMMARY (user): 16/16 passed",
    "SELFTEST net-tls-session-gate PASS",
    "SELFTEST SUMMARY (net): 12/12 passed",
    "halt: powering off",
]
# ── the network mode (M7) ─────────────────────────────────────────────────
# The DHCP client runs during boot, before the shell; the commands then exercise
# the lease (`net`), the ARP/ICMP path (`ping` the gateway) and the shell itself.
NET_COMMANDS = b"net\nping 10.0.2.2\ncat /proc/net/arp\nhalt\n"
NET_BOOT = [
    "virtio-net: virtio-net0 at 0x",
    "net         DHCP ip 10.0.2.15/",
]
NET_EXPECTED = NET_BOOT + [
    "Type 'help' for the command list.",
    "pagh:rv64> net",
    "net: virtio-net0 MAC",
    "ip 10.0.2.15 mask 255.255.255.0 gateway 10.0.2.2",
    "pagh:rv64> ping 10.0.2.2",
    "ping: reply from 10.0.2.2",
    # The ARP cache is live state the stack has always kept and no command
    # showed. QEMU's user-mode network gives the gateway a fixed MAC, and the
    # ping above is what put the entry there.
    "pagh:rv64> cat /proc/net/arp",
    "IP address       HW address        Device\n10.0.2.2         52:55:0a:00:02:02 virtio-net0\n",
    "halt: powering off",
]

DISK_EXPECTED_EXISTING = [
    "pagh:rv64> cat /mnt/ci/marker.txt",
    "hello-from-ci",
    "pagh:rv64> run statfs",
    "statfs: /mnt ext2 ",
    "statfs: done",
    "run: statfs exited with code 0",
    "SELFTEST ext2 summary: 11/11 passed",
    "SELFTEST user-tls-abi PASS",
    "SELFTEST user-proc-fs PASS",
    "SELFTEST user-statfs PASS",
    "SELFTEST SUMMARY (user): 16/16 passed",
    "halt: powering off",
]

BANNER = "Type 'help' for the command list."
HALT = "halt: powering off"
# One write to the serial chardev must stay well under the guest's 256-byte
# receive ring (`RING_CAPACITY` in `src/drivers/uart.rs`); see `boot()`.
COMMAND_CHUNK = 96
COMMAND_CHUNK_PAUSE = 0.25
QEMU = os.environ.get("QEMU", "qemu-system-riscv64")
# Firmware: QEMU's `-bios default` looks for the OpenSBI image it was built
# against, which some distributions install somewhere else. PAGH_BIOS names the
# file explicitly when it is; CI sets it when it finds one.
BIOS = os.environ.get("PAGH_BIOS", "").strip()


def boot(kernel, ramfb=False, disk=None, commands=COMMANDS, timeout=120.0, net=False):
    """Boot `kernel`, drive the shell, return (reached_halt, powered_off, exit_code, log)."""
    cmd = [
        QEMU, "-machine", "virt", "-m", "256M",
        "-bios", BIOS if BIOS else "default",
        "-kernel", kernel,
        "-display", "none", "-monitor", "none", "-serial", "stdio", "-no-reboot",
        # The kernel speaks modern virtio only; the pointer is on every mode.
        "-global", "virtio-mmio.force-legacy=false",
        "-device", "virtio-tablet-device",
    ]
    if ramfb:
        cmd += ["-device", "ramfb"]
    if disk:
        # QEMU's virtio-mmio transports default to the legacy (version 1) layout;
        # this kernel speaks modern virtio only, so the transport is pinned here.
        cmd += [
            "-drive", "file=%s,format=raw,if=none,id=hd0" % disk,
            "-device", "virtio-blk-device,drive=hd0",
        ]
    if net:
        # QEMU's user-mode network: the guest gets 10.0.2.15/24 over DHCP,
        # 10.0.2.2 is the gateway (and the host's loopback, so a host listener
        # can be reached) and 10.0.2.3 is the built-in DNS forwarder.
        cmd += [
            "-netdev", "user,id=net0",
            "-device", "virtio-net-device,netdev=net0",
        ]
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
        # The guest's UART receive ring holds 256 bytes and the shell does not
        # drain it while a command runs, so one large write can overflow the
        # ring and drop the tail of the script (that is how `halt` once arrived
        # as `ha`). Send it in chunks with a pause: worst case is
        # `2 * COMMAND_CHUNK` bytes in flight, comfortably under the ring.
        for start in range(0, len(commands), COMMAND_CHUNK):
            proc.stdin.write(commands[start:start + COMMAND_CHUNK])
            proc.stdin.flush()
            time.sleep(COMMAND_CHUNK_PAUSE)
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


def keyboard_boot(kernel, timeout=120.0):
    """Boot with a virtio-keyboard and type `help` / `halt` through the monitor.

    This is the QEMU-window input path in a headless test: the guest's keyboard
    is a virtio-mmio input device (there is no PS/2 on `virt`), so the keys are
    delivered by QEMU's input layer rather than by the serial line. The monitor
    is the only way to inject them, which is why this mode starts its own QEMU
    instead of reusing [`boot`].

    The serial line is a unix socket rather than stdio on purpose: with
    `-serial stdio` the guest's output stopped mid-command when the monitor was
    injecting keys, while the same guest on a socket stays responsive. The
    socket is also what the other drivers in this repository use.

    Returns `(ok, log)`.
    """
    tag = os.getpid()
    mon = "/tmp/pagh-smoke-keyboard-%d.mon" % tag
    ser = "/tmp/pagh-smoke-keyboard-%d.ser" % tag
    for path in (mon, ser):
        if os.path.exists(path):
            os.unlink(path)
    cmd = [
        QEMU, "-machine", "virt", "-m", "256M",
        "-bios", BIOS if BIOS else "default",
        "-kernel", kernel,
        "-display", "none", "-no-reboot",
        "-global", "virtio-mmio.force-legacy=false",
        "-device", "ramfb",
        "-device", "virtio-tablet-device",
        "-device", "virtio-keyboard-device",
        "-serial", "unix:%s,server=on,wait=on" % ser,
        "-monitor", "unix:%s,server=on,wait=off" % mon,
    ]
    proc = subprocess.Popen(
        cmd, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    log = bytearray()

    def connect(path, deadline):
        while time.time() < deadline:
            sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            try:
                sock.connect(path)
                return sock
            except OSError:
                sock.close()
                time.sleep(0.1)
        return None

    serial = None
    try:
        deadline = time.time() + timeout
        serial = connect(ser, deadline)
        if serial is None:
            return False, ""
        serial.setblocking(False)
        fd = serial.fileno()

        def pump_until(marker, limit=None):
            end = time.time() + (limit if limit is not None else timeout)
            while time.time() < end:
                ready, _, _ = select.select([fd], [], [], 0.2)
                if ready:
                    try:
                        chunk = serial.recv(4096)
                    except BlockingIOError:
                        continue
                    if not chunk:
                        return False
                    log.extend(chunk)
                    if marker in log.decode("utf-8", "replace"):
                        return True
                elif proc.poll() is not None:
                    return False
            return False

        if not pump_until(BANNER):
            return False, log.decode("utf-8", "replace")
        time.sleep(0.5)

        monitor = connect(mon, time.time() + 15)
        if monitor is None:
            return False, log.decode("utf-8", "replace")
        monitor.setblocking(False)

        def hmp(command):
            monitor.sendall((command + "\n").encode())
            time.sleep(0.25)
            try:
                monitor.recv(65536)
            except BlockingIOError:
                pass

        def typed(text):
            for ch in text:
                hmp("sendkey " + ("spc" if ch == " " else ch))
            hmp("sendkey ret")

        typed("help")
        if not pump_until("commands:", 20):
            return False, log.decode("utf-8", "replace")
        typed("halt")
        end = time.time() + timeout
        while time.time() < end and proc.poll() is None:
            ready, _, _ = select.select([fd], [], [], 0.2)
            if ready:
                try:
                    chunk = serial.recv(65536)
                except BlockingIOError:
                    continue
                if chunk:
                    log.extend(chunk)
        exit_code = proc.poll()
        if exit_code is None:
            proc.kill()
            exit_code = proc.wait()
        text = log.decode("utf-8", "replace")
        ok = (
            "virtio-input: keyboard at 0x" in text
            and "commands:" in text
            and HALT in text
            and exit_code == 0
        )
        return ok, text
    finally:
        if proc.poll() is None:
            proc.kill()
            proc.wait()
        if serial is not None:
            serial.close()
        for path in (mon, ser):
            if os.path.exists(path):
                os.unlink(path)


def main():
    args = sys.argv[1:]
    ramfb = "--ramfb" in args
    args = [a for a in args if a != "--ramfb"]
    net = "--net" in args
    args = [a for a in args if a != "--net"]
    keyboard = "--keyboard" in args
    args = [a for a in args if a != "--keyboard"]
    fresh = "--fresh" in args
    args = [a for a in args if a != "--fresh"]
    disk = None
    if "--disk" in args:
        i = args.index("--disk")
        if i + 1 >= len(args):
            print("usage: smoke.py <kernel> [--ramfb] [--net] [--keyboard] [--disk PATH [--fresh]]", file=sys.stderr)
            return 2
        disk = args[i + 1]
        args = args[:i] + args[i + 2 :]
    if len(args) != 1:
        print("usage: smoke.py <kernel> [--ramfb] [--net] [--disk PATH [--fresh]]", file=sys.stderr)
        return 2

    kernel = args[0]
    if not os.path.exists(kernel):
        print("SMOKE: no kernel at %s (run `cargo build` first)" % kernel, file=sys.stderr)
        return 2
    if disk and not os.path.exists(disk):
        print("SMOKE: no disk image at %s (create it first: qemu-img create -f raw ...)" % disk,
              file=sys.stderr)
        return 2

    if keyboard:
        ok, log = keyboard_boot(kernel)
        print(log, end="" if log.endswith("\n") else "\n")
        if not ok:
            print("SMOKE: FAIL (keyboard)", file=sys.stderr)
            return 1
        print("SMOKE: PASS (keyboard) — virtio-keyboard typed help and halt, QEMU exited 0")
        return 0

    mode = "ramfb" if ramfb else "serial"
    if net:
        mode += "+net"
        reached, powered_off, exit_code, log = boot(
            kernel, ramfb=ramfb, net=True, commands=NET_COMMANDS
        )
        expected = list(NET_EXPECTED)
        if ramfb:
            expected.insert(0, SCREEN_MARKER[True])
    elif disk:
        mode += "+disk" + ("+fresh" if fresh else "+existing")
        commands = DISK_COMMANDS_FRESH if fresh else DISK_COMMANDS_EXISTING
        reached, powered_off, exit_code, log = boot(kernel, ramfb=ramfb, disk=disk, commands=commands)
        # Boot markers that come before the banner, so they must be expected
        # before the shared block; the banner and everything after it are shared.
        banner_at = EXPECTED.index(BANNER)
        # The boot order on a disk boot is: ... pmm, virtio-blk probe line,
        # virtio-input line, fs device line, (format line), ext2 mount, banner.
        # The tablet marker lives inside EXPECTED (every mode attaches the
        # pointer), so the disk probe line has to be spliced in before it and the
        # rest of DISK_BOOT_PREFIX after it.
        tablet_at = EXPECTED.index("virtio-input: tablet at 0x")
        expected = (
            [SCREEN_MARKER[ramfb]]
            + EXPECTED[:tablet_at]          # banner .. pmm, before the devices
            + [DISK_BOOT_PREFIX[0]]         # virtio-blk probe
            + [EXPECTED[tablet_at]]         # virtio-input probe
            + [DISK_BOOT_PREFIX[1]]         # fs device line
            + (DISK_FRESH_BOOT if fresh else [])   # format line, when formatting
            + DISK_MOUNTED                  # ext2 mount
            + [EXPECTED[banner_at]]         # "Type 'help' ..."
            + (DISK_EXPECTED_FRESH if fresh else DISK_EXPECTED_EXISTING)
        )
    else:
        reached, powered_off, exit_code, log = boot(kernel, ramfb=ramfb)
        expected = [SCREEN_MARKER[ramfb]] + EXPECTED
    print(log, end="" if log.endswith("\n") else "\n")

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
        print("SMOKE: FAIL (%s)" % mode, file=sys.stderr)
        for why in failures:
            print("  - %s" % why, file=sys.stderr)
        return 1
    print(
        "SMOKE: PASS (%s) — %d markers in order, QEMU exited 0 after halt" % (mode, len(expected))
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())