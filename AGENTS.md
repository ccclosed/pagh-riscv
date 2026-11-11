# AGENTS.md — guide for AI agents (and humans) working on pagh-riscv

A small 64-bit RISC-V (RV64) kernel in Rust: `#![no_std]`; the kernel code is
written here, the only crates are the vendored TLS 1.3/X.509 stack (since 0.19.0),
and it is booted by OpenSBI in S-mode on QEMU's `virt` machine, with a polled
serial console and a ramfb screen. Written by AI under human supervision — read this file before
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
| `docs/roadmap.md` | M1–M8, what "done" means for each, what is explicitly out of scope |
| `docs/screenshot.md` | how a PNG of the screen is taken and how its pixels are verified |
| `docs/tls.md` | TLS 1.3: the verifier's gates, the shell's `https`, and the userland session syscalls |
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
# M7 acceptance (the smoke job runs it too): virtio-net on QEMU's user network,
# a DHCP lease at boot, then `net` and an ICMP echo to the gateway.
python3 .github/smoke.py target/riscv64gc-unknown-none-elf/debug/pagh --net
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
selftest                                        # in-QEMU suites: 7 mem + 4 block/pointer + 11 fs + 4 arch/timer + 5 task + 14 user (14/14 with /mnt, 12/14 + 2 skip without) + 12 net + ledgers
# from the host
python3 tools/run.py --keys "help\n" --shot /tmp/shot.png --check-font --scan-all
python3 tools/run.py --disk /tmp/pagh.img --keys "ls /mnt\n" --shot /tmp/fs.png
python3 tools/run.py --mouse 900 300 --shot /tmp/cursor.png   # software cursor at (900,300)
# package installer E2E: builds a .ppkg, serves it over HTTP, boots QEMU with a disk
python3 tools/test_pkg.py                    # port 8000 must be free; --release for the release image
# live sockets: host HTTP server on 127.0.0.1:8000 + QEMU user net + http_get
python3 tools/test_sockets.py                # port 8000 must be free; writes docs/live-sockets.txt
# live UDP: ext2 image + QEMU user net + libpagh udp_echo + host echo server
python3 tools/test_udp.py                    # builds nothing; writes docs/live-udp.txt
# live TCP server: mke2fs -d image + QEMU user net (hostfwd) + libpagh tcp_echo
python3 tools/test_tcp_server.py             # builds nothing; writes docs/live-tcp-server.txt
# live TLS 1.3: internet + shell https, negative chain/SAN/resolve cases, and the
# libpagh https_get example in the guest (TLS syscalls 1100-1103); writes docs/live-tls.txt
python3 tools/test_tls.py                    # needs host internet; --no-userland skips the example
```

`rustup target add riscv64gc-unknown-none-elf` once; the nightly is pinned in
`rust-toolchain.toml`.

CI (`.github/workflows/ci.yml`) runs the commands above — the smoke job covers the
six QEMU modes, including the two-run disk acceptance — and reads the toolchain pin
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
kernel-smoke    -> .github/smoke.py        # headless QEMU driver used by CI (serial/ramfb/keyboard/net/disk)
kernel-selftest -> the shell's `selftest`  # in-QEMU memory + block + filesystem checks
package-e2e     -> tools/test_pkg.py       # .ppkg + HTTP mirror + QEMU disk, install/run/remove
socket-live     -> tools/test_sockets.py   # host HTTP server + QEMU user net + libpagh http_get
udp-live        -> tools/test_udp.py       # ext2 + QEMU user net + libpagh udp_echo + host echo
tcp-server-live -> tools/test_tcp_server.py # mke2fs -d + QEMU hostfwd + libpagh tcp_echo
kernel-halt     -> SBI SRST via the shell  # makes QEMU exit 0
```

## Repo layout

- `src/` — the kernel (crate `pagh`, **binary**, target
  `riscv64gc-unknown-none-elf`; `[dependencies]` is the exactly pinned, vendored
  TLS/X.509 set). Modules: `main.rs` (boot phases, global allocator, panic
  handler), `arch/riscv64/{boot,csr,sbi,traps,mmu}.rs`, `fdt.rs`,
  `memory/{pmm,heap,selftest}.rs` (+ `src/memory/README.md`),
  `drivers/{uart,fw_cfg,ramfb,framebuffer,block,virtio_mmio,virtio_net,virtio_input,selftest}.rs`
  (+ `src/drivers/README.md`), `fs/{ext2,format_policy,selftest}.rs`
  (+ `src/fs/README.md`), `vfs/mod.rs` (+ `src/vfs/README.md`),
  `task/{mod,scheduler,selftest}.rs` (+ `src/task/README.md`),
  `user/{mod,process,signal,elf,syscall,selftest,futex,stack}.rs` (+ `src/user/README.md`),
  `net/{mod,wire,dhcp,dns,tcp,selftest}.rs` (+ `src/net/README.md`),
  `pkg/{mod,sha256}.rs` (+ `src/pkg/README.md`), `console.rs`, `shell.rs`,
  `sync.rs`.
- `userland/` — nine static RV64 fixture programs (`hello`, `fault`, `spin`,
  `argv`, `abi`, `exec-child`, `stack`, `stack-over`, `stack-fork`), their
  linker script and `build.sh`
  (+ `userland/README.md`).
  The ELFs are committed and embedded with `include_bytes!`; a rebuild must be
  committed with the sources in the same commit.
- `docs/` — architecture, roadmap, screenshots (`shell.png`, `fs.png`, `user.png`,
  `net.png`, …).
- `assets/font8x18.bin` — the 8×18 coverage font (generated by `tools/gen_font.py`).
- `tools/` — `run.py` (boot + screendump + font check), `test_pkg.py` (the
  `pkg` end-to-end harness), `test_sockets.py` (the live TCP acceptance:
  host HTTP server + QEMU user net + libpagh `http_get`), `test_udp.py` (the
  live UDP acceptance: ext2 image + host echo server + libpagh `udp_echo`),
  `udp_echo_server.py`, `test_tcp_server.py`, `gen_font.py`, `font_check.py`.
- `.github/` — CI workflow and the smoke driver.
- `run.sh`, `linker.ld`, `rust-toolchain.toml`, `.cargo/config.toml` — build and run.
- Absent on purpose (do not assume they exist): `host-tests/`, `tools/build.py`,
  `tools/e2e.py`, `limine/`, `disk.img`, `OVMF.fd`. `vendor/` is **not** absent: it
  is the committed source of the pinned TLS/X.509 crates, selected by
  `.cargo/config.toml` so the build is offline and `--locked`.

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
   the id is still in `a0` (`boot.rs:30-35`, static at `63-70`). The same applies to every other
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
   shell (nothing to park: firmware already left the other harts in HSM stopped). M3 adds two: the timer and the PLIC after the
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
15. **`#![no_std]`, and the dependency set is vendored and pinned.** Since 0.19.0
    `[dependencies]` holds the TLS 1.3/X.509 stack: 11 direct pins (`embedded-tls`
    with its local `Finished`-ordering patch, `embedded-io[-async]`, `rand_core`,
    `rsa`, `p384`, `ed25519-dalek`, `sha2`, `p256`, `signature`, `const-oid`), 77
    crates in total under `vendor/`, built offline via `.cargo/config.toml`. The
    rest of the kernel uses no crates. `alloc` **is** available (`extern crate
    alloc` + `#[global_allocator]` in `src/main.rs`). Nothing that runs *before*
    the frame pool may allocate, and the drivers that run early (console, ramfb,
    virtio probe) use fixed statics — but the filesystem mount and the shell are
    normal `alloc` users by design. Adding a crate, refreshing the vendored set, or
    dropping the embedded-tls patch is an architecture decision, not an
    implementation detail.
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
22. **U-mode faults kill the process, never the hart.** Any synchronous
    exception with saved `SPP == 0` (`ecall` excepted) goes to `user::on_fault`,
    which prints one `[user] <name> killed: …` line, records `Exit::Fault`,
    retires the process's other threads with `scheduler::kill_group` and calls
    `scheduler::exit`; the runner or the parent's `wait4` releases pages and
    frames afterwards. A fault is process-fatal on purpose: a thread that died
    holding a userspace lock would leave the others waiting forever. S-mode
    exceptions keep the print-and-park contract. A user program's bad pointer
    must be `-EFAULT` via PTE validation (`U=1` and `R=1` for every page, `SUM`
    only around the copy), not an exception.
23. **The process outlives every thread, and owns its address space.** A
    `Process` (`src/user/process.rs`) is created by `run_source`/`fork`, holds
    `root`/`satp`/`asid`, the fd row, heap/mmap tracking, `threads`, `exit` and
    `wait_tid`, and is released only after `threads == 0` and `reap`. A thread's
    exit only decrements `Process::threads` (and records the main thread's code);
    it never tears down fd/heap/mmap. `exit_group`/`on_fault` retire survivors
    with `scheduler::kill_group` and zero the counter; the runner or `wait4` then
    does the one teardown. `Task::satp` points at the process root and
    `scheduler::install_satp` switches it before `task_switch` with a full
    `sfence.vma`; ASID is a TLB tag, not permission to skip the flush. A field a
    thread touches while another runs belongs in `Process`, not in `Task`; a
    per-thread field (blocked/pending/altstack) stays in `ThreadSignal`.
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
    do not spend the remaining ~64 KiB on a stack local. The same rule applies
    to a user task's 16 KiB kernel stack: `execve` runs the ELF loader there, and
    the loader's 256-entry page-plan array is a heap `Vec` for exactly this
    reason — it used to overflow the guard during the ABI fixture's exec child.
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
+ 4 interrupt/timer + 5 scheduler + 14 user-mode (including the native
thread/futex and the signal/altstack/siginfo/job-control sections, the three
demand-grown-stack runs and `user-tls-abi` — `userland/tls.elf` exercising
`getrandom(278)` and the fail-closed answer of every TLS syscall on a descriptor
that is not a session, with no server and no `/mnt` needed) + 12 network
checks, idempotent, each group prints a
free-frame ledger; `net-tls-session-gate` adds the kernel-side view of the same
session table), the headless QEMU smoke driver in six
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
counters to move and no live handler, pending bit or `Stopped` task to survive),
the process family (`fork`/`execve`/`wait4`/`getppid`; markers `abi: fork ok`,
`abi: wait ok`, `abi: exec ok`, `abi: exec wait ok`; `live_processes() == 0`
and the fork/exec counters must move), the demand-grown-stack pair
(`user-stack-grows` runs a 160 KiB chain that must return and print
`stack: deep recursion ok`, with `stack::stats()` showing at least the 16 pages
the initial window does not cover; `user-stack-cap` runs a 1.2 MiB chain that
must die `Exit::Fault` with `cause == store page fault` after exactly
`MAX_GROWTH_PAGES = 240` grown pages and at least one refusal; `user-stack-fork`
forks a chain that has already grown ~14 pages and requires both markers
(`stack: fork child ok`, `stack: fork parent ok`), the fork/wait counters and an
unchanged ledger, so `fork`'s copy of `stack_low..USER_STACK_BASE` and `wait4`'s
teardown of it are both exercised; all three compare the
PMM ledger before and after) and the socket section (bad args/EBADF always; `connect` to `10.0.2.2:8000` plus
send/recv/shutdown/EOF only when a host server is listening, otherwise a
`SKIP`, never a failure). The TLS *handshake* is not in this suite — it needs a
server — so `tools/test_tls.py` is the acceptance run for it: the shell's
`https` against `example.com` plus the two negative gates (a local self-signed
server for the right name must fail `cause=chain`; a real server whose chain is
trusted but whose SAN does not cover the name must fail `cause=hostname`), and
the libpagh `https_get` example doing the same verified fetch from userland
through syscalls 1100–1103 (transcript `docs/live-tls.txt`). `run demo` must print
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

Semver `MAJOR.MINOR.PATCH` in `Cargo.toml [package].version` — currently `0.21.0`,
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
  blocked/pending masks, SA_RESTORER and a delivered user signal frame;
  `0.15.0` completes the signal surface: sigaltstack/SA_ONSTACK, full siginfo,
  SA_SIGINFO/ucontext, SA_NODEFER/RESETHAND/RESTART and journal-style
  job control (SIGSTOP/CONT, stopped scheduler state); `0.16.0` integrates
  native processes: fork(1000)/execve(221)/wait4(260)/getppid(173), a per-process
  Sv39 root and 16-bit ASID with `satp` switching, deep-copy fork, fd/pipe
  inheritance (sockets become `-ENOTSUP` in the child), signal handling across
  fork/exec, wait status and the orphan policy, without changing the 0.15 signal;
  `0.16.1` bounds the execve path C-string read (a missing NUL no longer walks
  past the 256-byte buffer); `0.16.2` is the C-runtime/dynamic-linking docs
  patch; `0.17.0` adds userland UDP sockets (`socket(SOCK_DGRAM)`, `bind(200)`,
  `sendto`/`recvfrom`, per-port bounded queues and a libpagh `UdpSocket`);
  `0.18.0` adds TCP server sockets (`bind(200)` for a stream socket, `listen(201)`,
  `accept(202)` with a bounded queue and peer `sockaddr_in`, one listener, a
  libpagh `TcpListener` and the `tcp_echo` live fixture) and fixes the virtio-net
  used-ring advance; `0.19.0` adds the TLS 1.3 client with fail-closed chain/SAN/
  clock/`CertificateVerify` verification; `0.20.0` adds demand-grown user stacks
  (64 KiB initial window, page-at-a-time growth on a load/store fault below
  `stack_low` up to the 1 MiB per-process cap, a real guard below
  `USER_STACK_FLOOR`, `stack_low` bookkeeping carried through fork/exec/teardown
  and the `user-stack-grows`/`user-stack-cap` selftests); `0.21.0` makes TLS and
  entropy reachable from userland: `getrandom(278)` over the kernel entropy pool
  and the pagh-native `tls_connect(1100)`/`tls_write(1101)`/`tls_read(1102)`/
  `tls_close(1103)` family (kernel-side fail-closed handshake on a connected TCP
  descriptor, plaintext `read`/`write`/`poll`/`shutdown`/`close` on it,
  `-ECONNRESET` for a TCP close without `close_notify`, no flag that weakens
  verification), the `user-tls-abi`/`net-tls-session-gate` selftests and the
  libpagh `https_get` phase of `tools/test_tls.py`. The same rule applies
  to both: the tag equals the manifest and the `Cargo.lock` entry, and the
  release is not optional.

## Known gaps (documented, not hidden)

The authoritative list is `README.md` → «Чего нет», with per-milestone criteria in
`docs/roadmap.md`. The ones that bite an agent most often:

| Gap | Consequence for your change |
|---|---|
| Sv39 is on with an **identity** map, so a physical address is still a pointer | the kernel root and every process root copy that identity map; any address handed to a device (`fw_cfg`, ramfb) relies on it. `create_user_space` copies only supervisor leaves, so a kernel mapping added after a process root was created is absent there — do not add kernel maps under a live process without a plan |
| The tick is 100 Hz and preemption happens only on it (no priorities/deadline scheduler) | scheduling policy is a scheduler change; `timer::set_tick_handler` is the seam, not a policy |
| `pkg`/apt have no package signatures and no TLS transport in the installer | TLS 1.3 exists in the kernel (0.19.0 client, 0.21.0 userland sessions) but `pkg` speaks plain HTTP on purpose; it verifies size + SHA-256 from the index only (integrity, not signatures) and says so; apt stays deliberately unwired until OpenPGP package-signature verification lands. Never call the digest a signature or `pkg` an apt replacement |
| TCP is a minimal client + one listener, not a general stack; userland gets one active `AF_INET`/`SOCK_STREAM` client, one listener (`bind`/`listen`/`accept`) and `AF_INET`/`SOCK_DGRAM` (bind/sendto/recvfrom) | one listener, backlog ≤ 4, 8 connection slots (4 KiB rx each); no UDP `connect`, IPv6 sockets, DNS syscall, keepalive/TIME_WAIT/half-close, out-of-order reassembly, congestion control or TLS; UDP is 8 bindings × 4 datagrams × 1024 B, one interface |
| The NIC is polled and the stack is synchronous | network progress happens while a command, a socket syscall (`connect`/`send`/`recv`/`close`) or the shell's idle loop polls; an IRQ-driven NIC is a later milestone |
| Interrupt handlers cannot allocate or print | put work in normal context; bump a counter and read it from `time`/`selftest` |
| The pointer/keyboard have **no IRQ**: both are polled from the shell's idle loop | a long-running command (or a blocking wait that does not poll) freezes the cursor and drops typed bytes; do not put input polling in a place that never returns to the idle loop |
| The keyboard translates **US layout only** and ignores arrows/Home/End/dead keys | a program needing another layout or key semantics needs a keymap layer, not a driver tweak |
| Clicks/selection are not consumed: `BTN_LEFT` is read and stored, nothing acts on it | mouse-driven editing/selection is a UI milestone, not a driver change |
| Per-process Sv39 root/ASID, but no COW/demand paging beyond the user stack: `MAX_PROCESSES = 6`, `fork` deep-copies every page, `mmap`/`brk` allocate eagerly, ASID is only a TLB tag (switch still flushes) | releasing a process root while any thread lives, or reusing an ASID without the flush, aliases two processes; a seventh live process is `-EAGAIN`, not silent serialisation. `brk` is one fixed 1 MiB window, `MAP_SHARED`/`MAP_FIXED`/file-backed `mmap` are absent. 16 fds per process, console reads are non-blocking (`-EAGAIN`), `getdents64` is a snapshot taken at `openat`, `openat`/`mkdirat` ignore `mode` and `fchmodat` is a per-process override never written to the inode or enforced; `run <path>` streams from the VFS and accepts images up to 1 MiB (`MAX_IMAGE_BYTES`), one foreground image at a time |
| The filesystem has **no journal** (M5 deliberately skipped the x86 WAL) | a power cut between two writes can leave an operation half-applied; do not claim crash consistency |
| One block device slot, no partitions/hot-plug | a second disk is a design change, not a registration |
| Formatting is the only destructive operation and is gated by `format_policy` | never format "to recover": a failed mount is not permission to erase |
| No S-mode page-fault recovery | a kernel bad access halts (`[EXC] … halting`); a *user* fault ends that process instead (`user::on_fault`) |
| Screenshot pixels are not gated in CI yet (`--check-font` returns a non-zero code) | wiring it into the smoke job is an open task, not a reason to trust a shot by eye |
| No host tests; state coverage is `selftest` only | you own the verification: extend the smoke driver or add a `SELFTEST` routine (the M6b suite is `src/user/selftest.rs`) |
| Threads are a native MVP: `clone` with explicit entry, no TLS/`CLONE_SETTLS`, no join/waitid, no priorities or PI futexes, max 14 live threads per process | libc's `clone` wrapper cannot be mapped one-to-one; document the native signature and keep the futex table static (32 waiters / 16 addresses) rather than growing it silently |
| Processes are an MVP: `fork` deep-copies, sockets are not inherited (child gets `-ENOTSUP`), file offsets are copied rather than shared, `SIGCHLD` has no child-exit source, no init/reparenting (the foreground runner kills orphans), `MAX_PROCESSES = 6` | do not describe this as POSIX process semantics; wait status is `code<<8` or signal in low 7 bits; a new process resource belongs in `Process` (or a side table) and must be released by `run_source`, `wait4` and `cleanup_orphans` |
| Signals are a documented subset: no `sigqueue`/`sival_ptr`/queued signals, no SIGFPE producer, no tty/process groups, `SA_RESTART` only for `futex`/`nanosleep` (full timeout again), `SIGCHLD` has no child-exit source (si_status 0), no cross-process `kill`, and `kill(0, SIGSTOP)` can park a whole process | the frame/`siginfo_t`/`ucontext_t` layouts are the ABI (see invariant 41) and must move with `docs/architecture.md` + `userland/abi.rs`; do not claim Linux signal semantics, and add queueing/tty/process-group state as a real design change rather than a flag |
| 20 compiler warnings remain (unused diagnostic helpers, `trap_entry as usize`, …) | do not add more; warning-cleanup PRs are welcome |
| No `LICENSE` file although `Cargo.toml` says MIT | do not add a license your work does not own |
| `src/memory/paging.rs` owns the kernel root and creates process roots; `src/arch/riscv64/mmu.rs` is encoding only | do not map over the kernel image or drop the identity map while a device holds a physical address; `free_user_space` frees table frames only, the process teardown frees the leaves first |
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
31. **The fd table is per process, not per kernel.** `src/user/syscall.rs` has
    `FdTables` with one 16-entry row per process slot, reset by `run_source`
    before spawning and released after the last thread is gone; 0/1/2 are the
    console, 3.. are `Arc<dyn VfsNode>` + offset + flags, and at most one is
    `FdEntry::Socket { id, connected }` (the stack owns one TCP connection; a
    second `socket()` is `-ENOSPC`). `fork` copies the row: file/pipe entries
    clone (a pipe ring gains one reference), a socket becomes
    `FdEntry::Unsupported` because the kernel owns exactly one connection;
    file offsets are copied, not shared. `execve` keeps the row. Never hold its
    lock across a VFS, block or network call (the block driver polls and may
    `wfi`; `tcp::close` sends a segment): copy the `Arc`/offset/handle out, do
    the I/O, publish afterwards. `release_fds` collects pipe references and the
    socket flag under the lock, releases the pipes and closes the socket
    outside it, and a node or connection must not outlive the process.
32. **The user heap is exactly `0x2000_0000..0x2010_0000` (1 MiB).** `brk` maps
    zeroed PMM frames on demand through `paging::map_in(proc.root, …, Flags::user_rw())`,
    records them in `process::HEAPS` (not inline in `Process`, which must stay
    small enough to move through a 16 KiB kernel stack), and rolls a partial
    growth back on `-ENOMEM`. The runner/`wait4`/`execve` release them after
    `reap`/before the root is freed; the selftest's PMM ledger is the check. A
    new mapping in this window, or an image placed there, is `-ENOMEM`, not an
    overlap.
33. **`read(0)` never blocks.** It tries `uart::try_get_byte()` then
    `virtio_input::try_key()` and returns `-EAGAIN` when both are empty;
    libpagh retries. A blocking read in the trap path would freeze the cursor,
    the keyboard and (because the shell is not polling) the network. TCP
    `recv` follows the same contract: one `net::poll()` and `-EAGAIN`, never a
    sleep waiting for a segment. A blocking UDP `recvfrom` is the deliberate
    exception: it polls with interrupts enabled under a tick budget, and
    `MSG_DONTWAIT`/`SOCK_NONBLOCK` restores the one-poll `-EAGAIN` shape.
34. **The package gate runs before the package parser.** `pkg install` streams
    `<repo>/<url>` into `/mnt/pkg/<name>.ppkg` in 4 KiB chunks and hashes the same
    bytes. The byte count may never exceed the index's `size`; at EOF it must
    equal it and the SHA-256 must match, or the download is removed and nothing
    under `/mnt/bin` is touched. Only after that gate is `PPKG0001` parsed and a
    payload copied to `/mnt/bin`; the manifest is written last. Never reorder
    this into "parse while downloading", "install then verify", or a whole-package
    heap buffer. `pkg` does not use the kernel's TLS client, and there is no
    OpenPGP and no apt: do not describe the SHA-256 as authentication, and do not
    call `pkg` an apt replacement.

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
    and `fchmodat` only fills the per-process `ModeTable` that `fstat` reads —
    not the ext2 inode, not an access check, gone with the process (and not
    inherited by `fork`; `-ENOTSUP` on a
    synthetic node, `-ENOSPC` past 16 overrides). A non-empty `rmdir` is
    `-ENOTEMPTY` (ext2 reports it as `AlreadyExists` internally; translate it),
    and a cross-mount `linkat`/`renameat2` is `-ENOTSUP`. Do not add path
    mutation anywhere else (the shell keeps using the VFS directly); the
    `selftest` run through `userland/abi.elf` is the evidence.

36. **The socket ABI is one active TCP client, one listener plus per-port UDP
    queues, and every wait must pump the NIC.** `socket` accepts
    `AF_INET`/protocol 0 with `SOCK_STREAM` (one *active* client fd; a second is
    `-ENOSPC`; listeners and accepted `TcpConn` descriptors are separate kinds)
    or `SOCK_DGRAM` (`SOCK_NONBLOCK` optional). `FdEntry::Socket`/`Listener`/
    `TcpConn`/`UdpSocket` live in the per-process table; stream
    `read`/`write`/`close` route to `tcp::recv_nonblocking_id`/`send_id`/
    `close_id`, UDP `read`/`poll`/`close` to the port queue, and
    `bind`/`listen`/`accept`/`sendto`/`recvfrom` are the socket paths. A user
    program runs while the shell sits in `wait_for_process`, so **nothing else
    polls the NIC**: `connect`, `send`, a blocking `recvfrom` and a blocking
    `accept` run with interrupts enabled for the wait (the same deliberate
    exception `nanosleep` makes, with no lock held) so ticks and preemption still
    work, and each wait loop calls `net::poll()` every iteration. TCP `recv` is
    non-blocking by contract; `accept` blocks for at most 30 s and answers
    `-EAGAIN` (immediately for `SOCK_NONBLOCK`); UDP `recvfrom` blocks only when
    the descriptor is blocking and `MSG_DONTWAIT` is absent, and even then the
    wait is bounded (it answers `-EAGAIN` after ~10 s). Every `sockaddr` goes
    through `copy_from_user`; the length (exactly 16), family, port
    (big-endian) and address are validated before any state changes. The TCP
    table is 8 slots, the single listener has a 4-entry queue and `listen`
    clamps the backlog to `1..=4`; the UDP binding table is static (8 ports, 4
    datagrams each), the queue drops the oldest datagram when full, and no lock
    is held across `copy_to_user` or a NIC poll. IPv4 literals only: there is
    no DNS syscall, no UDP `connect`, no IPv6 socket, and no TLS socket option
    (TLS is the separate `tls_*` 1100-1103 family over a connected stream). The live
    evidence is `tools/test_sockets.py` / `docs/live-sockets.txt` (TCP client),
    `tools/test_tcp_server.py` / `docs/live-tcp-server.txt` (TCP server) and
    `tools/test_udp.py` / `docs/live-udp.txt` (UDP); the CI-safe evidence is the
    always-running bad-arg half of `userland/abi.elf` (including the
    bind/listen/nonblocking-accept section) plus its `SKIP` when
    `10.0.2.2:8000` is closed.
37. **The process, not the thread, owns the address space.** `Process` is created
    by `run_source`/`fork`, holds `root/satp/asid`, fd row, heap/mmap tracking and
    the futex pid, and is released only after the last thread is `Exited` and
    reaped. A thread's exit only decrements `Process::threads` (and records the
    main thread's code); it never tears down fd/heap/mmap. `exit_group` and
    `on_fault` retire survivors with `scheduler::kill_group` and zero the
    counter; the runner or `wait4` then does the one teardown. A new field that a
    thread may touch while another runs belongs in `Process`, not in `Task`.
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
40. **`exit` is a thread exit; `exit_group` is a process exit.** `exit`
    decrements `Process::threads`, keeps the main thread's code and discards a
    non-main code; the last thread stores the result and wakes the parent's
    `wait4`. `exit_group` retires every thread and uses the main thread's code if
    it already exited, otherwise the caller's. A fault is an `Exit::Fault` for
    the whole process. Do not make `exit` call `kill_group`, and do not let a
    thread return to U-mode after either call.

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
    an explicit restart contract. The action table is per **process**:
    `fork_process` copies handlers, blocked mask and altstack settings (not
    pending bits); `exec_process` sets every handler to `SIG_DFL` — including
    `SIG_IGN`, a documented divergence — disables the alternate stack and drops
    pending/frames while keeping the blocked mask.
42. **A process is one root, one ASID and one fd row; its children are its own.**
    `Process.root/satp/asid` are the address-space identity, `Task::satp` must
    match the process of the running thread, and `install_satp` is the only
    writer before a switch (`set_current_satp` updates it after `execve`).
    `fork` deep-copies image/heap/mmap/stack into a fresh root, copies the fd
    row (pipes +1 reference, sockets `-EINVAL`/`-ENOTSUP`), copies handlers but
    not pending, and returns 0 in the child with `sepc+4`; the parent gets the
    pid. `execve` builds the new root/ELF/stack **before** switching `satp`,
    retires sibling threads, frees the old image/heap/mmap/stack/root, keeps
    pid/ppid/fd/blocked and resets `wait_tid`, then rewrites the trap frame
    (`a0=argc`, `a1=argv`, no `sepc` bump). `wait4` reaps only a
    `threads == 0 && ready` child of the caller, status `code<<8` or signal in
    the low 7 bits; `SIGCHLD` is not generated. The foreground runner kills and
    reaps every survivor (`cleanup_orphans`) before `run_source` returns, so a
    released process root is never still installed and a zombie never outlives
    the command. Do not free `Process.root` while any of its threads is live or
    while the hart is running on it.
43. **The user stack grows one page at a time and stops at a real guard.**
    `src/user/stack.rs::try_grow` is the single growth path and it runs from
    `trap_dispatch` **before** `signal::deliver_fault`, for load/store page
    faults only, only below the task's current bottom (`Process::stack_low`),
    only at or above `USER_STACK_FLOOR = USER_TOP - USER_STACK_MAX_SIZE` (1 MiB
    per process), and only within `FAULT_SLACK = 64 KiB` of the interrupted user
    `sp`. It verifies the whole `page..stack_low` span is unmapped **before** it
    allocates (an image `PT_LOAD` may legally sit in that window and must never
    be overwritten), zeroes every frame through the identity map (a stack page
    must not leak a previous tenant), maps it `user_rw()`, and leaves `sepc`
    alone so the epilogue retries the instruction. A refusal is not a kernel
    error: the fault keeps its pre-growth meaning (`SIGSEGV`/`on_fault`), which
    is what makes recursion past 1 MiB die cleanly on the page below the floor.
    `stack_low..USER_STACK_BASE` is exactly the set of pages this module mapped:
    `fork` copies that span (`copy_grown_stack`), `execve` resets `stack_low`,
    and the one teardown (`release_process_memory`) calls `stack::release`
    before the page tables go. Do not move the check after the first allocation,
    do not map an instruction-fetch fault, and do not grow past the floor.
44. **TLS is fail-closed, and there is no flag that says otherwise.**
    `net::tls::user_open` (syscall 1100) runs the *same* `KernelProvider`/
    `KernelVerifier` as the shell's `https`: chain to the committed CA bundle, SAN
    authorization of the requested name, the clock gate, `CertificateVerify`, and
    the independent `VERIFIED_HANDSHAKES` counter check. `tls_connect` answers
    `-EINVAL` for any non-zero `flags`, so an "insecure"/"skip verify" bit cannot
    be added without changing the documented ABI — keep it that way. A refused
    handshake abandons the connection (RFC 8446 §6.1): the fd becomes a tombstone,
    and a TCP close *without* `close_notify` must surface as `-ECONNRESET`, never
    as end-of-file.
45. **A userland TLS session is self-referential and table-owned.**
    `UserSession` (`src/net/tls.rs`) holds a `TlsConnection<'static, ..>` that
    borrows the heap blocks of its own `read_buf`/`write_buf`: `conn` is declared
    **first** (fields drop in declaration order) and the buffers are **boxed**
    (their addresses do not move with the struct). Do not reorder those fields,
    unbox them, or add a `Drop`. A blocking call *takes the session out* of
    `USER_SESSIONS` and puts it back afterwards — the table lock is a spinlock and
    `block_on` sleeps on the timer tick, so holding it across a handshake would
    freeze the tick that makes progress. `put_session` must drop a session whose
    slot was released meanwhile, never resurrect it; the slot's `used` bit stays
    set while a call is in flight, which is what turns a concurrent second call
    into `-EAGAIN`.
