//! The system-call ABI M6b implements: the RISC-V Linux numbers, a trap frame
//! that carries the arguments, and pointer validation that never trusts a
//! U-mode address.
//!
//! Dispatch runs in the trap handler, with `sstatus.SIE = 0` (hardware cleared
//! it on entry), so a syscall cannot be preempted by the timer halfway through a
//! user-buffer copy. The one voluntary switch this layer performs is
//! `sched_yield`; the scheduler's own critical section is what makes that safe.
//!
//! | `a7` | name | arguments | result |
//! |---|---|---|---|
//! | 64 | `write` | `a0` fd, `a1` buffer, `a2` length | bytes written, `-EBADF`, `-EFAULT` |
//! | 93 | `exit` | `a0` status | does not return |
//! | 124 | `sched_yield` | — | 0 |
//! | 172 | `getpid` | — | the scheduler task id |
//!
//! `write` accepts fd 1 (stdout) and 2 (stderr); both go to the console. Every
//! byte of the buffer is checked to lie in a leaf whose entry has `U=1` and
//! `R=1`, so a kernel address, an unmapped hole, or the guard page below the user
//! stack is `-EFAULT` rather than a fault. `SUM` is set only around the copy and
//! restored from the exact value the handler entered with.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::arch::riscv64::csr;
use crate::arch::riscv64::mmu::{PTE_R, PTE_U};
use crate::arch::riscv64::traps::TrapFrame;
use crate::memory::{paging, FRAME_SIZE};

/// `write(2)`.
const SYS_WRITE: usize = 64;
/// `exit(2)`.
const SYS_EXIT: usize = 93;
/// `sched_yield(2)`.
const SYS_SCHED_YIELD: usize = 124;
/// `getpid(2)`.
const SYS_GETPID: usize = 172;

/// Linux's `ENOSYS` for this target, negated for the return register.
const ENOSYS: isize = -38;
/// Linux's `EFAULT`.
const EFAULT: isize = -14;
/// Linux's `EBADF`.
const EBADF: isize = -9;

/// Bytes copied from a user page per chunk. Small enough for the handler's stack,
/// large enough that the UART, not the copy, is the cost.
const COPY_CHUNK: usize = 256;

/// Counters the selftest reads as evidence that the ABI actually ran.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Successful `write`s.
    pub writes: u64,
    /// Bytes handed to the console by those writes.
    pub bytes: u64,
    /// `sched_yield` calls that returned.
    pub yields: u64,
    /// `getpid` calls.
    pub getpids: u64,
    /// Unknown syscall numbers answered with `-ENOSYS`.
    pub unknown: u64,
    /// Bad user buffers answered with `-EFAULT`.
    pub faults: u64,
}

static WRITES: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);
static YIELDS: AtomicU64 = AtomicU64::new(0);
static GETPIDS: AtomicU64 = AtomicU64::new(0);
static UNKNOWN: AtomicU64 = AtomicU64::new(0);
static FAULTS: AtomicU64 = AtomicU64::new(0);

/// A snapshot of the syscall counters (all relaxed: they are evidence, not
/// synchronisation).
pub fn stats() -> Stats {
    Stats {
        writes: WRITES.load(Ordering::Relaxed),
        bytes: BYTES.load(Ordering::Relaxed),
        yields: YIELDS.load(Ordering::Relaxed),
        getpids: GETPIDS.load(Ordering::Relaxed),
        unknown: UNKNOWN.load(Ordering::Relaxed),
        faults: FAULTS.load(Ordering::Relaxed),
    }
}

/// Handle one `ecall` from U-mode. The result is placed in the trap frame's
/// `a0`, which the trap epilogue restores before `sret`.
///
/// `exit` and a bad trap never return here; everything else does, and the same
/// frame carries the answer back to the user instruction after `ecall`.
pub fn dispatch(frame: &mut TrapFrame) {
    let number = frame.a7;
    let result = match number {
        SYS_WRITE => write(frame.a0, frame.a1, frame.a2),
        SYS_EXIT => crate::user::exit_current(frame.a0 as u32),
        SYS_SCHED_YIELD => {
            crate::task::scheduler::yield_now();
            YIELDS.fetch_add(1, Ordering::Relaxed);
            0
        }
        SYS_GETPID => {
            GETPIDS.fetch_add(1, Ordering::Relaxed);
            crate::task::scheduler::current_id() as isize
        }
        _ => {
            UNKNOWN.fetch_add(1, Ordering::Relaxed);
            ENOSYS
        }
    };
    frame.a0 = result as usize;
    // `sret` resumes at the saved `sepc`, which still points at the `ecall`
    // instruction. A returning syscall resumes at the instruction after it;
    // `exit` never reaches this line (it ends the task), so one increment
    // serves every call that returns.
    frame.sepc = frame.sepc.wrapping_add(4);
}

/// `write(fd, buf, len)`: fd 1/2 to the console, with the whole buffer validated
/// against the current map first.
fn write(fd: usize, buf: usize, len: usize) -> isize {
    if fd != 1 && fd != 2 {
        return EBADF;
    }
    if len == 0 {
        return 0;
    }
    let Some(end) = buf.checked_add(len) else {
        FAULTS.fetch_add(1, Ordering::Relaxed);
        return EFAULT;
    };
    if !user_range_mapped(buf, end) {
        FAULTS.fetch_add(1, Ordering::Relaxed);
        return EFAULT;
    }

    // SUM is the supervisor's licence to touch a U=1 page. It is scoped to the
    // copy: validation above reads the page tables, not user memory, and no
    // user code can run while this handler is on the CPU.
    let was = csr::read_sstatus();
    core::sync::atomic::compiler_fence(Ordering::SeqCst);
    csr::write_sstatus(was | csr::SSTATUS_SUM);
    core::sync::atomic::compiler_fence(Ordering::SeqCst);

    let mut offset = 0usize;
    let mut chunk = [0u8; COPY_CHUNK];
    while offset < len {
        let va = buf + offset;
        let page_end = (va & !(FRAME_SIZE - 1)) + FRAME_SIZE;
        let take = (len - offset).min(COPY_CHUNK).min(page_end - va);
        // SAFETY: the range `[buf, end)` was validated page by page above: every
        // page has a live leaf with U=1 and R=1, so reading through `va` cannot
        // fault, and `take` stops at the page boundary. `SUM` is set for exactly
        // this window. The destination is a local array, disjoint from user
        // memory by construction.
        unsafe {
            core::ptr::copy_nonoverlapping(va as *const u8, chunk.as_mut_ptr(), take);
        }
        crate::console::write_bytes(&chunk[..take]);
        offset += take;
    }

    core::sync::atomic::compiler_fence(Ordering::SeqCst);
    csr::write_sstatus(was);
    core::sync::atomic::compiler_fence(Ordering::SeqCst);

    WRITES.fetch_add(1, Ordering::Relaxed);
    BYTES.fetch_add(len as u64, Ordering::Relaxed);
    len as isize
}

/// True when every page byte in `[start, end)` is mapped for user reads.
///
/// The last byte is `end - 1`; an end equal to a page boundary does not require
/// the next page. `paging::flags` walks the same tables the hardware will, and a
/// leaf without `U` (a kernel page, a device window) is not a user buffer even if
/// it is mapped supervisor-RW.
fn user_range_mapped(start: usize, end: usize) -> bool {
    let mut page = start & !(FRAME_SIZE - 1);
    while page < end {
        let Some(flags) = paging::flags(page) else {
            return false;
        };
        let bits = flags.bits();
        if bits & PTE_U == 0 || bits & PTE_R == 0 {
            return false;
        }
        page += FRAME_SIZE;
    }
    true
}
