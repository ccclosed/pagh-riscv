# AGENTS.md — guide for AI agents (and humans) working on pagh-riscv

A small 64-bit RISC-V (RV64) kernel in Rust: `#![no_std]`, **zero dependencies**,
booted by OpenSBI in S-mode on QEMU's `virt` machine, with a polled serial console
and a ramfb screen. Written by AI under human supervision — read this file before
touching the code. It lists the gates that must pass and the invariants that are
easy to break silently.

Human-facing prose lives in Russian (`README.md`, `CONTRIBUTING.md`, `docs/*`); this
file is in English, like its x86 counterpart, because it is written for agents.

## Authoritative documentation

Read in this order; do not rely on memory over these files:

| File | Contents |
|---|---|
| `README.md` | pitch, honest status per subsystem, quickstart, what is *not* implemented |
| `docs/architecture.md` | boot phases, trap frame layout with offsets, privilege model, memory (PMM/heap), console+screen, memory map — every claim cites file:line |
| `docs/roadmap.md` | M1–M6, what "done" means for each, what is explicitly out of scope |
| `docs/screenshot.md` | how a PNG of the screen is taken and how its pixels are verified |
| `src/README.md` | crate-root map and the boot sequence |
| `src/<subsystem>/README.md` | each folder documents itself; update it when the design changes |
| `CONTRIBUTING.md` | contributor guide (RU): build, run, adding a subsystem, house rules |
| `.github/smoke.py` | the executable list of strings a healthy boot must print |

## Commands (all gates must be green before you push)

```sh
cargo fmt --all -- --check                                              # formatting gate
cargo build --locked                                                    # debug kernel
cargo build --locked --release                                          # release kernel
python3 .github/smoke.py target/riscv64gc-unknown-none-elf/debug/pagh            # serial boot + shell
python3 .github/smoke.py target/riscv64gc-unknown-none-elf/debug/pagh --ramfb    # + the screen path
# M5 acceptance (the smoke job runs these too): blank disk -> format+mount+write,
# then boot the same image again -> mount without formatting, read the file back
qemu-img create -f raw /tmp/ci.img 64M
python3 .github/smoke.py target/riscv64gc-unknown-none-elf/debug/pagh --ramfb --disk /tmp/ci.img --fresh
python3 .github/smoke.py target/riscv64gc-unknown-none-elf/debug/pagh --ramfb --disk /tmp/ci.img
python3 .github/smoke.py target/riscv64gc-unknown-none-elf/debug/pagh --ramfb --keyboard  # type help/halt
```

`./run.sh` boots the kernel interactively (QEMU `virt` + OpenSBI, serial on stdio;
`Ctrl-A X` to leave). It always passes `-device ramfb`, so the screen exists too:
in a QEMU window by default when a display is reachable (`DISPLAY` or
`WAYLAND_DISPLAY`), serial-only with `--headless` (the shell renders off-screen,
which is what lets `screendump` work headless). `PROFILE=release`, `MEMORY=…`,
`BIOS=…` cover the variants, `DISK=/tmp/pagh.img` attaches the virtio-mmio
disk (a missing image is created blank and the kernel formats it on boot) and
`NO_MOUSE=1` boots without the virtio-tablet (the pointer-absent path); what
`run.sh` does not take as an argument it takes from the environment, and an
unknown argument is rejected, not ignored. Checks that are not part of CI:

```sh
# at the `pagh:rv64>` prompt
selftest                                        # in-QEMU suites: 7 mem + 4 block/pointer + 11 fs + 4 arch/timer + 5 task + 10 user (10/10 with /mnt, 8/10 + 2 skip without) + 11 net + ledgers
# from the host
python3 tools/run.py --keys "help\n" --shot /tmp/shot.png --check-font --scan-all
python3 tools/run.py --disk /tmp/pagh.img --keys "ls /mnt\n" --shot /tmp/fs.png
python3 tools/run.py --mouse 900 300 --shot /tmp/cursor.png   # software cursor at (900,300)
# package installer E2E: builds a .ppkg, serves it over HTTP, boots QEMU with a disk
python3 tools/test_pkg.py                    # port 8000 must be free; --release for the release image
# live sockets: host HTTP server on 127.0.0.1:8000 + QEMU user net + http_get
python3 tools/test_sockets.py                # port 8000 must be free; writes docs/live-sockets.txt
```

`rustup target add riscv64gc-unknown-none-elf` once; the nightly is pinned in
`rust-toolchain.toml`.

CI (`.github/workflows/ci.yml`) runs the commands above — the smoke job covers the
four QEMU modes, including the two-run disk acceptance — and reads the toolchain pin
out of `rust-toolchain.toml`. There are **no** other gates yet: no `tools/build.py`, no
`tools/check_safety.py`, no host-test runner, no general E2E driver (those exist in the
x86 repository and were deliberately not carried over). `tools/test_pkg.py` is a
focused E2E for the package path only, and it is not wired into CI. If you add a
gate, add it to CI *and* to this block in the same commit.

## Canonical entry points

One role, one current path — the one CI, the docs and a fresh checkout should use.
Unlike the x86 tree there are **no `-legacy` companions** here: the Windows launcher
(`run.cmd`) and the PowerShell E2E harnesses were not ported, so do not add a second
path "for parity".

```text
kernel-build    -> cargo (target from .cargo/config.toml, linker.ld, -Tlinker.ld)
kernel-run      -> run.sh                  # Linux/QEMU launcher; window by default, --headless for serial
kernel-screen   -> tools/run.py            # boots with ramfb, screendump, font check
kernel-smoke    -> .github/smoke.py        # headless QEMU driver used by CI (serial/ramfb/disk)
kernel-selftest -> the shell's `selftest`  # in-QEMU memory + block + filesystem checks
package-e2e     -> tools/test_pkg.py       # .ppkg + HTTP mirror + QEMU disk, install/run/remove
socket-live     -> tools/test_sockets.py   # host HTTP server + QEMU user net + libpagh http_get
kernel-halt     -> SBI SRST via the shell  # makes QEMU exit 0
```

## Repo layout

- `src/` — the kernel (crate `pagh`, **binary**, target `riscv64gc-unknown-none-elf`,
  `[dependencies]` empty). Modules: `main.rs` (boot phases, global allocator, panic
  handler), `arch/riscv64/{boot,csr,sbi,traps,mmu}.rs`, `fdt.rs`,
  `memory/{pmm,heap,selftest}.rs` (+ `src/memory/README.md`),
  `drivers/{uart,fw_cfg,ramfb,framebuffer,block,virtio_mmio,virtio_net,virtio_input,selftest}.rs`
  (+ `src/drivers/README.md`), `fs/{ext2,format_policy,selftest}.rs`
  (+ `src/fs/README.md`), `vfs/mod.rs` (+ `src/vfs/README.md`),
  `task/{mod,scheduler,selftest}.rs` (+ `src/task/README.md`),
  `user/{mod,signal,elf,syscall,selftest,futex}.rs` (+ `src/user/README.md`),
  `net/{mod,wire,dhcp,dns,tcp,selftest}.rs` (+ `src/net/README.md`),
  `pkg/{mod,sha256}.rs` (+ `src/pkg/README.md`), `console.rs`, `shell.rs`,
  `sync.rs`.
- `userland/` — five static RV64 fixture programs (`hello`, `fault`, `spin`,
  `argv`, `abi`), their linker script and `build.sh` (+ `userland/README.md`).
  The ELFs are committed and embedded with `include_bytes!`; a rebuild must be
  committed with the sources in the same commit.
- `docs/` — architecture, roadmap, screenshots (`shell.png`, `fs.png`, `user.png`,
  `net.png`, …).
- `assets/font8x18.bin` — the 8×18 coverage font (generated by `tools/gen_font.py`).
- `tools/` — `run.py` (boot + screendump + font check), `test_pkg.py` (the
  `pkg` end-to-end harness), `test_sockets.py` (the live socket acceptance:
  host HTTP server + QEMU user net + libpagh `http_get`), `gen_font.py`,
  `font_check.py`.
- `.github/` — CI workflow and the smoke driver.
- `run.sh`, `linker.ld`, `rust-toolchain.toml`, `.cargo/config.toml` — build and run.
- Absent on purpose (do not assume they exist): `host-tests/`, `tools/build.py`,
  `tools/e2e.py`, `vendor/`, `limine/`, `disk.img`, `OVMF.fd`.

## Hard invariants (breaking these looks like "hardware" bugs)

1. **The trap frame layout is the ABI.** `trap_entry`
   (`src/arch/riscv64/traps.rs:57-140`) saves x1…x31, `sepc`, `sstatus` in a fixed
   order; `#[repr(C)] TrapFrame` (`traps.rs:20-55`) declares the same order, and the
   offsets 0…256 are documented in `docs/architecture.md` §4.1. The frame is 264
   bytes (`addi sp, sp, -264` / `addi sp, sp, 264`). Offset 8 holds the frame's own
   base address (the decremented `sp`), not the interrupted `sp`, and the epilogue
   restores `sp` arithmetically. Change the asm, the struct and the doc in one commit;
   never add a field on one side only.
2. **Nothing panics in a trap handler.** `panic = "abort"` in both profiles
   (`Cargo.toml:10-16`): a panic anywhere kills the machine, and inside a trap the
   frame is the only evidence. The handler prints and parks — keep it that way.
   Exception paths never return; only interrupts return via `sret`. The timer
   tick handler may context-switch (M6a): that is safe precisely because each
   thread's trap frame lives on that thread's own stack.
3. **The context switch is the second ABI.** `TaskContext`
   (`src/arch/riscv64/switch.rs`) saves exactly the callee-saved set (`ra`, `sp`,
   `s0`–`s11`) and nothing else: `task_switch` is an ordinary `extern "C"` call,
   so the compiler already assumes caller-saved registers are clobbered. The
   assembly takes its offsets from the Rust layout (const operands in
   `global_asm!`), and a `const _` block fails the build if the struct stops being
   14 pointer-sized fields — edit the struct and the asm follows. Two rules ride
   on top: never call `task_switch` while holding a scheduler lock (select under
   the lock, release, then switch), and never touch the trap frame from here — a
   thread switched out inside a trap keeps its frame on its own stack.
4. **Single-hart boot.** OpenSBI's boot protocol hands S-mode to exactly one
   hart and leaves the rest stopped in its HSM, and the id it picks need not be 0
   (`-smp 4` booted hart 2 — the old `bnez a0, park` parked the only running
   hart and hung silently). `_start` therefore makes no id assumption. All kernel
   state assumes one context and `sync.rs` is one non-reentrant spinlock; SMP is
   not a refactor — it is a new boot protocol plus real locking (and an atomic
   claim, which `rv64gc` here does not let inline asm use yet: Zaamo is off).
5. **`mhartid` is an M-mode CSR.** Reading it from S-mode is an illegal instruction
   (this actually happened: `scause=2, stval=0xf1402573`, machine halted). Use
   `arch::riscv64::hart_id()`, which reads `BOOT_HART_ID`, captured in `_start` while
   the id is still in `a0` (`boot.rs:23-28, 56-63`). The same applies to every other
   M-mode CSR: `ecall` (SBI) is the only door.
6. **Device-tree honesty.** The FDT is the machine description; a parse failure is
   fatal and loud (`src/main.rs`), never a guess. The reader answers exactly five
   questions: `/memory` `reg`, `/cpus:timebase-frequency`, the `virtio,mmio`
   transport windows (M5), the PLIC's `reg` (M3) and the console UART's
   `interrupts` cell (M3). It assumes 2+2 `reg` cells and reads every field as
   big-endian byte-by-byte. Do not extend it speculatively: a new device means a
   named accessor and an update here.
   The two hard-coded addresses are the console UART (`drivers/uart.rs:20`) and
   `fw_cfg` (`drivers/mod.rs:1-11`), because both are needed before the tree is
   parsed — keep them in those places, with the comments that say why.
7. **Boot phase order is load-bearing** (`docs/architecture.md` §3): console before
   any print, FDT before the shell (and before any device work), **frame pool before
   the first allocation**, paging before any MMIO beyond the console, traps before the
   shell, hart parking before Rust. M3 adds two: the timer and the PLIC after the
   trap vector is installed (`sie` must stay 0 until both compare register and
   controller are ready, and the PLIC must route the UART line before the UART
   raises it), M5 adds two more: the block driver after the map and traps (it
   reads MMIO), the filesystem after the driver and the heap (mounting allocates),
   and M7/M8 add the NIC and the pointer after the block driver — both virtio-mmio
   devices on the same mapped windows, both optional, neither may move before the
   map exists.
8. **`.bss` is not zeroed by `_start`.** The kernel relies on the loader zero-filling
   the NOBITS `PT_LOAD` (QEMU does). Three things stand on that: the boot stack, the
   `BOOT_HART_ID` static, and the PMM bitmap's polarity ("a set bit means **free**",
   `memory/pmm.rs:10-16`), which makes an allocation before `memory::init` fail
   instead of handing out the kernel's own image. If you add a boot path that does not
   zero `.bss` (or a custom loader), zero `__bss_start..__bss_end` first.
9. **The frame pool owns physical memory; the heap owns chunks of it.** Reserved and
   never handed out: everything below `__kernel_end`, the boot stack named explicitly,
   and the whole DTB blob (`src/main.rs:87-96`). The heap takes 64 KiB chunks (16
   contiguous frames) and returns a chunk to the PMM once its whole span is free —
   that is what makes `selftest` idempotent. Lock order is heap first, PMM second,
   never the other way round (`memory/heap.rs:28-30`). Deliberate heap limits: a
   request aligned above 16 bytes is refused (null), a request bigger than one chunk
   fails, at most 64 chunks (4 MiB) are ever mapped.
10. **Console markers are the test surface.** `pagh OS — RISC-V (RV64)`,
   `pagh:rv64> `, `pmm         ` (prefix), `framebuffer  ramfb `, the `help` lines
   and `halt: powering off` are asserted by `.github/smoke.py`. Renaming one means
   updating the driver in the same commit (the x86 tree learned this the hard way
   with `[BOOTID]`). Assert counts by prefix, never verbatim: `-m` changes them.
11. **`halt` goes through SBI SRST** (`src/arch/riscv64/sbi.rs`). Parking the hart in
    `wfi` leaves QEMU alive, the smoke test then fails, and the help text lies — do
    not "simplify" it back.
12. **The console has two sinks and one frame boundary.** `console::init`
    (`src/console.rs:44-48`) brings up the UART and then tries the screen; one
    `kprint!`/`kprintln!` is one `flush` (`console.rs:91-104`). The screen is optional
    by design: without `-device ramfb` the probe fails, the reason goes to serial, and
    every framebuffer call is a no-op. Only ASCII `0x20..0x7E` has glyphs in the font
    asset — non-ASCII characters (the banner's em dash) appear in the serial log but
    are dropped on screen.
13. **Anything an interrupt handler can preempt must mask interrupts.**
    `uart::put_byte` takes the UART spinlock inside `csr::without_interrupts`, and
    the same rule applies to every critical section a handler can interrupt. The
    handler runs with interrupts masked and must not: panic, print (outside the
    documented counters/paths), allocate, take a spinlock a normal context may
    hold, or do unbounded work. Prints happen from normal context (`time` reads the
    counters the handler bumps, `selftest` checks them). This closed the latent
    trap that existed while `sie` was still zero; see `docs/architecture.md` §12.1.
14. **Interrupt bring-up order is a contract.** Timer: `set_timer` first, then
    `sie.STIE`; PLIC: `plic::init` (routes the UART source) before
    `uart::enable_rx_interrupt()`/`sie.SEIE`. Reversing either loses a byte or takes
    an interrupt on an unarmed register. `plic::windows()` derives the S-mode
    context page — never map `CONTEXT_BASE` alone (that is firmware's M-mode
    context; the first threshold write faults, as the first M3 boot showed).
15. **`#![no_std]`, no dependencies.** `Cargo.toml`'s `[dependencies]` is empty.
    `alloc` **is** available (`extern crate alloc` + `#[global_allocator]` in
    `src/main.rs`). Nothing that runs *before* the frame pool may allocate, and the
    drivers that run early (console, ramfb, virtio probe) use fixed statics — but
    the filesystem mount and the shell are normal `alloc` users by design. Adding a
    crate is an architecture decision, not an implementation detail.
16. **`unsafe` blocks carry `SAFETY:` comments** — every one, including the asm
    contracts (house style; no automated gate yet, unlike x86's `check_safety.py`).
17. **Only a blank device is ever formatted.** `main.rs::init_fs` mounts first and
    asks `fs::format_policy` only after a failed mount; the policy formats a device
    only when its probed boot area is entirely zero. A partition table, a foreign
    image or an ext2 magic whose superblock does not validate is refused with a named
    reason (`Refusal::*`), and there is no "continue anyway" flag to add. A filesystem
    that fails to mount must never become permission to erase what is on the disk.
18. **Formatting is a gate.** Run `cargo fmt --all` before committing; CI checks it.
19. **`ecall` returns at `sepc + 4`.** `user::syscall::dispatch` bumps the saved
    `sepc` after every returning syscall; `exit`/`exit_group` never return (they
    call `scheduler::exit` after accounting the run). `sret` resumes exactly at `sepc`, so without the bump a
    program loops on the same `ecall` forever — this actually happened to `hello`
    and `fault` before the fix. The trap frame's `sepc` field is part of the
    contract; change the bump and the ABI together.
20. **`sscratch` is a two-state invariant.** While S-mode runs, `sscratch == 0`
    (`traps::init` writes it); before `sret` into U-mode, `enter_user` (main
    thread) or the `trap_return` epilogue (a `clone` child's first entry) stores
    the task's kernel stack pointer there. `trap_entry` starts with
    `csrrw sp, sscratch, sp`: a nonzero `sp` means "came from U-mode", the user
    `sp` goes to frame slot 8 and `sscratch` is cleared; zero is an ordinary
    S-mode trap. The epilogue splits on the **saved** `sstatus.SPP` and, for
    U-mode, re-arms `sscratch` with `frame_base + 264` before restoring the user
    `sp`. Do not read/write `sscratch` outside `traps.rs`/`user/mod.rs`; there is
    exactly one epilogue that `sret`s into U-mode (`enter_user` is a one-shot
    setup of that same state for the main thread, not a second layout).
21. **User entry builds `sstatus` in Rust, once.** `enter_user` and the
    `clone`-child frame builder both call `csr::user_entry_status()`:
    `SIE=0, SPP=0, SPIE=1, SUM=0`. It is written with `csrw sstatus`, not
    `csrci`/`csrsi`: SPIE (bit 5) and SPP (bit 8) do not fit the 5-bit CSR
    immediate, and the earlier `csrci 8`/`csrsi 5` silently left them wrong (the
    machine still booted, which is how it hid). A child's frame reaches `sret`
    through `trap_return`, the same epilogue a preempted U-mode thread uses; do
    not add a second entry layout.
22. **U-mode faults kill the run, never the hart.** Any synchronous exception
    with saved `SPP == 0` (`ecall` excepted) goes to `user::on_fault`, which
    prints one `[user] <name> killed: …` line, records `Exit::Fault`, retires the
    run's other threads with `scheduler::kill_group` and calls `scheduler::exit`;
    the runner releases pages and frames afterwards. A fault is process-fatal on
    purpose: a thread that died holding a userspace lock would leave the others
    waiting forever. S-mode exceptions keep the print-and-park contract. A user
    program's bad pointer must be `-EFAULT` via PTE validation (`U=1` and `R=1`
    for every page, `SUM` only around the copy), not an exception.
23. **`RUN` is not held across blocking work, and outlives every thread.** The
    runner claims the one user program (`Busy` otherwise) and releases `RUN` only
    after `RunState::threads == 0` and the pages are gone; the wait loop
    (`wait_for_run_end`, `yield_now`) and the trap paths only read/update short
    sections. A `clone` child may outlive the main thread, so "the task is
    Exited" is not "the run is over": releasing the address space, fd table or
    heap while a child still runs is use-after-free. One address space, one ASID,
    one program at a time: there is no per-process `satp` and no isolation
    between two user programs.
24. **virtio-net's header is a contract, not a guess.** The driver negotiates
    `VIRTIO_F_VERSION_1` + `VIRTIO_NET_F_MRG_RXBUF`, so every buffer starts with
    a **12-byte** `virtio_net_hdr` (the 10-byte base plus `num_buffers`). A
    10-byte header shifts every captured frame by two bytes — the first version
    did exactly that, visible only in a QEMU `filter-dump`. RX buffers are
    2 KiB (one frame each), posted ahead of time and re-posted on completion; TX
    is one descriptor with header+frame contiguous and a bounded used-ring poll;
    `NO_INTERRUPT` means the device never raises an IRQ. Device addresses are
    Rust pointers because RAM is identity-mapped (`memory::paging`).
25. **The boot stack is 128 KiB; big state belongs on the heap.** It was 64 KiB
    and the deepest normal-context chain (a file-backed `run` with its 4 KiB
    padding write plus loader/heap frames) came within tails of `__stack_bottom`;
    the native-thread work pushed it over and the timer tick handler called a
    value written into `timer::TICK_HANDLER` by the overflow. `VirtioNet`
    initially held a 12 KiB frame ring inline and building it on the boot stack
    overflowed into `.bss` too (`chunk_count = 16384`). Box large buffers in
    device/stack structs, keep `probe`-style builders returning small values, and
    do not spend the remaining ~64 KiB on a stack local.
26. **Network parsing never panics and never trusts a frame.** `src/net/wire.rs`
    is the only place that reads packet bytes: bounds-checked reads, checked
    arithmetic, checksum validation, fragments dropped. Before a DHCP lease
    exists `handle_ipv4` accepts only limited-broadcast frames (that is how
    OFFER/ACK arrive); one interface, one address, one gateway, one DNS. Waiting
    is polling with a tick deadline, never a blocking sleep, because the NIC has
    no interrupt. The TCP client is a single active-open connection for HTTP/1.0
    — do not describe it as a general TCP stack.
27. **Network smoke markers are asserted.** `virtio-net: virtio-net0 at 0x…`,
    `net         DHCP ip 10.0.2.15/…` (boot) and `ping: reply from 10.0.2.2`
    (command) are what `.github/smoke.py --net` looks for. Renaming or
    reflowing them silences the M7 gate; update the driver and the smoke mode in
    the same commit.

## Testing philosophy

What exists: the in-QEMU suites (shell `selftest`: 7 memory + 4 block/pointer + 11 ext2/VFS
+ 4 interrupt/timer + 5 scheduler + 10 user-mode (including the native
thread/futex and the signal/altstack/siginfo/job-control sections) + 11 network
checks, idempotent, each group prints a
free-frame ledger), the headless QEMU smoke driver in six
modes (serial, ramfb, disk-fresh, disk-existing, `--net`, `--keyboard`) — every mode attaches a
virtio-tablet and the boot log must carry `virtio-input: tablet at 0x…` — including the M5
format/mount/persist acceptance and the M7 DHCP/ping acceptance, and the pixel
check that proves a screenshot is really the committed font. The network
acceptance is deterministic: QEMU's slirp answers DHCP and ICMP to `10.0.2.2`
without touching the runner's internet. The user-mode suite is the M6b/native-ABI evidence: ELF accept/reject matrix, round
trip, `Exit::Fault`, a run after the fault, the timer-in-U counter
(`traps::user_timer_ticks()`) moved by the `spin` fixture, the exact argv line
from `userland/argv.elf` captured from U-mode `write`, `userland/abi.elf`
calling every syscall, including the mutation family (it exits 77 and becomes a
SKIP when `/mnt` is absent after the filesystem-independent half passed; with a
disk it creates, renames, hard-links, symlinks, truncates, unlinks and
mkdir/rmdir's entries under `/mnt`, and the kernel verifies they are gone), the
signal sections (markers `abi: altstack ok`, `abi: siginfo ok`,
`abi: jobcontrol ok`; the kernel requires the altstack/stop/continue/restart
counters to move and no live handler, pending bit or `Stopped` task to survive)
and the socket section (bad args/EBADF always; `connect` to `10.0.2.2:8000` plus
send/recv/shutdown/EOF only when a host server is listening, otherwise a
`SKIP`, never a failure). `run demo` must print
`run: demo exited with code 42`, `run fault` must be killed with cause 13 while
the shell survives, `run spin` must exit 7. The package path has its own E2E
(`tools/test_pkg.py`): a real `.ppkg`, a host HTTP mirror, a disk, and the
install/run/remove plus corrupted-package refusal. What does not exist: host
property tests, a general E2E driver for the rest of the kernel, adversarial
fuzzing. That is a gap, not a policy — the rule stands:

- **Input before the banner is discarded, by design.** `uart::init` sets the
  16550's `FCR` to clear both FIFOs, so bytes a driver writes before the guest is
  listening never reach the shell (piping `help\nhalt\n` into QEMU at start-up
  produces a partial command). Scripts wait for `Type 'help' for the command list.`
  first — `.github/smoke.py` does, and so must anything new.
- **A regression fix without a check is not done.** The cheapest check is extending
  `EXPECTED` in `.github/smoke.py`; state that can be exercised belongs in a
  `SELFTEST <name> PASS/FAIL` routine next to `memory/selftest.rs`; pure logic
  (`fdt.rs` is the obvious candidate) wants the x86 pattern of host property tests.
- **Verify by booting, not by reading.** A claim in a doc or a commit message should
  be reproducible from a serial log; the README's log block is an actual capture.
- **Keep the docs' numbers honest.** When a phase, offset, address or command set
  changes, update `docs/architecture.md`, the README status table and the subsystem
  README in the same commit. Stale docs here are treated as bugs.

## Conventions

- Doc comments carry the design: contracts, why-not-what, `SAFETY` notes. Match the
  density of neighboring code; when behavior changes, change the comment with it.
- Each `src/<subsystem>/` has a README that matches the code. Update it when the
  design moves.
- Commits: `area: imperative summary` (`riscv:`, `arch:`, `drivers:`, `memory:`,
  `docs:`, `ci:`), body explains *why* and *what breaks without it*.
- Do not edit files another workstream owns. Code (`src/**`) belongs to the code
  workstreams; documentation (`README.md`, `docs/**`, `AGENTS.md`, `CONTRIBUTING.md`,
  `.github/**`) to the docs workstream — and a subsystem README to whoever writes that
  subsystem, unless it is missing and unclaimed.
- Never commit `target/`, `*.log`, `*.img`, firmware blobs, or screenshots that were
  not produced by the kernel itself.

## Versioning

Semver `MAJOR.MINOR.PATCH` in `Cargo.toml [package].version` — currently `0.14.0`,
which `Cargo.lock` mirrors. **The two move together in one commit**: CI builds with
`--locked`, and cargo refuses to rewrite the lock (`cannot update the lock file …
because --locked was passed`), so a `Cargo.toml`-only bump fails the build job in
seconds. Verify locally with `cargo build --locked` after any version change.

- **MAJOR** — image-level breaks: boot/disk format changes, removed commands or
  syscall families, incompatible userland expectations. While the crate is `0.x`,
  MINOR may still break; say so in the commit body.
- **MINOR** — a new user-visible feature or a behavior change, even a compatible one
  (a new shell command, the screen coming up, the frame pool). Land the bump in the PR
  that ships the feature, not later.
- **PATCH** — fixes, diagnostics, docs, tooling, with no behavior change. Docs-only
  commits do not bump.
- **Tags** — tag the release commit with the bare version string (`0.5.0`) and push
  it. Tags must never run ahead of `Cargo.toml`. Note: `0.2.0` was tagged on a commit
  whose manifest still said `0.1.0` (the drift this rule exists to prevent); it is
  left as history, and `0.4.0` is the first tag that matches its manifest (M3/M5/M6a landed after `0.3.0` was written but before it was tagged). `0.5.0` ships M6b; `0.6.0` ships M7 (the network: virtio-net + own stack, DHCP,
  DNS, ICMP, a TCP client); `0.7.0` ships M8 (virtio-input pointer and the
  software cursor that is part of the console's frame); `0.8.0` ships the
  streaming `run <path>` (risk #5); `0.9.0` ships the virtio-keyboard; `0.9.1`
  makes `run.sh` open the window by default; `0.10.0` ships the native userland
  ABI (argv/envp, 15 syscalls, fd table, brk heap) and the native `pkg` installer
  (HTTP index + `.ppkg`, size/SHA-256 gate before parsing); `0.10.1` is the
  follow-up patch: TCP window/reconnect/timeout fixes, virtio-net RX ring
  preservation, bundle limits and a manifest-failure rollback; `0.11.0` adds the
  seven file-mutation syscalls (mkdirat/unlinkat/symlinkat/linkat/fchmodat/
  ftruncate/renameat2) and the mutation half of the ABI fixture; `0.12.0` adds
  BSD sockets over the one TCP client and the POSIX memory/descriptor set:
  mmap/munmap/mprotect, pipe2/dup/dup2/dup3, poll/ppoll; `0.13.0` adds native
  threads and futexes: clone(220) with CLONE_VM|CLONE_THREAD, futex(98) wait/wake,
  gettid/set_tid_address/exit_group; `0.14.0` adds POSIX-style signals
  (rt_sigaction/rt_sigprocmask/kill/tgkill/rt_sigreturn) with user handlers,
  blocked/pending masks, SA_RESTORER and a delivered user signal frame. The same rule applies
  to both: the tag equals the manifest and the `Cargo.lock` entry, and the
  release is not optional.

## Known gaps (documented, not hidden)

The authoritative list is `README.md` → «Чего нет», with per-milestone criteria in
`docs/roadmap.md`. The ones that bite an agent most often:

| Gap | Consequence for your change |
|---|---|
| Sv39 is on with an **identity** map, so a physical address is still a pointer | any address handed to a device (`fw_cfg`, ramfb) relies on that identity; a non-identity or per-process map breaks it silently |
| The tick is 100 Hz and preemption happens only on it (no priorities/deadline scheduler) | scheduling policy is a scheduler change; `timer::set_tick_handler` is the seam, not a policy |
| No TLS/apt: the network stack speaks plain HTTP | `pkg` verifies size + SHA-256 from the index only (integrity, not signatures) and says so; apt stays deliberately unwired until the vendored-TLS + OpenPGP milestone lands. Never call the digest a signature or `pkg` an apt replacement |
| TCP is one active-open client (HTTP/1.0), not a general stack, and userland gets one `AF_INET`/`SOCK_STREAM` socket on top of it | no `listen`/`accept`/`bind`, UDP/IPv6 sockets, DNS syscall, multiple connections, out-of-order reassembly, congestion control or TLS |
| The NIC is polled and the stack is synchronous | network progress happens while a command, a socket syscall (`connect`/`send`/`recv`/`close`) or the shell's idle loop polls; an IRQ-driven NIC is a later milestone |
| Interrupt handlers cannot allocate or print | put work in normal context; bump a counter and read it from `time`/`selftest` |
| The pointer/keyboard have **no IRQ**: both are polled from the shell's idle loop | a long-running command (or a blocking wait that does not poll) freezes the cursor and drops typed bytes; do not put input polling in a place that never returns to the idle loop |
| The keyboard translates **US layout only** and ignores arrows/Home/End/dead keys | a program needing another layout or key semantics needs a keymap layer, not a driver tweak |
| Clicks/selection are not consumed: `BTN_LEFT` is read and stored, nothing acts on it | mouse-driven editing/selection is a UI milestone, not a driver change |
| One address space, one ASID: user pages carry `U=1` in the kernel's single Sv39 tree | two user programs are not isolated from each other; no per-process `satp`; `brk` is one fixed 1 MiB window, `fork`/`exec`/`wait` are another workstream; 16 fds, console reads are non-blocking (`-EAGAIN`), `getdents64` is a snapshot taken at `openat`, `openat`/`mkdirat` ignore `mode` and `fchmodat` is a per-run override never written to the inode or enforced; `run <path>` streams from the VFS and accepts images up to 1 MiB (`MAX_IMAGE_BYTES`), one image at a time |
| The filesystem has **no journal** (M5 deliberately skipped the x86 WAL) | a power cut between two writes can leave an operation half-applied; do not claim crash consistency |
| One block device slot, no partitions/hot-plug | a second disk is a design change, not a registration |
| Formatting is the only destructive operation and is gated by `format_policy` | never format "to recover": a failed mount is not permission to erase |
| No S-mode page-fault recovery | a kernel bad access halts (`[EXC] … halting`); a *user* fault ends that task instead (`user::on_fault`) |
| Screenshot pixels are not gated in CI yet (`--check-font` returns a non-zero code) | wiring it into the smoke job is an open task, not a reason to trust a shot by eye |
| No host tests; state coverage is `selftest` only | you own the verification: extend the smoke driver or add a `SELFTEST` routine (the M6b suite is `src/user/selftest.rs`) |
| Threads are a native MVP: `clone` with explicit entry, no TLS/`CLONE_SETTLS`, no join/waitid, no priorities or PI futexes, max 14 live threads per run | libc's `clone` wrapper cannot be mapped one-to-one; document the native signature and keep the futex table static (32 waiters / 16 addresses) rather than growing it silently |
| Signals are a documented subset: no `sigqueue`/`sival_ptr`/queued signals, no SIGFPE producer, no tty/process groups, `SA_RESTART` only for `futex`/`nanosleep` (full timeout again), `SIGCHLD` has no child-exit source (si_status 0), and `kill(0, SIGSTOP)` can park a whole run | the frame/`siginfo_t`/`ucontext_t` layouts are the ABI (see invariant 41) and must move with `docs/architecture.md` + `userland/abi.rs`; do not claim Linux signal semantics, and add queueing/tty/process-group state as a real design change rather than a flag |
| 19 compiler warnings remain (unused diagnostic helpers, `trap_entry as usize`, …) | do not add more; warning-cleanup PRs are welcome |
| No `LICENSE` file although `Cargo.toml` says MIT | do not add a license your work does not own |
| `src/memory/paging.rs` owns the only page table; `src/arch/riscv64/mmu.rs` is encoding only | do not map over the kernel image or drop the identity map while a device holds a physical address |
28. **The software cursor is frame content.** It is erased before text is drawn and
    repainted after the frame flush; only its 12x19 rectangle is pushed
    (`flush_rect`). Drawing under the arrow outside that path leaves a stale
    arrow or a hole. Input moves the pointer only through
    `framebuffer::cursor_move_to`; the arrow never reaches the device on its own.
29. **The pointer and keyboard are polled, and their absence is normal.** `virtio_input::poll()` runs
    from the shell's idle loop next to `net::poll()` and the UART ring; there is no
    routed IRQ for it. A blocking wait that never returns to that loop freezes the
    cursor and drops typed keys (and the network). Keyboard bytes are consumed
    exactly like UART bytes (`virtio_input::try_key()` next to
    `uart::try_get_byte()`); a full key ring drops a byte rather than blocking the
    idle loop. A boot without a pointer or keyboard device is a valid configuration
    (`NO_MOUSE=1`, serial-only CI): one log line `virtio-input: no device`, no
    cursor or keys, nothing else changes.
30. **User pointers go through `copy_from_user`/`copy_to_user`, never through a
    bare dereference.** The helpers validate every page of the range before the
    copy (`U=1` and `R=1`, plus `W=1` for stores), set `SUM` only around the
    copy, and restore `sstatus` from the exact entry value on every path. A bad
    pointer is `-EFAULT` and the fault counter, never an S-mode page fault — a
    store to a read-only user page would otherwise print and halt the hart. Any
    new syscall with a buffer argument must use these helpers.
31. **The fd table is per run, not per kernel.** `src/user/syscall.rs` has one
    static table of 16 entries, reset by `run_source` before spawning and
    released after the task is gone; 0/1/2 are the console, 3.. are
    `Arc<dyn VfsNode>` + offset + flags, and at most one is
    `FdEntry::Socket { id, connected }` (the stack owns one TCP connection;
    a second `socket()` is `-ENOSPC`). Never hold its lock across a VFS, block
    or network call (the block driver polls and may `wfi`; `tcp::close` sends a
    segment): copy the `Arc`/offset/handle out, do the I/O, publish afterwards.
    A node or connection must not survive a faulted program into the next run;
    `release_fds` closes a socket the program left open.
32. **The user heap is exactly `0x2000_0000..0x2010_0000` (1 MiB).** `brk` maps
    zeroed PMM frames on demand through `paging::map(Flags::user_rw())`, records
    them in `RunState::heap_frames`, and rolls a partial growth back on
    `-ENOMEM`. The runner's `release_heap` unmaps and frees them after `reap`
    and before `run_source` returns; the selftest's PMM ledger is the check. A
    new mapping in this window, or an image placed there, is `-ENOMEM`, not an
    overlap.
33. **`read(0)` never blocks.** It tries `uart::try_get_byte()` then
    `virtio_input::try_key()` and returns `-EAGAIN` when both are empty;
    libpagh retries. A blocking read in the trap path would freeze the cursor,
    the keyboard and (because the shell is not polling) the network. Socket
    `recv` follows the same contract: one `net::poll()` and `-EAGAIN`, never a
    sleep waiting for a segment.
34. **The package gate runs before the package parser.** `pkg install` streams
    `<repo>/<url>` into `/mnt/pkg/<name>.ppkg` in 4 KiB chunks and hashes the same
    bytes. The byte count may never exceed the index's `size`; at EOF it must
    equal it and the SHA-256 must match, or the download is removed and nothing
    under `/mnt/bin` is touched. Only after that gate is `PPKG0001` parsed and a
    payload copied to `/mnt/bin`; the manifest is written last. Never reorder
    this into "parse while downloading", "install then verify", or a whole-package
    heap buffer. There is no TLS, no OpenPGP and no apt: do not describe the
    SHA-256 as authentication, and do not call `pkg` an apt replacement.

35. **Every path mutation shares one shape and one error contract.** `mkdirat`,
    `unlinkat`, `symlinkat`, `linkat`, `renameat2` and `fchmodat` read the path
    through `read_user_path`/`read_path` (absolute UTF-8, <= 255 bytes + NUL,
    `-ENAMETOOLONG` rather than truncation), split it with
    `vfs::parent_and_leaf`, resolve the parent with `vfs::walk(parent, true)`,
    then call exactly one mutating `VfsNode` method on that directory.
    `unlinkat`/`linkat` resolve the final component with `lookup_path` (the link
    itself, never its target); `fchmodat` follows it like `chmod`. A `flags` bit
    the ABI does not implement is `-EINVAL`, never silently ignored. A mutation
    is one VFS transaction, so an error leaves the tree as it was; the one
    create-then-install path, `openat(O_CREAT)`, removes the new entry when the
    descriptor cannot be installed and rolls the descriptor back when `O_TRUNC`
    fails. Permissions are the documented gap: `mkdirat`/`openat` ignore `mode`,
    and `fchmodat` only fills the per-run `ModeTable` that `fstat` reads — not
    the ext2 inode, not an access check, gone with the run (`-ENOTSUP` on a
    synthetic node, `-ENOSPC` past 16 overrides). A non-empty `rmdir` is
    `-ENOTEMPTY` (ext2 reports it as `AlreadyExists` internally; translate it),
    and a cross-mount `linkat`/`renameat2` is `-ENOTSUP`. Do not add path
    mutation anywhere else (the shell keeps using the VFS directly); the
    `selftest` run through `userland/abi.elf` is the evidence.

36. **The socket ABI is one connection, and every wait must pump the NIC.**
    `socket` accepts only `AF_INET`/`SOCK_STREAM`/protocol 0; a second socket
    while one fd is open is `-ENOSPC`. `FdEntry::Socket` lives in the per-run
    table and `read`/`write`/`close` route to `tcp::recv_nonblocking`/
    `tcp::send`/`tcp::close`; `shutdown` closes the connection but keeps the
    descriptor (`read` then returns 0, `write` is `-EPIPE`). A user program runs
    while the shell sits in `wait_for_run_end`, so **nothing else polls the NIC**:
    `connect` and `send` run with interrupts enabled for the wait (the same
    deliberate exception `nanosleep` makes, with no lock held) so ticks and
    preemption still work, and the TCP wait loop calls `net::poll()` every
    iteration. `recv` is non-blocking by contract. Every `sockaddr` goes through
    `copy_from_user`; the length (exactly 16), family, port (big-endian) and
    address are validated before any TCP state changes. IPv4 literals only:
    there is no DNS syscall, no `listen`/`accept`/`bind`, no UDP or IPv6 socket,
    and no TLS. The live evidence is `tools/test_sockets.py` /
    `docs/live-sockets.txt`; the CI-safe evidence is the always-running bad-arg
    half of `userland/abi.elf` plus its `SKIP` when `10.0.2.2:8000` is closed.
37. **The run, not the thread, owns the address space.** `RunState` is created by
    `run_source`, holds the shared address space, fd table, heap/mmap tracking and
    the futex run id, and is released only after the last thread is `Exited` and
    reaped. A thread's exit only decrements `RunState::threads` (and records the
    main thread's code); it never tears down fd/heap/mmap. `exit_group` and
    `on_fault` retire survivors with `scheduler::kill_group` and zero the counter;
    the runner then does the one teardown. A new field that a thread may touch
    while another runs belongs in `RUN`, not in `Task`.
38. **Every thread owns a kernel stack; the user stack is not yours.** A `clone`
    child gets the scheduler's 16 KiB + 4 KiB guard PMM stack like any task, and
    starts through the one trap-return path: a zeroed `TrapFrame` at the top of
    that stack (`sepc=entry`, `sp=user stack`, `a0=arg`, `sstatus =
    user_entry_status()`) and `TaskContext { ra = trap_return, sp = frame_base }`.
    Never invent a second frame layout or a second `sret`. The user stack address
    is caller-provided: validate it (16-byte aligned, 16 bytes below it in a
    mapped `U|R|W` page) and never free or zero it at thread exit; its `mmap`
    region is released by the runner with the run.
39. **Futex lock order is queue → scheduler, and the queue is never held across
    a block.** `user::futex` registers a waiter, drops its lock, calls
    `scheduler::block_current`, and removes the entry on return. Do not call
    `wake_task` from the timer/trap path, do not take the futex lock under the
    scheduler lock, and do not hold either across a context switch. The wait
    value check is done by the syscall layer before registration; a syscall trap
    runs with `SIE = 0`, which is what makes check + insert + block atomic on one
    hart. Limits are static and documented: 32 waiters, 16 addresses.
40. **`exit` is a thread exit; `exit_group` is a run exit.** `exit` decrements
    `threads`, keeps the main thread's code and discards a non-main code; the
    last thread stores the result. `exit_group` retires every thread and uses the
    main thread's code if it already exited, otherwise the caller's. A fault is
    an `Exit::Fault` for the whole run. Do not make `exit` call
    `kill_group`, and do not let a thread return to U-mode after either call.

41. **Signals deliver one fixed user frame; alt stacks and stops are scheduler
    state.** The frame is 736 bytes below `sp` (or below the alternate-stack
    top): saved `TrapFrame`, mask, magic, restorer, 128-byte `siginfo_t`,
    `prev_frame`, signum, 288-byte `ucontext_t`, frame flags. The offsets are
    asserted in `src/user/signal.rs` and documented in `docs/architecture.md`
    §9.3; change both with `userland/abi.rs` in one commit. `siginfo_t` is
    pagh's own (signo/errno/code at 0/4/8, `si_pid`+`si_uid` or `si_addr` at
    16, `si_status` at 24) and `ucontext_t` is flags/link/sigmask followed by
    32 `gregs` and `pc`. `SA_SIGINFO` selects the three-argument handler; without
    it the native `(signo, *siginfo)` convention stays. Every page of the frame
    and of the alternate-stack range is validated `U|R|W` before the copy: a bad
    stack is a clean signal death, never a kernel fault. `sigaltstack` is
    per-thread; `SS_ONSTACK` means `alt_active > 0`; nested delivery while
    already on the alternate stack does not re-switch, and `rt_sigreturn` pops
    exactly the level the frame recorded. Job control stops only the running
    thread from its own `default_action` (`scheduler::stop_current`), never by
    editing another task's context from `kill`; `State::Stopped` is not in the
    ready queue, `wake_task` must not wake it, `SIGCONT` is `continue_task` plus
    clearing pending stop bits, and `kill_group`/liveness scans include it.
    `SA_RESTART` is honoured only for `futex`/`nanosleep` by leaving `sepc` on
    the `ecall` and `a0` untouched; do not extend it to other syscalls without
    an explicit restart contract.
