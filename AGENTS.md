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
  `drivers/{uart,fw_cfg,ramfb,framebuffer,block,virtio_mmio,selftest}.rs`
  (+ `src/drivers/README.md`), `fs/{ext2,format_policy,selftest}.rs`
  (+ `src/fs/README.md`), `vfs/mod.rs` (+ `src/vfs/README.md`),
  `task/{mod,scheduler,selftest}.rs` (+ `src/task/README.md`), `console.rs`,
  `shell.rs`, `sync.rs`.
- `docs/` — architecture, roadmap, screenshots (`shell.png`, `fs.png`, …).
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

## Testing philosophy

What exists: the in-QEMU suites (shell `selftest`: 7 memory + 3 block + 11 ext2/VFS
+ 4 interrupt/timer + 5 scheduler checks, idempotent, each group prints a
free-frame ledger), the headless QEMU smoke
driver in four modes (serial, ramfb, disk-fresh, disk-existing) including the M5
format/mount/persist acceptance, and the pixel check that proves a screenshot is
really the committed font. What does not: host property tests, an E2E driver,
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

Semver `MAJOR.MINOR.PATCH` in `Cargo.toml [package].version` — currently `0.3.0`,
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
- **Tags** — tag the release commit with the bare version string (`0.3.0`) and push
  it. Tags must never run ahead of `Cargo.toml`. Note: `0.2.0` was tagged on a commit
  whose manifest still said `0.1.0` (the drift this rule exists to prevent); it is
  left as history, and `0.4.0` is the first tag that matches its manifest (M3/M5/M6a landed after `0.3.0` was written but before it was tagged).

## Known gaps (documented, not hidden)

The authoritative list is `README.md` → «Чего нет», with per-milestone criteria in
`docs/roadmap.md`. The ones that bite an agent most often:

| Gap | Consequence for your change |
|---|---|
| Sv39 is on with an **identity** map, so a physical address is still a pointer | any address handed to a device (`fw_cfg`, ramfb) relies on that identity; a non-identity or per-process map breaks it silently |
| A 100 Hz tick exists, but nothing is scheduled on it yet (M6) | preemption is a scheduler change; `timer::set_tick_handler` is the seam, not a scheduler |
| Interrupt handlers cannot allocate or print | put work in normal context; bump a counter and read it from `time`/`selftest` |
| No user mode, no syscalls, no ELF loader yet (M6b) | kernel threads only; `spawn(fn)` is the seam a user-program trampoline will use |
| The filesystem has **no journal** (M5 deliberately skipped the x86 WAL) | a power cut between two writes can leave an operation half-applied; do not claim crash consistency |
| One block device slot, no partitions/hot-plug | a second disk is a design change, not a registration |
| Formatting is the only destructive operation and is gated by `format_policy` | never format "to recover": a failed mount is not permission to erase |
| No page-fault recovery | a bad access halts the machine; expect `[EXC] … halting` |
| Screenshot pixels are not gated in CI yet (`--check-font` returns a non-zero code) | wiring it into the smoke job is an open task, not a reason to trust a shot by eye |
| No host tests; state coverage is `selftest` only | you own the verification: extend the smoke driver or add a `SELFTEST` routine |
| 6 compiler warnings remain (5 unused CSR helpers, `trap_entry as usize`) | do not add more; warning-cleanup PRs are welcome |
| No `LICENSE` file although `Cargo.toml` says MIT | do not add a license your work does not own |
| `src/memory/paging.rs` owns the only page table; `src/arch/riscv64/mmu.rs` is encoding only | do not map over the kernel image or drop the identity map while a device holds a physical address |