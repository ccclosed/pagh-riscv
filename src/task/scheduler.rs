//! The round-robin scheduler: kernel threads, their stacks and the switch.
//!
//! Scope of this milestone: *kernel* threads only. There is no user mode, no
//! address-space switch, no `sscratch` stack and no `Blocked` state yet (M6b);
//! a thread owns a PMM stack plus a saved [`TaskContext`](super::super::arch::riscv64::switch::TaskContext)
//! and runs until it yields, is preempted by a tick, or returns from its entry.
//!
//! # What is in the table
//!
//! [`MAX_TASKS`] slots live in a static array, so a switch never allocates and a
//! `TaskContext` pointer never moves. Slot 0 is the boot thread — the one that
//! runs `kmain`, and later the shell — slot 1 is the idle thread, slots 2.. are
//! handed out to `spawn`. The shell thread participates in round-robin like any
//! other: `yield`/tick from the shell puts it back in the queue, which is what
//! lets a demo thread run while the shell is waiting on the UART.
//!
//! # States and the ready queue
//!
//! [`State`] is `Free` (slot unused), `Ready` (queued), `Running` (the current
//! thread) and `Exited` (returned from its entry; its stack is reaped next time
//! a scheduler entry runs). The ready queue is a ring of slot indices:
//! push-back on switch-out, pop-front on switch-in. No allocation, no intrusive
//! list, and the queue holds no pointer that could dangle when a slot is reused.
//!
//! # Lock discipline (single hart)
//!
//! One global [`Spinlock`] guards the ready queue and the counters. Every entry
//! into the scheduler masks interrupts for the *whole* decision and the switch
//! itself ([`InterruptsOff`]): on one hart a timer tick that re-entered the
//! scheduler between the two halves of a switch would corrupt both contexts, and
//! a spinlock taken with interrupts on is a deadlock waiting for the first
//! interrupt source. That is stronger than the kernel's `csr::without_interrupts`
//! closure, which only covers the closure body — and a lock guard outlives it.
//!
//! Lock order: scheduler → PMM. A spawn takes the thread's frames between two
//! lock sections, a reap gives frames back after releasing the lock, and `init`
//! takes idle's five frames under the lock exactly once; nothing takes the
//! scheduler lock while holding the PMM or the heap lock.
//!
//! # Preemption
//!
//! [`tick`] is the clock: every call advances the running thread's quantum
//! ([`QUANTUM_TICKS`]) and, when it expires, switches to the next ready thread.
//! Today the clock is *software*: the shell's UART poll loop and the demo thread
//! body call [`pump`], which issues ticks until a real one exists. The M3 branch
//! (`src/arch/riscv64/timer.rs`, `set_tick_handler(fn())`) attaches the timer
//! interrupt with exactly one line once it merges — see [`tick_from_timer`],
//! whose doc comment is the seam contract. From then on [`pump`] is a no-op and
//! preemption happens asynchronously in the trap handler, with no other change
//! here: a switch inside the handler saves the handler's context, and the
//! interrupted thread's trap frame stays on that thread's own stack.
//!
//! # Stacks
//!
//! A spawned thread's stack is [`STACK_ALLOC_FRAMES`] consecutive PMM frames:
//! one guard frame, then [`STACK_SIZE`] usable bytes. The guard frame is filled
//! with [`GUARD_FILL`] except its last word, which holds [`CANARY`]; the usable
//! area is filled with [`STACK_FILL`] so that a reaped stack reports a
//! high-water mark ([`check_stack`]). An overflow past the bottom of the stack
//! hits the canary first, and the reap that returns the frames reports it on the
//! serial line and bumps the violation counter in [`stats`].
//!
//! Everything here is `core`-only: no heap allocation happens in the spawn path,
//! in `tick`, or in the switch. *(The task selftest does not allocate either, so
//! the heap ledger is untouched by this subsystem.)*

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use crate::arch::riscv64::csr;
use crate::arch::riscv64::switch::{self, TaskContext};
use crate::memory::{pmm, FRAME_SIZE};
use crate::sync::Spinlock;

/// Task-table slots: the shell, the idle thread and six spawnable threads.
pub const MAX_TASKS: usize = 8;

/// Ticks a thread runs before the scheduler switches away from it.
pub const QUANTUM_TICKS: u32 = 8;

/// Usable bytes of a spawned thread's stack.
pub const STACK_SIZE: usize = 16 * 1024;

/// Guard frame below every stack: overflow lands here (and on the canary) first.
pub const GUARD_SIZE: usize = FRAME_SIZE;

/// Frames a spawned thread's stack costs the PMM: guard + usable.
pub const STACK_ALLOC_FRAMES: usize = (GUARD_SIZE + STACK_SIZE) / FRAME_SIZE;

/// Byte the guard frame is filled with; any other value at a non-canary byte is
/// evidence that something wrote below the stack.
const GUARD_FILL: u8 = 0xa5;

/// Byte the usable stack is filled with at spawn, so a reaped stack can report
/// how deep it actually went.
const STACK_FILL: u8 = 0xcc;

/// The canary word at the top of the guard frame (`GUARD_SIZE - 8`). An
/// overflowing stack reaches it before anything else below the stack.
const CANARY: u64 = 0x5afe_57ac_c0de_f00d;

/// The boot thread's slot: `kmain` and the shell run here, on the boot stack.
const SHELL_SLOT: usize = 0;

/// The idle thread's slot; it is the fallback, never a round-robin member.
const IDLE_SLOT: usize = 1;

/// First slot `spawn` may hand out.
const FIRST_SPAWNABLE_SLOT: usize = 2;

/// Longest name in the table, `\0`-free (`TaskName` keeps a length).
const NAME_LEN: usize = 16;

/// Pumps [`cancel_demo`] may issue while waiting for a cohort to run its exit
/// path. Each demo thread needs at most a switch and one iteration, so this is
/// three orders of magnitude of headroom; it exists so a broken thread parks the
/// suite with a FAIL instead of the machine hanging.
const QUIESCE_LIMIT: usize = 4096;

/// Why the scheduler was entered; the only difference between the paths is what
/// happens to the outgoing thread.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reason {
    /// A clock tick: run the quantum down and switch when it expires.
    Quantum,
    /// `yield_now`: switch to the next ready thread immediately.
    Yield,
    /// The thread returned from its entry: it leaves the queue for good.
    Exit,
    /// The selftest asks to run the idle thread once (it steps back on its own).
    ProbeIdle,
    /// Idle has nothing to wait for yet: give the CPU back without queueing.
    IdleStep,
}

/// Where a thread is in its life.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// The slot holds no thread: never used, or its evidence was reused.
    Free,
    /// Runnable: in the ready queue (the idle thread is ready but not queued).
    Ready,
    /// The thread the CPU is executing right now.
    Running,
    /// Returned from its entry point; waiting for the next reap to give its
    /// stack frames back to the PMM.
    Exited,
}

impl State {
    /// The word `tasks` prints; stable strings, they are grepped from serial.
    pub fn name(self) -> &'static str {
        match self {
            State::Free => "free",
            State::Ready => "ready",
            State::Running => "running",
            State::Exited => "exited",
        }
    }
}

/// A fixed-capacity thread name.
///
/// A name is a label, not state: formatting one must not allocate (a spawn is
/// allowed to allocate only the stack), so the digits of `task-<id>` are written
/// into this buffer by hand in [`TaskName::set_numbered`]. `&'static str` would
/// do for the shell and idle, but spawned ids are not known at compile time.
#[derive(Clone, Copy)]
pub struct TaskName {
    bytes: [u8; NAME_LEN],
    len: u8,
}

impl TaskName {
    /// The empty name of a `Free` slot.
    pub const fn empty() -> Self {
        TaskName {
            bytes: [0; NAME_LEN],
            len: 0,
        }
    }

    fn set(&mut self, text: &str) {
        let bytes = text.as_bytes();
        let take = bytes.len().min(NAME_LEN);
        self.bytes[..take].copy_from_slice(&bytes[..take]);
        self.len = take as u8;
    }

    /// `prefix` followed by `number` in decimal, truncated to [`NAME_LEN`].
    fn set_numbered(&mut self, prefix: &str, number: u64) {
        let mut buf = [0u8; NAME_LEN];
        let mut len = 0usize;
        for &byte in prefix.as_bytes() {
            if len == NAME_LEN {
                break;
            }
            buf[len] = byte;
            len += 1;
        }
        let mut digits = [0u8; 20];
        let mut count = 0usize;
        let mut value = number;
        if value == 0 {
            digits[0] = b'0';
            count = 1;
        } else {
            while value > 0 && count < digits.len() {
                digits[count] = b'0' + (value % 10) as u8;
                value /= 10;
                count += 1;
            }
        }
        for index in (0..count).rev() {
            if len == NAME_LEN {
                break;
            }
            buf[len] = digits[index];
            len += 1;
        }
        self.bytes = buf;
        self.len = len as u8;
    }

    /// The name as a string; a `?` for the impossible non-UTF-8 case (the bytes
    /// only ever come from ASCII literals and decimal digits).
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.len as usize]).unwrap_or("?")
    }
}

/// One task-table entry.
///
/// Public fields are deliberately not used by callers outside this module; the
/// shell and the selftest read copies through [`snapshot`] so that formatting a
/// listing can never race a scheduler state change.
struct Task {
    /// Where this thread resumes. Written by [`switch::switch`] on switch-out.
    context: TaskContext,
    /// Monotonic id: never reused, unlike the slot.
    id: u64,
    name: TaskName,
    state: State,
    /// Kernel-thread body; `None` only for the boot thread, which is already
    /// running when the scheduler is built.
    entry: Option<fn()>,
    /// First frame of the stack allocation (guard frame), `0` for the boot
    /// thread's stack (the linker's `__stack_*`, not the scheduler's to free).
    stack_base: usize,
    /// 16-byte-aligned top of the usable stack.
    stack_top: usize,
    /// Demo cohort this thread belongs to, compared against [`DEMO_EPOCH`].
    epoch: u64,
    /// Work counter, bumped by the demo body and by the selftest's worker. Atomic
    /// because the shell reads it between scheduler entries, from another context.
    counter: AtomicU64,
    /// Times this thread was switched out (guarded by the scheduler lock).
    switches: u64,
}

impl Task {
    /// The `Free` slot value; everything is present so the array needs no
    /// `Option<Task>` and a slot's address never moves.
    const fn empty() -> Task {
        Task {
            context: TaskContext::zeroed(),
            id: 0,
            name: TaskName::empty(),
            state: State::Free,
            entry: None,
            stack_base: 0,
            stack_top: 0,
            epoch: 0,
            counter: AtomicU64::new(0),
            switches: 0,
        }
    }
}

/// The task table.
///
/// A `static mut` array, not a field of [`Sched`]: contexts must not move when
/// the scheduler lock is released, and the pointer `switch` stores into has to
/// stay valid while another thread runs. Every access is through [`task`], which
/// returns a raw pointer; the lock (or single-hart ownership of the running
/// slot) is what makes the accesses mutually exclusive.
static mut TASKS: [Task; MAX_TASKS] = [const { Task::empty() }; MAX_TASKS];

/// Slot of the thread currently on the CPU.
///
/// A mirror of `Sched::current` for readers that must not take the scheduler
/// lock (a demo thread bumping its own counter, which is trivial work between
/// two scheduler entries). Updated under the lock before every switch.
static CURRENT_SLOT: AtomicUsize = AtomicUsize::new(SHELL_SLOT);

/// Whether a real (timer) tick source is attached; see [`tick_from_timer`].
static HARDWARE_CLOCK: AtomicBool = AtomicBool::new(false);

/// Current demo cohort. [`spawn_demo`] bumps it; threads of an older cohort see
/// the mismatch in [`demo_main`] and exit on their next iteration.
static DEMO_EPOCH: AtomicU64 = AtomicU64::new(0);

/// Stacks whose guard/canary was overwritten before they were reaped.
static CANARY_VIOLATIONS: AtomicU64 = AtomicU64::new(0);

/// Frames handed back by reaps, for the boot line and `tasks`.
static REAPS: AtomicU64 = AtomicU64::new(0);

/// Times the idle thread has been entered (the selftest probe reads it).
static IDLE_WAKES: AtomicU64 = AtomicU64::new(0);

/// Scheduler state that needs the lock: ready ring plus counters.
struct Sched {
    initialized: bool,
    ready: [u8; MAX_TASKS],
    ready_head: usize,
    ready_len: usize,
    next_id: u64,
    quantum_left: u32,
    ticks: u64,
    switches: u64,
}

static SCHED: Spinlock<Sched> = Spinlock::new(Sched {
    initialized: false,
    ready: [0; MAX_TASKS],
    ready_head: 0,
    ready_len: 0,
    next_id: FIRST_SPAWNABLE_SLOT as u64,
    quantum_left: QUANTUM_TICKS,
    ticks: 0,
    switches: 0,
});

/// Interrupts disabled for as long as this value lives.
///
/// [`csr::without_interrupts`] masks for the duration of a *closure*; the
/// scheduler's critical section is a lock guard plus a context switch, which
/// outlives any closure it could be written in. This guard is the lock-shaped
/// version, and it is what makes "spinlocks mask interrupts" true for this
/// module (the M3 branch needs the same treatment in `pmm`/`heap`/`uart`; see
/// `docs/architecture.md` §10.1, which the Lead owns).
struct InterruptsOff {
    was_enabled: bool,
}

impl InterruptsOff {
    fn new() -> Self {
        let was_enabled = csr::interrupts_enabled();
        csr::disable_interrupts();
        InterruptsOff { was_enabled }
    }
}

impl Drop for InterruptsOff {
    fn drop(&mut self) {
        // Runs when the critical section ends — including when this context is
        // resumed after having been switched out inside it, at which point the
        // state being restored is *this* thread's, not the switching thread's.
        if self.was_enabled {
            csr::enable_interrupts();
        }
    }
}

/// Raw pointer to a task slot.
///
/// `TASKS` is a static array, so the address is stable for the program's life;
/// the slot's *contents* change only under the scheduler lock (or while the
/// running thread owns the slot), which is the caller's obligation.
fn task(slot: usize) -> *mut Task {
    // The callers are bounded by construction; this check keeps a future caller
    // from turning a logic bug into a wild pointer in a path that must not panic.
    if slot >= MAX_TASKS {
        return core::ptr::null_mut();
    }
    // SAFETY: `addr_of_mut!` never forms a reference to the `static mut`, and
    // `slot < MAX_TASKS` means the offset stays inside the array.
    unsafe { core::ptr::addr_of_mut!(TASKS).cast::<Task>().add(slot) }
}

/// A task as the shell's `tasks` command and the selftest see it: a copy taken
/// under the scheduler lock, so nothing below can observe a half-updated task.
#[derive(Clone, Copy)]
pub struct TaskInfo {
    /// Table slot (an implementation detail, stable for a running thread).
    pub slot: usize,
    /// Monotonic thread id.
    pub id: u64,
    /// Thread name (`shell`, `idle`, `task-<id>`).
    pub name: TaskName,
    /// Life-cycle state.
    pub state: State,
    /// Demo/selftest work counter.
    pub counter: u64,
    /// Times the scheduler switched away from this thread.
    pub switches: u64,
    /// Stack allocation base, `0` for the boot thread or a reaped stack.
    pub stack_base: usize,
    /// Stack top, `0` for the boot thread.
    pub stack_top: usize,
}

impl TaskInfo {
    /// The zero value for fixed-size snapshots (the shell prints a slice of these
    /// and does not allocate one).
    pub const fn zeroed() -> TaskInfo {
        TaskInfo {
            slot: 0,
            id: 0,
            name: TaskName::empty(),
            state: State::Free,
            counter: 0,
            switches: 0,
            stack_base: 0,
            stack_top: 0,
        }
    }

    /// Usable stack bytes for this thread (`0` for the boot thread, whose stack
    /// belongs to the linker script rather than the PMM).
    pub fn stack_bytes(&self) -> usize {
        if self.stack_top > self.stack_base + GUARD_SIZE {
            STACK_SIZE
        } else {
            0
        }
    }
}

/// Counters and the current thread, for the shell's `yield` line and the tests.
#[derive(Clone, Copy)]
pub struct Stats {
    /// Clock ticks the scheduler has seen (software and timer).
    pub ticks: u64,
    /// Context switches performed.
    pub switches: u64,
    /// Stacks returned to the PMM.
    pub reaps: u64,
    /// Stacks reaped with a smashed guard/canary (must stay 0 unless a test
    /// deliberately overflows one).
    pub canary_violations: u64,
    /// Times the idle thread was entered.
    pub idle_wakes: u64,
    /// Id of the running thread.
    pub current_id: u64,
    /// Slots holding a thread (exited ones included, as evidence).
    pub live_tasks: usize,
    /// Threads in the ready queue right now.
    pub ready_tasks: usize,
    /// Whether the M3 timer has attached itself as the clock.
    pub hardware_clock: bool,
}

/// What [`check_stack`] found in a thread's stack allocation.
#[derive(Clone, Copy)]
pub struct StackHealth {
    /// Usable bytes ([`STACK_SIZE`] for a spawned thread).
    pub usable: usize,
    /// Deepest byte the stack grew to, measured against the fill pattern.
    pub used: usize,
    /// The canary word was not overwritten.
    pub canary_ok: bool,
    /// Every non-canary guard byte still holds [`GUARD_FILL`].
    pub guard_ok: bool,
}

/// Build the scheduler: take the boot thread as slot 0, allocate the idle
/// stack, and mark the rest free.
///
/// Call after the PMM exists (idle's stack is frames) and after `traps::init`
/// (the M6a software clock does not need interrupts, but the M3 timer that will
/// replace it does). Idempotent: a second call is a no-op.
pub fn init() {
    let _mask = InterruptsOff::new();
    let mut sched = SCHED.lock();
    if sched.initialized {
        return;
    }
    sched.initialized = true;
    sched.next_id = FIRST_SPAWNABLE_SLOT as u64;
    sched.quantum_left = QUANTUM_TICKS;
    CURRENT_SLOT.store(SHELL_SLOT, Ordering::Relaxed);

    // Slot 0 is the thread that is executing this function: it is already
    // running, on the boot stack, and its context is filled in by the first
    // switch away from it. `stack_base = 0` is the marker that says "the
    // scheduler does not own this stack".
    let shell = task(SHELL_SLOT);
    let idle = task(IDLE_SLOT);
    // SAFETY: before `init` every slot is `Free` and nothing else writes the
    // table; we hold the scheduler lock with interrupts masked.
    unsafe {
        (*shell).id = 0;
        (*shell).name.set("shell");
        (*shell).state = State::Running;
        (*shell).entry = None;
        (*shell).stack_base = 0;
        (*shell).stack_top = 0;
        (*shell).epoch = 0;
        (*shell).counter.store(0, Ordering::Relaxed);
        (*shell).switches = 0;

        (*idle).id = 1;
        (*idle).name.set("idle");
        (*idle).entry = Some(idle_main);
        (*idle).epoch = 0;
        (*idle).counter.store(0, Ordering::Relaxed);
        (*idle).switches = 0;
    }

    // The idle stack is persistent state for the whole boot: the frame ledger
    // that the memory selftest prints is measured *after* this allocation, so
    // idle costs the same five frames in every pass. Exited threads are a
    // different story — those stacks go back (see `reap`).
    match pmm::alloc_frames_contiguous(STACK_ALLOC_FRAMES) {
        Some(base) => {
            prepare_stack(base);
            let top = base + GUARD_SIZE + STACK_SIZE;
            // SAFETY: as above; the slot is free and the frames are now ours.
            unsafe {
                (*idle).state = State::Ready;
                (*idle).stack_base = base;
                (*idle).stack_top = top;
                (*idle).context = switch::initial_context(task_entry, top);
            }
        }
        None => {
            // Idle is a fallback; without a stack it simply never becomes
            // runnable. The rest of the kernel works, so say so and continue.
            // SAFETY: the slot is free; marking it free again keeps it out of
            // every listing and every switch.
            unsafe { (*idle).state = State::Free };
            crate::kprintln!("[task] no frames for the idle stack: idle is unavailable");
        }
    }
}

/// Create a kernel thread that runs `entry` on a fresh stack.
///
/// Returns the new thread's id, or `None` when the table has no reusable slot or
/// the frame allocator has no run of [`STACK_ALLOC_FRAMES`] frames. The thread
/// starts queued (`Ready`) and runs after the next tick/yield of the caller.
pub fn spawn(entry: fn()) -> Option<u64> {
    spawn_thread(entry, 0)
}

/// `spawn` with a demo cohort tag; the tag is only read by [`demo_main`].
fn spawn_thread(entry: fn(), epoch: u64) -> Option<u64> {
    let _mask = InterruptsOff::new();

    // Two phases so that no frame allocation happens under the scheduler lock:
    // (1) find a slot, (2) take frames, (3) commit. Nothing can steal the slot in
    // between because interrupts are masked and this is the only hart.
    let slot = {
        let sched = SCHED.lock();
        if !sched.initialized {
            return None;
        }
        find_free_slot()
    }?;

    // `?` above deliberately runs with the mask still on: a failed spawn must not
    // leak frames, and it does not take any until after the slot is known.
    let base = match pmm::alloc_frames_contiguous(STACK_ALLOC_FRAMES) {
        Some(base) => base,
        None => return None,
    };
    prepare_stack(base);
    let stack_top = base + GUARD_SIZE + STACK_SIZE;

    let mut sched = SCHED.lock();
    let t = task(slot);
    let id = sched.next_id;
    sched.next_id += 1;
    // SAFETY: the slot was found `Free` and interrupts are masked, so no other
    // context is running (single hart) and the slot cannot have changed. `base`
    // belongs to this thread until it exits and is reaped.
    unsafe {
        (*t).id = id;
        (*t).name.set_numbered("task-", id);
        (*t).state = State::Ready;
        (*t).entry = Some(entry);
        (*t).stack_base = base;
        (*t).stack_top = stack_top;
        (*t).epoch = epoch;
        (*t).counter.store(0, Ordering::Relaxed);
        (*t).switches = 0;
        (*t).context = switch::initial_context(task_entry, stack_top);
    }
    push_ready(&mut sched, slot);
    Some(id)
}

/// Find a slot for `spawn`: a never-used one first (so a freshly exited thread's
/// listing survives), then a recycled one whose stack has already been reaped.
///
/// The caller holds the scheduler lock. A slot that is `Exited` with frames still
/// attached is *not* reusable until [`reap`] has returned them, otherwise the
/// ledger would silently lose [`STACK_ALLOC_FRAMES`] frames per spawn.
fn find_free_slot() -> Option<usize> {
    let mut first_exited = None;
    for slot in FIRST_SPAWNABLE_SLOT..MAX_TASKS {
        let t = task(slot);
        // SAFETY: slot index is in range and the caller holds the scheduler
        // lock; `state`/`stack_base` are plain fields written only there.
        unsafe {
            match (*t).state {
                State::Free => return Some(slot),
                State::Exited if (*t).stack_base == 0 && first_exited.is_none() => {
                    first_exited = Some(slot)
                }
                _ => {}
            }
        }
    }
    first_exited
}

/// Fill a fresh stack allocation: guard frame, canary, usable fill.
fn prepare_stack(base: usize) {
    // SAFETY: `base` is the first frame of a run of `STACK_ALLOC_FRAMES` frames
    // just taken from the PMM; no other code owns the memory, and the kernel
    // identity-maps RAM read/write (the stack must be writable for the thread to
    // run on it). The guard frame is bytes `0..GUARD_SIZE`, the canary is its
    // last word, and the usable stack is the next `STACK_SIZE` bytes.
    unsafe {
        core::ptr::write_bytes(base as *mut u8, GUARD_FILL, GUARD_SIZE);
        core::ptr::write_unaligned((base + GUARD_SIZE - 8) as *mut u64, CANARY);
        core::ptr::write_bytes((base + GUARD_SIZE) as *mut u8, STACK_FILL, STACK_SIZE);
    }
}

/// True when nothing wrote past the bottom of a spawned thread's stack.
fn stack_guard_intact(base: usize) -> bool {
    // SAFETY: `base` is the first frame of a stack allocation the scheduler owns
    // (the only caller passes a `stack_base` from the task table), so the guard
    // frame is live, writable memory that this function only reads.
    let guard = unsafe { core::slice::from_raw_parts(base as *const u8, GUARD_SIZE) };
    let split = GUARD_SIZE - 8;
    let mut canary_bytes = [0u8; 8];
    canary_bytes.copy_from_slice(&guard[split..]);
    let canary = u64::from_le_bytes(canary_bytes);
    canary == CANARY && guard[..split].iter().all(|&byte| byte == GUARD_FILL)
}

/// Give a thread's stack frames back to the PMM, reporting a smashed guard first.
fn free_stack(base: usize) {
    if !stack_guard_intact(base) {
        CANARY_VIOLATIONS.fetch_add(1, Ordering::Relaxed);
        crate::kprintln!(
            "[task] STACK OVERFLOW: guard/canary at {:#x} was overwritten; frames returned anyway",
            base
        );
    }
    pmm::free_frames_contiguous(base, STACK_ALLOC_FRAMES);
    REAPS.fetch_add(1, Ordering::Relaxed);
}

/// Return every exited thread's stack to the PMM.
///
/// The running thread's own stack is never freed here (it is still in use); that
/// is why the reap of a thread that called `exit` happens on the next scheduler
/// entry from another context. Called from every [`schedule`] and by the tests;
/// returns the number of stacks returned.
pub fn reap() -> usize {
    let _mask = InterruptsOff::new();
    let mut bases = [0usize; MAX_TASKS];
    let count = {
        let _sched = SCHED.lock();
        collect_reaps(&mut bases)
    };
    for base in bases[..count].iter() {
        free_stack(*base);
    }
    count
}

/// Move every exited thread's stack base out of the table into `bases`.
///
/// Called under the scheduler lock, which is what makes "mark reaped, then free
/// outside the lock" safe on one hart: interrupts are masked for the whole
/// entry, so the two halves are atomic with respect to a tick.
fn collect_reaps(bases: &mut [usize; MAX_TASKS]) -> usize {
    let current = CURRENT_SLOT.load(Ordering::Relaxed);
    let mut count = 0usize;
    for slot in 0..MAX_TASKS {
        if slot == current {
            continue;
        }
        let t = task(slot);
        // SAFETY: slot is in range; the caller holds the scheduler lock.
        unsafe {
            if (*t).state == State::Exited && (*t).stack_base != 0 {
                bases[count] = (*t).stack_base;
                (*t).stack_base = 0;
                count += 1;
            }
        }
    }
    count
}

/// One `spawn`-able demo thread's body: bump the counter, tick the clock.
///
/// It never calls `yield_now` and never sleeps: the only way it loses the CPU is
/// the tick, which is exactly what the M3 timer will do asynchronously. When a
/// newer `spawn` bumps the epoch, the next iteration falls out into `exit`.
fn demo_main() {
    let slot = CURRENT_SLOT.load(Ordering::Relaxed);
    let t = task(slot);
    // SAFETY: we are the running thread, so the slot is ours and `epoch` was set
    // by the spawn that created us; it is only read once, before any switch.
    let epoch = unsafe { (*t).epoch };
    loop {
        if DEMO_EPOCH.load(Ordering::Relaxed) != epoch {
            exit();
        }
        // SAFETY: slot belongs to this running thread; the counter is atomic
        // because the shell reads it from another context between entries.
        unsafe { (*t).counter.fetch_add(1, Ordering::Relaxed) };
        pump();
    }
}

/// The idle thread: `wfi` until something wakes the hart.
///
/// `wfi` is only correct once a wake-up source exists. Before the M3 timer
/// lands there is none, so parking here would sleep the machine forever; in that
/// configuration idle gives the CPU back instead, which also lets the selftest
/// probe it. The moment [`tick_from_timer`] reports a hardware clock, the `wfi`
/// branch is taken and the timer becomes the wake-up.
fn idle_main() {
    loop {
        IDLE_WAKES.fetch_add(1, Ordering::Relaxed);
        if HARDWARE_CLOCK.load(Ordering::Relaxed) {
            // SAFETY: `wfi` is a hint with no memory effects and needs no state;
            // with the timer attached, its interrupt is what makes it return.
            unsafe { core::arch::asm!("wfi", options(nomem, nostack)) };
        } else {
            schedule(Reason::IdleStep);
        }
    }
}

/// The first instruction a freshly spawned thread executes.
///
/// It is reached by the restored `ra` of a context built by
/// [`switch::initial_context`], with `sp` already pointing at the thread's own
/// stack top. Interrupts were masked by the switching thread's critical section,
/// so a fresh thread enables them here: a mask belongs to a critical section,
/// not to a thread.
extern "C" fn task_entry() -> ! {
    csr::enable_interrupts();
    let slot = CURRENT_SLOT.load(Ordering::Relaxed);
    let t = task(slot);
    // SAFETY: we are the thread this slot describes and it is `Running`, so the
    // entry is the body its spawn installed. `fn()` is `Copy`, so no borrow of
    // the table survives the load.
    let entry = unsafe { (*t).entry };
    if let Some(body) = entry {
        body();
    }
    // Falling off the entry point of a kernel thread is the normal way to end:
    // drop out of the queue for good and let the reaper have the stack.
    exit()
}

/// Let the next runnable thread run; the caller continues when it is scheduled
/// again. This is the only voluntary switch.
pub fn yield_now() {
    schedule(Reason::Yield);
}

/// End the calling thread. Never returns.
///
/// The stack is not freed here — the thread is still running on it until the
/// switch completes — but the slot is `Exited` before the switch, so the next
/// scheduler entry from any other context reaps the frames.
pub fn exit() -> ! {
    schedule(Reason::Exit);
    // A thread that exited is never made runnable again; if it ever were, its
    // stack may already belong to the frame allocator and parking is the only
    // safe answer. This line is unreachable by construction.
    crate::kprintln!("[task] an exited thread was scheduled again; parking");
    crate::arch::riscv64::park_forever()
}

/// One clock tick: run the current thread's quantum down and switch when it
/// expires. This is the preemption hook; it allocates nothing.
///
/// Callers today: the shell's UART poll loop and the demo thread body, both
/// through [`pump`]. Once M3 merges, the timer interrupt calls it (through
/// [`tick_from_timer`]) and those software call sites become no-ops.
pub fn tick() {
    schedule(Reason::Quantum);
}

/// The M3 seam: the timer interrupt's handler.
///
/// M3 adds `src/arch/riscv64/timer.rs` exposing `ticks()` and
/// `set_tick_handler(fn())`. The **only** wiring the scheduler then needs is one
/// line in `kmain`, after the timer is initialised and before `shell::run`:
///
/// ```text
/// arch::riscv64::timer::set_tick_handler(crate::task::scheduler::tick_from_timer);
/// ```
///
/// This function marks the hardware clock as attached — after which [`pump`]
/// stops issuing software ticks, so the shell's poll loop and the demo bodies
/// keep working unchanged — and then does exactly what a software tick does.
/// M3 must call the registered handler from the `CAUSE_SUPERVISOR_TIMER` branch
/// of `trap_dispatch`, after acknowledging/rearming the timer, and must not hold
/// a spinlock across it. A switch from inside the trap handler is supported by
/// construction: the interrupted thread's trap frame lives on that thread's own
/// stack, and the handler resumes into `sret` when the thread is scheduled again.
#[allow(dead_code)] // wired by the one line above once `riscv64/timer.rs` merges (M3)
pub fn tick_from_timer() {
    HARDWARE_CLOCK.store(true, Ordering::Relaxed);
    schedule(Reason::Quantum);
}

/// Whether a real timer is driving the clock (set by [`tick_from_timer`]).
pub fn timer_clock_attached() -> bool {
    HARDWARE_CLOCK.load(Ordering::Relaxed)
}

/// The software clock: one tick if no hardware clock exists yet, otherwise
/// nothing. Called from the shell's idle loop and from the demo thread body —
/// the two places a timer interrupt will preempt once M3 lands.
pub fn pump() {
    if !HARDWARE_CLOCK.load(Ordering::Relaxed) {
        schedule(Reason::Quantum);
    }
}

/// Enter the idle thread for exactly one pass, then come back.
///
/// This is the selftest's probe: it proves the idle thread is reachable, that a
/// switch into a thread with a fresh context works, and that idle hands control
/// back without a hardware clock. It is not the production path (M6b's blocking
/// wait will be), but it is the same switch.
pub fn run_idle_once() -> bool {
    let before = IDLE_WAKES.load(Ordering::Relaxed);
    schedule(Reason::ProbeIdle);
    IDLE_WAKES.load(Ordering::Relaxed) > before
}

/// Start `count` demo kernel threads, replacing the previous cohort.
///
/// Used by the shell's `spawn` command and by nothing else. Returns how many
/// threads were actually started; `count == 0` only cancels the old cohort.
pub fn spawn_demo(count: usize) -> usize {
    let _ = cancel_demo();
    let epoch = DEMO_EPOCH.fetch_add(1, Ordering::Relaxed) + 1;
    let count = count.min(MAX_TASKS - FIRST_SPAWNABLE_SLOT);
    let mut started = 0usize;
    for _ in 0..count {
        if spawn_thread(demo_main, epoch).is_some() {
            started += 1;
        }
    }
    started
}

/// Invalidate the demo cohort and pump until its threads have exited and been
/// reaped. Returns the number of stacks returned.
///
/// A demo thread checks its epoch every iteration, so "cancel" is just a flag it
/// cannot miss; the pump gives each of them a turn to notice. Also used by the
/// task selftest so the suite starts with free slots and a clean ledger.
pub fn cancel_demo() -> usize {
    DEMO_EPOCH.fetch_add(1, Ordering::Relaxed);
    let mut pumped = 0usize;
    while foreign_tasks() > 0 && pumped < QUIESCE_LIMIT {
        schedule(Reason::Quantum);
        pumped += 1;
    }
    reap()
}

/// Threads that are neither the shell nor idle and are still runnable.
///
/// The selftest uses this as its "all spawned work has finished" predicate; the
/// shell never needs it. Exited-and-reaped slots do not count, which is what
/// makes it usable as a quiescence test.
pub fn foreign_tasks() -> usize {
    let _mask = InterruptsOff::new();
    let _sched = SCHED.lock();
    let mut live = 0usize;
    for slot in 0..MAX_TASKS {
        if slot == SHELL_SLOT || slot == IDLE_SLOT {
            continue;
        }
        let t = task(slot);
        // SAFETY: slot is in range and the scheduler lock is held.
        unsafe {
            if matches!((*t).state, State::Ready | State::Running) {
                live += 1;
            }
        }
    }
    live
}

/// The scheduling decision plus the switch.
///
/// Interrupts are masked for the whole function: the ready queue, the current
/// pointer and the switch must be atomic with respect to a timer tick. No
/// allocation happens here — the only work besides state changes is giving
/// already-reaped stacks back to the PMM, whose lock nests inside this mask.
fn schedule(reason: Reason) {
    let _mask = InterruptsOff::new();
    let mut bases = [0usize; MAX_TASKS];
    let (pair, reap_count) = {
        let mut sched = SCHED.lock();
        if !sched.initialized {
            return;
        }
        let pair = pick_next(&mut sched, reason);
        let reap_count = collect_reaps(&mut bases);
        (pair, reap_count)
    };

    // Outside the scheduler lock (but still with interrupts masked): returning
    // frames to the PMM is not scheduler state, and doing it here keeps the lock
    // order scheduler → PMM one-directional.
    for base in bases[..reap_count].iter() {
        free_stack(*base);
    }

    if let Some((from_slot, to_slot)) = pair {
        let from = task(from_slot);
        let to = task(to_slot);
        // SAFETY: both slots are live and distinct (the decision above), their
        // contexts live in the static table so the pointers are stable, and
        // interrupts are masked for the whole switch. When this thread is
        // scheduled again it continues right here.
        unsafe { switch::switch(&mut (*from).context, &(*to).context) };
    }
    // `_mask` drops here — or when this thread is resumed after the switch — and
    // restores *this* context's interrupt state.
}

/// Choose the next thread, update the states and the queue.
///
/// The caller holds the scheduler lock with interrupts masked. Returns
/// `(from_slot, to_slot)` when a switch is due, `None` to keep running.
fn pick_next(sched: &mut Sched, reason: Reason) -> Option<(usize, usize)> {
    let current = CURRENT_SLOT.load(Ordering::Relaxed);

    // The quantum is only run down when there is somebody else to run; otherwise
    // the thread would spend its quantum while alone and be switched out on the
    // first tick after a spawn.
    match reason {
        Reason::Quantum => {
            sched.ticks += 1;
            if sched.ready_len == 0 {
                sched.quantum_left = QUANTUM_TICKS;
                return None;
            }
            if sched.quantum_left > 1 {
                sched.quantum_left -= 1;
                return None;
            }
        }
        Reason::Yield => {
            if sched.ready_len == 0 {
                sched.quantum_left = QUANTUM_TICKS;
                return None;
            }
        }
        Reason::Exit | Reason::ProbeIdle | Reason::IdleStep => {}
    }

    // Retire or requeue the outgoing thread.
    let outgoing = task(current);
    match reason {
        Reason::Quantum | Reason::Yield => {
            // SAFETY: `current` names the running thread; the lock is held.
            unsafe {
                if (*outgoing).state == State::Running {
                    (*outgoing).state = State::Ready;
                    // Idle is a fallback, not a round-robin member: only an
                    // explicit probe/park reaches it, so it is not requeued.
                    if current != IDLE_SLOT {
                        push_ready(sched, current);
                    }
                }
            }
        }
        Reason::Exit => {
            // SAFETY: as above; the thread has returned from its entry.
            unsafe { (*outgoing).state = State::Exited };
        }
        Reason::ProbeIdle => {
            // SAFETY: as above. The boot thread (or whoever probes) waits in the
            // queue while idle runs.
            unsafe {
                if (*outgoing).state == State::Running {
                    (*outgoing).state = State::Ready;
                    push_ready(sched, current);
                }
            }
        }
        Reason::IdleStep => {
            // SAFETY: as above; idle goes back to the fallback slot, unqueued.
            unsafe { (*outgoing).state = State::Ready };
        }
    }

    let next = match reason {
        // The probe names its target: idle is never in the queue.
        Reason::ProbeIdle => IDLE_SLOT,
        _ => match pop_ready(sched) {
            Some(slot) => slot,
            None => {
                // Idle stepping back with nothing queued means the probe was
                // called from idle itself; just stay there.
                if reason == Reason::IdleStep {
                    // SAFETY: as above; restore the running state we changed.
                    unsafe { (*outgoing).state = State::Running };
                    return None;
                }
                // The boot thread is always queued while a spawned thread runs,
                // so this is unreachable by construction. Report and park rather
                // than switch into a thread that is not there.
                crate::kprintln!("[task] no runnable thread after exit; parking");
                crate::arch::riscv64::park_forever();
            }
        },
    };

    if next == current {
        // Only the outgoing thread was runnable (the probe path can land here if
        // idle is probed twice without the caller being queued). Keep running.
        // SAFETY: as above.
        unsafe { (*outgoing).state = State::Running };
        sched.quantum_left = QUANTUM_TICKS;
        return None;
    }

    let incoming = task(next);
    // SAFETY: both slots are live and distinct; the lock is held and interrupts
    // are masked, so no other context can observe the half-finished switch.
    unsafe {
        (*outgoing).switches += 1;
        (*incoming).state = State::Running;
    }
    CURRENT_SLOT.store(next, Ordering::Relaxed);
    sched.quantum_left = QUANTUM_TICKS;
    sched.switches += 1;
    Some((current, next))
}

/// Append a slot to the ready ring.
fn push_ready(sched: &mut Sched, slot: usize) {
    // The queue holds at most one entry per slot (7 of 8 at the very most), so
    // a full ring is a logic bug; dropping the entry keeps the scheduler alive
    // instead of panicking in the switch path.
    if sched.ready_len >= MAX_TASKS {
        return;
    }
    let tail = (sched.ready_head + sched.ready_len) % MAX_TASKS;
    sched.ready[tail] = slot as u8;
    sched.ready_len += 1;
}

/// Take the oldest slot from the ready ring.
fn pop_ready(sched: &mut Sched) -> Option<usize> {
    if sched.ready_len == 0 {
        return None;
    }
    let slot = sched.ready[sched.ready_head] as usize;
    sched.ready_head = (sched.ready_head + 1) % MAX_TASKS;
    sched.ready_len -= 1;
    Some(slot)
}

/// A copy of every live task-table entry, in slot order. Returns the count.
///
/// The caller passes a fixed-size buffer (the shell prints it, the selftest
/// inspects it); nothing here allocates.
pub fn snapshot(out: &mut [TaskInfo]) -> usize {
    let _mask = InterruptsOff::new();
    let _sched = SCHED.lock();
    let mut count = 0usize;
    for slot in 0..MAX_TASKS {
        if count == out.len() {
            break;
        }
        let t = task(slot);
        // SAFETY: slot is in range and the scheduler lock is held; every field
        // is plain data (the counter is atomic), copied out so the listing
        // cannot observe a later change.
        unsafe {
            if (*t).state == State::Free {
                continue;
            }
            out[count] = TaskInfo {
                slot,
                id: (*t).id,
                name: (*t).name,
                state: (*t).state,
                counter: (*t).counter.load(Ordering::Relaxed),
                switches: (*t).switches,
                stack_base: (*t).stack_base,
                stack_top: (*t).stack_top,
            };
        }
        count += 1;
    }
    count
}

/// Counters for the shell and the boot line.
pub fn stats() -> Stats {
    let _mask = InterruptsOff::new();
    let sched = SCHED.lock();
    let current = CURRENT_SLOT.load(Ordering::Relaxed);
    let current_task = task(current);
    let mut live = 0usize;
    let mut ready = 0usize;
    for slot in 0..MAX_TASKS {
        let t = task(slot);
        // SAFETY: slot is in range and the scheduler lock is held.
        unsafe {
            match (*t).state {
                State::Free => {}
                State::Ready => {
                    live += 1;
                    ready += 1;
                }
                State::Running | State::Exited => live += 1,
            }
        }
    }
    // SAFETY: `current` is a live slot; the lock is held.
    let current_id = unsafe { (*current_task).id };
    Stats {
        ticks: sched.ticks,
        switches: sched.switches,
        reaps: REAPS.load(Ordering::Relaxed),
        canary_violations: CANARY_VIOLATIONS.load(Ordering::Relaxed),
        idle_wakes: IDLE_WAKES.load(Ordering::Relaxed),
        current_id,
        live_tasks: live,
        ready_tasks: ready,
        hardware_clock: HARDWARE_CLOCK.load(Ordering::Relaxed),
    }
}

/// Id of the running thread.
pub fn current_id() -> u64 {
    let _mask = InterruptsOff::new();
    let current = CURRENT_SLOT.load(Ordering::Relaxed);
    // SAFETY: `current` is a live slot and interrupts are masked so it cannot
    // change while we read it.
    unsafe { (*task(current)).id }
}

/// Add one to the running thread's work counter (the demo/selftest body does
/// this; the scheduler never touches it).
pub(crate) fn bump_counter() {
    let slot = CURRENT_SLOT.load(Ordering::Relaxed);
    // SAFETY: the running thread owns this slot; the counter is atomic because
    // the shell reads it between scheduler entries.
    unsafe { (*task(slot)).counter.fetch_add(1, Ordering::Relaxed) };
}

/// Address of the running thread's canary word, for the overflow selftest.
///
/// The selftest writes one byte here on purpose — an overflow that the reap must
/// detect. `None` for the boot thread, whose stack has no guard.
pub(crate) fn current_canary_address() -> Option<usize> {
    let slot = CURRENT_SLOT.load(Ordering::Relaxed);
    // SAFETY: the running thread owns this slot.
    let base = unsafe { (*task(slot)).stack_base };
    if base == 0 {
        None
    } else {
        Some(base + GUARD_SIZE - 8)
    }
}

/// Inspect a spawned thread's guard/canary and stack high-water mark.
///
/// `None` for a slot with no stack (the boot thread, an exited-and-reaped
/// thread, a free slot). Used by the selftest; the numbers it returns are the
/// evidence that the guard is real memory and that overflow is detectable.
pub(crate) fn check_stack(slot: usize) -> Option<StackHealth> {
    let _mask = InterruptsOff::new();
    let t = task(slot);
    if t.is_null() {
        return None;
    }
    // SAFETY: slot is in range; the lock is held so the stack cannot be reaped
    // while it is inspected. `stack_base != 0` means a live PMM allocation.
    unsafe {
        let base = (*t).stack_base;
        let top = (*t).stack_top;
        if base == 0 || top != base + GUARD_SIZE + STACK_SIZE {
            return None;
        }
        let guard = core::slice::from_raw_parts(base as *const u8, GUARD_SIZE);
        let split = GUARD_SIZE - 8;
        let mut canary_bytes = [0u8; 8];
        canary_bytes.copy_from_slice(&guard[split..]);
        let canary_ok = u64::from_le_bytes(canary_bytes) == CANARY;
        let guard_ok = guard[..split].iter().all(|&byte| byte == GUARD_FILL);
        let stack = core::slice::from_raw_parts((base + GUARD_SIZE) as *const u8, STACK_SIZE);
        let mut used = 0usize;
        for (index, &byte) in stack.iter().enumerate() {
            if byte != STACK_FILL {
                used = STACK_SIZE - index;
                break;
            }
        }
        Some(StackHealth {
            usable: STACK_SIZE,
            used,
            canary_ok,
            guard_ok,
        })
    }
}
