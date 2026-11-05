# libpagh signal API — intended surface (report, not implemented here)

This is the `native-sig2` workstream's report for the separate `pagh-api`
repository. **No file in `pagh-api` was modified.** It lists what `libpagh`
must add or change to match the kernel (`src/user/signal.rs`,
`pagh-api/docs/abi.md`). All layouts and numbers are frozen by the kernel's
`const _` assertions and `docs/architecture.md` §9.3.

## 1. Syscalls and constants (`libpagh/src/sys.rs`)

```rust
pub const SYS_SIGALTSTACK: usize = 132;

pub const SA_SIGINFO: usize = 0x0000_0004;
pub const SA_RESTORER: usize = 0x0400_0000;
pub const SA_ONSTACK: usize = 0x0800_0000;
pub const SA_RESTART: usize = 0x1000_0000;
pub const SA_NODEFER: usize = 0x4000_0000;
pub const SA_RESETHAND: usize = 0x8000_0000;

pub const SS_ONSTACK: i32 = 1;
pub const SS_DISABLE: i32 = 2;
pub const MINSIGSTKSZ: usize = 4096;

pub const SI_USER: i32 = 0;
pub const SI_KERNEL: i32 = 0x80;
pub const SI_TKILL: i32 = -6;
pub const SEGV_MAPERR: i32 = 1;
pub const BUS_ADRALN: i32 = 1;
pub const TRAP_BRKPT: i32 = 1;
```

`SIGSTOP = 19`, `SIGCONT = 18` already exist; the doc comment “job control is
not implemented” must be replaced by “default action stops the thread; SIGCONT
resumes”. Add `SIGTSTP = 20`, `SIGTTIN = 21`, `SIGTTOU = 22` if libpagh wants
to name them. `SIGKILL`/`SIGSTOP` remain uncatchable.

## 2. `SigAltStack` (`libpagh/src/abi.rs`)

24 bytes, pagh layout (not Linux's `stack_t`):

```rust
#[repr(C)]
pub struct SigAltStack {
    pub ss_sp: usize,
    pub ss_flags: i32,
    pub _pad: i32,
    pub ss_size: usize,
}
const _: () = assert!(core::mem::size_of::<SigAltStack>() == 24);
```

Wrapper suggestion (both pointers optional, like `rt_sigprocmask`):

```rust
pub fn sigaltstack(new: Option<&SigAltStack>, old: Option<&mut SigAltStack>) -> Result<()>;
```

The buffer must stay mapped `U|R|W` while installed; the kernel validates every
page at install and again at delivery. `MINSIGSTKSZ = 4096` is the minimum
accepted size. The stack is per thread, so a new pthread must call
`sigaltstack` itself if it wants `SA_ONSTACK`.

## 3. `siginfo_t` and `ucontext_t`

`SigInfo` (128 B) and `UContext` (288 B) are pagh structures, not Linux's:

```rust
#[repr(C)]
pub struct SigInfo {
    pub si_signo: i32,
    pub si_errno: i32,
    pub si_code: i32,
    pub _pad: i32,
    pub si_value: u64,   // si_pid | si_uid << 32, or si_addr
    pub si_status: i32,  // SIGCHLD child status; 0 without fork/wait
    pub _pad2: i32,
    pub _rest: [u8; 96],
}

#[repr(C)]
pub struct MContext {
    pub gregs: [u64; 32], // x0..x31; x2 = interrupted sp
    pub pc: u64,          // interrupted sepc
}

#[repr(C)]
pub struct UContext {
    pub uc_flags: u64,
    pub uc_link: u64,
    pub uc_sigmask: u64,
    pub mcontext: MContext,
}
const _: () = assert!(core::mem::size_of::<SigInfo>() == 128);
const _: () = assert!(core::mem::size_of::<MContext>() == 264);
const _: () = assert!(core::mem::size_of::<UContext>() == 288);
```

Recommended accessors on `SigInfo`: `pid()`, `uid()`, `addr()` (union slot at
offset 16), and a `status()` reader. `UContext::uc_sigmask` is the mask in force
inside the handler; `mcontext` is a read-only snapshot (kernel `rt_sigreturn`
restores its own saved `TrapFrame`, not user edits to this struct).

## 4. `SigAction` and handler ABI

`SigAction` (32 B) stays `{ handler, flags, mask, restorer }`. Update its doc:
the accepted flags are `SA_RESTORER | SA_ONSTACK | SA_RESTART | SA_NODEFER |
SA_RESETHAND | SA_SIGINFO`; `SA_RESTORER` and a user-executable restorer are
still required for every real handler.

Handler types:

```rust
pub type Handler = extern "C" fn(i32, *const SigInfo);
pub type SigInfoHandler = extern "C" fn(i32, *const SigInfo, *const UContext);
```

Without `SA_SIGINFO` the kernel keeps the existing two-argument convention
(`a2 = 0`). With it, `a2` points at the `UContext` inside the signal frame.
The restorer assembly is unchanged (`ecall 139`).

`rt_sigaction` currently takes `action: &SigAction`, so it cannot express a
pure query. Suggested signature (source-breaking, but the ABI is still
pre-1.0 userland):

```rust
pub fn rt_sigaction(signum: i32, action: Option<&SigAction>, old: Option<&mut SigAction>)
    -> Result<()>;
```

## 5. Behaviour libpagh must document

* **Delivery.** At the next kernel→user boundary (syscall return or trap from
  U-mode, including a timer tick). A thread blocked in `futex`/`nanosleep`
  sees `-EINTR`; with `SA_RESTART` the kernel re-enters the same `ecall` after
  the handler, with the **full** original timeout/sleep, not the remainder.
  `read(0)` remains non-blocking (`-EAGAIN`).
* **Masks.** Signal `n` is bit `n-1`; SIGKILL/SIGSTOP bits are dropped.
  Without `SA_NODEFER` the delivered signal is blocked inside the handler;
  `sa_mask` is added; `SA_RESETHAND` resets the disposition to `SIG_DFL` on
  entry.
* **Job control.** `kill(tid, SIGSTOP|SIGTSTP|SIGTTIN|SIGTTOU)` with the
  default action parks that thread in the scheduler; `SIGCONT` resumes it and
  clears a pending stop. `kill(0, SIGSTOP)` stops every thread of the run and
  there is no tty/process-group `SIGCONT`, so do not offer it as a recoverable
  operation. A stopped thread still answers `kill(tid, 0)`.
* **`siginfo_t` sources.** `kill` → `SI_USER` with sender pid/uid (uid always
  0); `tgkill` → `SI_TKILL`; page fault → SIGSEGV `SEGV_MAPERR`; illegal
  instruction → SIGILL `SI_KERNEL`; misaligned load/store → SIGBUS
  `BUS_ADRALN`; breakpoint → SIGTRAP `TRAP_BRKPT`. SIGFPE has no hardware
  source. `SIGCHLD` can only be sent explicitly today: `si_status` is 0 until
  the `fork`/`wait` workstream starts generating child-exit signals (the field
  is already reserved at offset 24).
* **Limits.** No `sigqueue`, `sival_ptr`, queued signals or per-signal sender
  queue: one pending bit per signal, last sender wins. No tty job control, no
  `SIGTSTP` from a terminal, no `sigwait`/`sigpending` syscall.
