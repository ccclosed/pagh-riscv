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
```

`./run.sh` boots the kernel interactively (QEMU `virt` + OpenSBI, serial on stdio;
`Ctrl-A X` to leave). It always passes `-device ramfb`, so the screen exists too:
off-screen by default (`-display none`, which is what lets `screendump` work
headless), in a QEMU window with `./run.sh --window`. `PROFILE=release`, `MEMORY=…`,
`BIOS=…` cover the variants, and `DISK=/tmp/pagh.img` attaches the virtio-mmio
disk (a missing image is created blank and the kernel formats it on boot); what
`run.sh` does not take as an argument it takes from the environment, and an
unknown argument is rejected, not ignored. Two in-guest checks that are not part of CI:

```sh
# at the `pagh:rv64>` prompt
selftest                                        # in-QEMU suites: 7 mem + 3 block + 11 fs + 4 arch/timer + 5 task + ledgers
# from the host
python3 tools/run.py --keys "help\n" --shot /tmp/shot.png --check-font --scan-all
python3 tools/run.py --disk /tmp/pagh.img --keys "ls /mnt\n" --shot /tmp/fs.png
```

`rustup target add riscv64gc-unknown-none-elf` once; the nightly is pinned in
`rust-toolchain.toml`.

CI (`.github/workflows/ci.yml`) runs the commands above — the smoke job covers the
four QEMU modes, including the two-run disk acceptance — and reads the toolchain pin
out of `rust-toolchain.toml`. There are **no** other gates yet: no `tools/build.py`, no
`tools/check_safety.py`, no host-test runner, no E2E driver (those exist in the x86
repository and were deliberately not carried over). If you add a gate, add it to CI
*and* to this block in the same commit.

## Canonical entry points

One role, one current path — the one CI, the docs and a fresh checkout should use.
Unlike the x86 tree there are **no `-legacy` companions** here: the Windows launcher
(`run.cmd`) and the PowerShell E2E harnesses were not ported, so do not add a second
path "for parity".

```text
kernel-build    -> cargo (target from .cargo/config.toml, linker.ld, -Tlinker.ld)
kernel-run      -> run.sh                  # Linux/QEMU launcher; ramfb on, --window for a window
kernel-screen   -> tools/run.py            # boots with ramfb, screendump, font check
kernel-smoke    -> .github/smoke.py        # headless QEMU driver used by CI (serial/ramfb/disk)
kernel-selftest -> the shell's `selftest`  # in-QEMU memory + block + filesystem checks
kernel-halt     -> SBI SRST via the shell  # makes QEMU exit 0
```

## Repo layout

- `src/` — the kernel (crate `pagh`, **binary**, target `riscv64gc-unknown-none-elf`,
  `[dependencies]` empty). Modules: `main.rs` (boot phases, global allocator, panic
  handler), `arch/riscv64/{boot,csr,sbi,traps,mmu}.rs`, `fdt.rs`,
  `memory/{pmm,heap,selftest}.rs` (+ `src/memory/README.md`),
  `drivers/{uart,fw_cfg,ramfb,framebuffer,block,virtio_mmio,virtio_net,selftest}.rs`
  (+ `src/drivers/README.md`), `fs/{ext2,format_policy,selftest}.rs`
  (+ `src/fs/README.md`), `vfs/mod.rs` (+ `src/vfs/README.md`),
  `task/{mod,scheduler,selftest}.rs` (+ `src/task/README.md`),
  `user/{mod,elf,syscall,selftest}.rs` (+ `src/user/README.md`),
  `net/{mod,wire,dhcp,dns,tcp,selftest}.rs` (+ `src/net/README.md`), `console.rs`,
  `shell.rs`, `sync.rs`.
- `userland/` — three static RV64 fixture programs (`hello`, `fault`, `spin`),
  their linker script and `build.sh` (+ `userland/README.md`). The ELFs are
  committed and embedded with `include_bytes!`; a rebuild must be committed with
  the sources in the same commit.
- `docs/` — architecture, roadmap, screenshots (`shell.png`, `fs.png`, `user.png`,
  `net.png`, …).
- `assets/font8x18.bin` — the 8×18 coverage font (generated by `tools/gen_font.py`).
- `tools/` — `run.py` (boot + screendump + font check), `gen_font.py`, `font_check.py`.
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
   raises it), and M5 adds two more: the block driver after the map and traps (it
   reads MMIO), the filesystem after the driver and the heap (mounting allocates).
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
    `sepc` after every returning syscall; `exit` never returns (it calls
    `scheduler::exit`). `sret` resumes exactly at `sepc`, so without the bump a
    program loops on the same `ecall` forever — this actually happened to `hello`
    and `fault` before the fix. The trap frame's `sepc` field is part of the
    contract; change the bump and the ABI together.
20. **`sscratch` is a two-state invariant.** While S-mode runs, `sscratch == 0`
    (`traps::init` writes it); before `sret` into U-mode, `enter_user` stores the
    task's kernel stack pointer there. `trap_entry` starts with
    `csrrw sp, sscratch, sp`: a nonzero `sp` means "came from U-mode", the user
    `sp` goes to frame slot 8 and `sscratch` is cleared; zero is an ordinary
    S-mode trap. The epilogue splits on the **saved** `sstatus.SPP` and, for
    U-mode, re-arms `sscratch` with `frame_base + 264` before restoring the user
    `sp`. Do not read/write `sscratch` outside `traps.rs`/`user/mod.rs`; there is
    exactly one `sret` into U-mode.
21. **`enter_user` builds `sstatus` in Rust.** `SIE=0, SPP=0, SPIE=1, SUM=0` via
    `csrw sstatus`, not `csrci`/`csrsi`: SPIE (bit 5) and SPP (bit 8) do not fit
    the 5-bit CSR immediate, and the earlier `csrci 8`/`csrsi 5` silently left
    them wrong (the machine still booted, which is how it hid). `sret` then enters
    U-mode with interrupts on and supervisor access to user pages off.
22. **U-mode faults kill the task, never the hart.** Any synchronous exception
    with saved `SPP == 0` (`ecall` excepted) goes to `user::on_fault`, which
    prints one `[user] <name> killed: …` line and calls `scheduler::exit`; the
    runner releases pages and frames afterwards. S-mode exceptions keep the
    print-and-park contract. A user program's bad pointer must be `-EFAULT` via
    PTE validation (`U=1` and `R=1` for every page, `SUM` only around the copy),
    not an exception.
23. **`RUN` is not held across blocking work.** `run_image` claims the one user
    program (`Busy` otherwise) and releases it after the pages are gone; the
    wait loop (`yield_now`) and the trap paths only read/update short sections.
    One address space, one ASID, one program at a time: there is no per-process
    `satp` and no isolation between two user programs.
24. **virtio-net's header is a contract, not a guess.** The driver negotiates
    `VIRTIO_F_VERSION_1` + `VIRTIO_NET_F_MRG_RXBUF`, so every buffer starts with
    a **12-byte** `virtio_net_hdr` (the 10-byte base plus `num_buffers`). A
    10-byte header shifts every captured frame by two bytes — the first version
    did exactly that, visible only in a QEMU `filter-dump`. RX buffers are
    2 KiB (one frame each), posted ahead of time and re-posted on completion; TX
    is one descriptor with header+frame contiguous and a bounded used-ring poll;
    `NO_INTERRUPT` means the device never raises an IRQ. Device addresses are
    Rust pointers because RAM is identity-mapped (`memory::paging`).
25. **The boot stack is 64 KiB; big state belongs on the heap.** `VirtioNet`
    initially held a 12 KiB frame ring inline. Building and moving it on the boot
    stack in a debug build overflowed into `.bss` and corrupted the allocator's
    state — the symptom was a heap panic (`chunk_count = 16384`), not a stack
    message. Box large buffers in device/stack structs, and keep `probe`-style
    builders returning small values.
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

What exists: the in-QEMU suites (shell `selftest`: 7 memory + 3 block + 11 ext2/VFS
+ 4 interrupt/timer + 5 scheduler + 7 user-mode + 11 network checks, idempotent,
each group prints a free-frame ledger), the headless QEMU smoke driver in five
modes (serial, ramfb, disk-fresh, disk-existing, `--net`) including the M5
format/mount/persist acceptance and the M7 DHCP/ping acceptance, and the pixel
check that proves a screenshot is really the committed font. The network
acceptance is deterministic: QEMU's slirp answers DHCP and ICMP to `10.0.2.2`
without touching the runner's internet. The user-mode suite is the M6b evidence: ELF accept/reject matrix, round
trip, `Exit::Fault`, a run after the fault, and the timer-in-U counter
(`traps::user_timer_ticks()`) moved by the `spin` fixture. `run demo` must print
`run: demo exited with code 42`, `run fault` must be killed with cause 13 while
the shell survives, `run spin` must exit 7. What does not: host property tests, an E2E driver,
adversarial fuzzing. That is a gap, not a policy — the rule stands:

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

Semver `MAJOR.MINOR.PATCH` in `Cargo.toml [package].version` — currently `0.6.0`,
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
  DNS, ICMP, a TCP client). The same rule applies to both: the tag equals the
  manifest and the `Cargo.lock` entry, and the release is not optional.

## Known gaps (documented, not hidden)

The authoritative list is `README.md` → «Чего нет», with per-milestone criteria in
`docs/roadmap.md`. The ones that bite an agent most often:

| Gap | Consequence for your change |
|---|---|
| Sv39 is on with an **identity** map, so a physical address is still a pointer | any address handed to a device (`fw_cfg`, ramfb) relies on that identity; a non-identity or per-process map breaks it silently |
| The tick is 100 Hz and preemption happens only on it (no priorities/deadline scheduler) | scheduling policy is a scheduler change; `timer::set_tick_handler` is the seam, not a policy |
| No TLS/apt: the network stack speaks plain HTTP | apt stays deliberately unwired until the vendored-TLS + OpenPGP milestone lands; “download and install without verification” is what the x86 fail-closed chain exists to prevent |
| TCP is one active-open client (HTTP/1.0), not a general stack | no listening sockets, out-of-order reassembly, congestion control or multiple connections |
| The NIC is polled and the stack is synchronous | network progress happens while a command or the shell's idle loop polls; an IRQ-driven NIC is a later milestone |
| Interrupt handlers cannot allocate or print | put work in normal context; bump a counter and read it from `time`/`selftest` |
| One address space, one ASID: user pages carry `U=1` in the kernel's single Sv39 tree | two user programs are not isolated from each other; no per-process `satp`, no `brk`/`mmap`, no `fork`/`exec`/signals; `run <path>` reads one 64 KiB heap chunk at most |
| The filesystem has **no journal** (M5 deliberately skipped the x86 WAL) | a power cut between two writes can leave an operation half-applied; do not claim crash consistency |
| One block device slot, no partitions/hot-plug | a second disk is a design change, not a registration |
| Formatting is the only destructive operation and is gated by `format_policy` | never format "to recover": a failed mount is not permission to erase |
| No S-mode page-fault recovery | a kernel bad access halts (`[EXC] … halting`); a *user* fault ends that task instead (`user::on_fault`) |
| Screenshot pixels are not gated in CI yet (`--check-font` returns a non-zero code) | wiring it into the smoke job is an open task, not a reason to trust a shot by eye |
| No host tests; state coverage is `selftest` only | you own the verification: extend the smoke driver or add a `SELFTEST` routine (the M6b suite is `src/user/selftest.rs`) |
| 15 compiler warnings remain (unused diagnostic helpers, `trap_entry as usize`, …) | do not add more; warning-cleanup PRs are welcome |
| No `LICENSE` file although `Cargo.toml` says MIT | do not add a license your work does not own |
| `src/memory/paging.rs` owns the only page table; `src/arch/riscv64/mmu.rs` is encoding only | do not map over the kernel image or drop the identity map while a device holds a physical address |