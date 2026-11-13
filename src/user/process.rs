//! The process table: one `Process` per live or zombie program.
//!
//! A process owns an Sv39 address space (a private root plus a nonzero ASID),
//! the heap and `mmap` bookkeeping, the image page list, one thread group, its
//! exit status and its parent/waiter links. Threads are scheduler tasks whose
//! `group` field is the process id, so the scheduler can retire a whole process
//! with `kill_group` without knowing what a process is.
//!
//! The table is small and static ([`MAX_PROCESSES`]) for the same reason the
//! scheduler's is: a fork must not depend on the heap, and the single hart means
//! a slot scan is a bounded loop under one spinlock. Everything that frees pages
//! or descriptors is done by the caller *after* the slot has been taken out, so
//! the lock is never held across a page-table walk, a VFS call or a blocking
//! wait.
//!
//! # Lifecycle
//!
//! `Free -> Live -> Zombie -> Free`. `Live` means at least one thread has not
//! exited; the last `exit`/`exit_group`/fault flips a process to `Zombie` and
//! wakes the parent thread blocked in `wait4` (if any). A zombie keeps its slot
//! and exit status until its parent reaps it; the shell reaps every survivor
//! when a foreground `run` ends, because this kernel has no init process to
//! adopt an orphan.

use crate::sync::Spinlock;
use crate::user::elf::Loaded;
use crate::user::{Cmdline, Exit, MmapRegion, RunName, MAX_MMAP_REGIONS, USER_HEAP_PAGES};

/// Most processes one boot may have live at once.
///
/// The scheduler has 14 spawnable task slots; six one-thread processes leave
/// eight slots for `clone` children, and six private Sv39 roots cost roughly
/// 3 MiB of page tables on the 256 MiB boot configuration. A seventh concurrent
/// `fork` is `-EAGAIN`, which the ABI documents instead of silently serialising.
pub(crate) const MAX_PROCESSES: usize = 6;

/// One program.
pub(crate) struct Process {
    /// Monotonic process id. It is also the scheduler `group` tag, the futex
    /// scope and the signal-table key, so it is never reused.
    pub pid: u64,
    /// Parent process id, `0` for a program the shell started.
    pub ppid: u64,
    /// Root table of this process's Sv39 address space.
    pub root: usize,
    /// `satp` value (Sv39 | ASID | root) the scheduler installs for it.
    pub satp: usize,
    /// Nonzero TLB tag; reserved for diagnostics and the `ps`-style evidence.
    pub asid: usize,
    /// Display/debug name (`argv[0]`, truncated to 32 bytes).
    pub name: RunName,
    /// Bounded snapshot of the argv the kernel built the entry frame from
    /// (`/proc/<pid>/cmdline`); see [`Cmdline`].
    pub cmdline: Cmdline,
    /// User entry point and initial stack frame, used by `user_task`.
    pub entry: usize,
    pub sp: usize,
    pub argc: usize,
    pub argv: usize,
    /// Scheduler id of the main thread (`gettid` of the thread that called
    /// `exec`/started the image); `pid` is the process identity.
    pub main_tid: u64,
    /// Exit code recorded by the main thread, even if other threads outlive it.
    pub main_code: Option<u32>,
    /// Threads of this process that have not exited yet.
    pub threads: usize,
    /// Set exactly once, when `threads` reaches 0.
    pub exit: Option<Exit>,
    /// Signal that killed the process, if it was a signal or a mapped fault
    /// (`SIGSEGV`/`SIGILL`/`SIGTRAP`). `wait4` reports it as `signum`.
    pub signal: Option<usize>,
    /// Page-aligned current `brk`. The frames themselves live in [`HEAPS`]:
    /// keeping a 1 MiB page list inline made `Process` too large to move through
    /// a 16 KiB kernel stack (the first `fork`/`wait4` pair overflowed it).
    pub heap_end: usize,
    pub heap_count: usize,
    /// Contiguous extents created by `mmap`.
    pub mmap_regions: [MmapRegion; MAX_MMAP_REGIONS],
    pub mmap_count: usize,
    /// First frame of the 64 KiB initial user stack allocation (`0` before it is
    /// set). The frames above [`stack_low`](Self::stack_low) are this run.
    pub stack_base: usize,
    /// Lowest user stack address currently mapped.
    ///
    /// Starts at `USER_STACK_BASE` and moves down one page at a time as the
    /// demand-grown stack expands (see `src/user/stack.rs`), never below
    /// `USER_STACK_FLOOR`. Teardown walks `stack_low..USER_STACK_BASE` to unmap
    /// and free the grown pages, so this field is the whole bookkeeping the
    /// grown half of the stack needs.
    pub stack_low: usize,
    /// Thread id blocked in `wait4` for one of this process's children (0 while
    /// nobody waits). Written by the parent's `wait4`, read by the child's exit.
    pub wait_tid: u64,
    /// `false` between slot publication and the main thread's successful spawn.
    /// `wait4` and the shell's cleanup skip such a slot; the window is one
    /// interrupts-off region in the creator.
    pub ready: bool,
}

impl Process {
    /// A process in the "slot reserved, nothing published yet" shape.
    pub fn new(pid: u64, ppid: u64, root: usize, satp: usize, asid: usize) -> Self {
        Process {
            pid,
            ppid,
            root,
            satp,
            asid,
            name: RunName::empty(),
            cmdline: Cmdline::empty(),
            entry: 0,
            sp: 0,
            argc: 0,
            argv: 0,
            main_tid: 0,
            main_code: None,
            threads: 1,
            exit: None,
            signal: None,
            heap_end: crate::user::USER_HEAP_BASE,
            heap_count: 0,
            mmap_regions: [MmapRegion::empty(); MAX_MMAP_REGIONS],
            mmap_count: 0,
            stack_base: 0,
            stack_low: crate::user::elf::USER_STACK_BASE,
            wait_tid: 0,
            ready: false,
        }
    }
}

static PROCS: Spinlock<[Option<Process>; MAX_PROCESSES]> =
    Spinlock::new([const { None }; MAX_PROCESSES]);

/// Image page lists, one row per process slot.
///
/// A `Loaded` carries 256 virtual addresses (2 KiB); keeping it out of
/// `Process` is what makes moves of a `Process` cheap enough for a trap stack.
static IMAGES: Spinlock<[Option<Loaded>; MAX_PROCESSES]> =
    Spinlock::new([const { None }; MAX_PROCESSES]);

/// Heap frame lists, one row per process slot. Same reason as [`IMAGES`].
static HEAPS: Spinlock<[[usize; USER_HEAP_PAGES]; MAX_PROCESSES]> =
    Spinlock::new([[0; USER_HEAP_PAGES]; MAX_PROCESSES]);

/// Install the image page list for `slot`.
pub(crate) fn set_image(slot: usize, loaded: Loaded) {
    crate::arch::riscv64::csr::without_interrupts(|| {
        if let Some(entry) = IMAGES.lock().get_mut(slot) {
            *entry = Some(loaded);
        }
    });
}

/// Read the image page list for `slot`.
pub(crate) fn with_image<R>(slot: usize, f: impl FnOnce(&Loaded) -> R) -> Option<R> {
    crate::arch::riscv64::csr::without_interrupts(|| IMAGES.lock().get(slot)?.as_ref().map(f))
}

/// One image page of `slot` by index.
pub(crate) fn image_page(slot: usize, index: usize) -> Option<usize> {
    with_image(slot, |loaded| loaded.pages().get(index).copied()).flatten()
}

/// Take the image page list for `slot` so its frames can be released.
pub(crate) fn take_image(slot: usize) -> Option<Loaded> {
    crate::arch::riscv64::csr::without_interrupts(|| {
        IMAGES.lock().get_mut(slot).and_then(|entry| entry.take())
    })
}

/// Read one heap frame of `slot` (`0` when unset).
pub(crate) fn heap_frame(slot: usize, index: usize) -> usize {
    crate::arch::riscv64::csr::without_interrupts(|| {
        HEAPS
            .lock()
            .get(slot)
            .and_then(|row| row.get(index))
            .copied()
            .unwrap_or(0)
    })
}

/// Record one heap frame of `slot`.
pub(crate) fn set_heap_frame(slot: usize, index: usize, frame: usize) {
    crate::arch::riscv64::csr::without_interrupts(|| {
        if let Some(slot_row) = HEAPS.lock().get_mut(slot) {
            if let Some(cell) = slot_row.get_mut(index) {
                *cell = frame;
            }
        }
    });
}

/// Clear the heap frame list of `slot` after teardown.
pub(crate) fn clear_heap(slot: usize) {
    crate::arch::riscv64::csr::without_interrupts(|| {
        if let Some(row) = HEAPS.lock().get_mut(slot) {
            *row = [0; USER_HEAP_PAGES];
        }
    });
}

/// Publish an empty process in a free slot and return the slot.
///
/// The fork path fills the fields (and copies the pages) afterwards, through
/// [`with_slot_mut`]; the slot is invisible to `wait4` until `ready` is set.
pub(crate) fn reserve(pid: u64, ppid: u64) -> Option<usize> {
    with_table(|table| {
        let slot = table.iter().position(|entry| entry.is_none())?;
        table[slot] = Some(Process::new(pid, ppid, 0, 0, 0));
        Some(slot)
    })
}

/// Run `f` with the process table locked and interrupts masked.
///
/// The process layer calls the scheduler while holding this lock in exactly one
/// direction (a child's exit reads its parent's `wait_tid`, releases the table
/// and then wakes). Nothing here allocates or touches user memory.
pub(crate) fn with_table<R>(f: impl FnOnce(&mut [Option<Process>; MAX_PROCESSES]) -> R) -> R {
    crate::arch::riscv64::csr::without_interrupts(|| f(&mut PROCS.lock()))
}

/// Publish a fully built process in the first free slot, handing the value back
/// when there is none so the caller can release its pages.
pub(crate) fn publish(process: Process) -> Result<usize, Process> {
    with_table(
        |table| match table.iter().position(|entry| entry.is_none()) {
            Some(slot) => {
                table[slot] = Some(process);
                Ok(slot)
            }
            None => Err(process),
        },
    )
}

/// Run `f` on a process in place, without moving it out of the table.
pub(crate) fn with_slot_mut<R>(slot: usize, f: impl FnOnce(&mut Process) -> R) -> Option<R> {
    with_table(|table| table.get_mut(slot).and_then(|entry| entry.as_mut()).map(f))
}

/// Free a slot after its resources were released.
pub(crate) fn clear_slot(slot: usize) {
    with_table(|table| {
        if let Some(entry) = table.get_mut(slot) {
            *entry = None;
        }
    });
}

/// Slot of `pid`, if it is live or zombie.
pub(crate) fn slot_of(pid: u64) -> Option<usize> {
    if pid == 0 {
        return None;
    }
    with_table(|table| {
        table
            .iter()
            .position(|entry| entry.as_ref().is_some_and(|process| process.pid == pid))
    })
}

/// The calling thread's process id, if it belongs to a user program.
pub(crate) fn current_pid() -> Option<u64> {
    let group = crate::task::scheduler::current_group();
    (group != 0).then_some(group)
}

/// The calling thread's process slot.
pub(crate) fn current_slot() -> Option<usize> {
    let pid = current_pid()?;
    slot_of(pid)
}

/// Read a value from a process without holding the lock afterwards.
pub(crate) fn peek<R>(pid: u64, f: impl FnOnce(&Process) -> R) -> Option<R> {
    with_table(|table| {
        table
            .iter()
            .flatten()
            .find(|process| process.pid == pid)
            .map(f)
    })
}

/// Mutate a process in place, returning `None` if the pid is gone.
pub(crate) fn update<R>(pid: u64, f: impl FnOnce(&mut Process) -> R) -> Option<R> {
    with_table(|table| {
        table
            .iter_mut()
            .flatten()
            .find(|process| process.pid == pid)
            .map(f)
    })
}

/// Every process the shell may kill during cleanup: published and with at least
/// one thread still accounted for.
pub(crate) fn ready_pids(out: &mut [u64]) -> usize {
    with_table(|table| {
        let mut count = 0usize;
        for process in table.iter().flatten() {
            if count == out.len() {
                break;
            }
            if process.ready {
                out[count] = process.pid;
                count += 1;
            }
        }
        count
    })
}

/// Pids of processes that still have at least one live thread, in table order.
///
/// `/proc` lists these; a zombie (no live thread, waiting for `wait4`) is not a
/// running program and is not exposed as a `/proc/<pid>` directory.
pub(crate) fn live_pids(out: &mut [u64]) -> usize {
    with_table(|table| {
        let mut count = 0usize;
        for process in table.iter().flatten() {
            if count == out.len() {
                break;
            }
            if process.ready && process.threads > 0 {
                out[count] = process.pid;
                count += 1;
            }
        }
        count
    })
}

/// True when `pid` names a process with at least one live thread.
pub(crate) fn is_live(pid: u64) -> bool {
    peek(pid, |process| process.ready && process.threads > 0).unwrap_or(false)
}

/// Force a process to a zombie after its threads were retired by
/// `scheduler::kill_group`.
pub(crate) fn force_finish(pid: u64, exit: Exit, signal: Option<usize>) {
    update(pid, |process| {
        process.threads = 0;
        process.exit = Some(exit);
        process.signal = signal;
        process.ready = true;
    });
}

/// True when `pid` still exists and has no live thread.
pub(crate) fn is_finished(pid: u64) -> bool {
    with_table(|table| {
        table
            .iter()
            .flatten()
            .find(|process| process.pid == pid)
            .is_some_and(|process| process.threads == 0 && process.ready)
    })
}

/// How many process slots are in use (the selftest's teardown evidence).
pub(crate) fn live_processes() -> usize {
    with_table(|table| table.iter().flatten().count())
}
