//! Trap entry: everything the CPU does when it is not executing the kernel's
//! next instruction.
//!
//! Two rules make this file short and honest:
//!
//! * **Nothing panics here.** `panic = "abort"` means a panic inside a trap
//!   handler kills the machine while the trap frame is still the only evidence;
//!   the handler prints and stops instead.
//! * **The frame layout is the ABI.** `trap_entry` saves 32 registers in a fixed
//!   order and passes the frame pointer to Rust; the offsets in `TrapFrame` are
//!   asserted against that order by the tests.

use core::arch::global_asm;
use core::sync::atomic::{AtomicU64, Ordering};

/// The register file as `trap_entry` saves it.
///
/// Order is the save order in the assembly below (x1..x31 then the CSRs), not the
/// architectural register numbering, because that is what the offsets can be
/// checked against.
#[repr(C)]
pub struct TrapFrame {
    pub ra: usize,
    pub sp: usize,
    pub gp: usize,
    pub tp: usize,
    pub t0: usize,
    pub t1: usize,
    pub t2: usize,
    pub s0: usize,
    pub s1: usize,
    pub a0: usize,
    pub a1: usize,
    pub a2: usize,
    pub a3: usize,
    pub a4: usize,
    pub a5: usize,
    pub a6: usize,
    pub a7: usize,
    pub s2: usize,
    pub s3: usize,
    pub s4: usize,
    pub s5: usize,
    pub s6: usize,
    pub s7: usize,
    pub s8: usize,
    pub s9: usize,
    pub s10: usize,
    pub s11: usize,
    pub t3: usize,
    pub t4: usize,
    pub t5: usize,
    pub t6: usize,
    pub sepc: usize,
    pub sstatus: usize,
}

global_asm!(
    r#"
    .section .text
    .align 4
    .globl trap_entry
trap_entry:
    addi    sp, sp, -264
    sd      ra,   0(sp)
    sd      sp,   8(sp)
    sd      gp,  16(sp)
    sd      tp,  24(sp)
    sd      t0,  32(sp)
    sd      t1,  40(sp)
    sd      t2,  48(sp)
    sd      s0,  56(sp)
    sd      s1,  64(sp)
    sd      a0,  72(sp)
    sd      a1,  80(sp)
    sd      a2,  88(sp)
    sd      a3,  96(sp)
    sd      a4, 104(sp)
    sd      a5, 112(sp)
    sd      a6, 120(sp)
    sd      a7, 128(sp)
    sd      s2, 136(sp)
    sd      s3, 144(sp)
    sd      s4, 152(sp)
    sd      s5, 160(sp)
    sd      s6, 168(sp)
    sd      s7, 176(sp)
    sd      s8, 184(sp)
    sd      s9, 192(sp)
    sd      s10,200(sp)
    sd      s11,208(sp)
    sd      t3, 216(sp)
    sd      t4, 224(sp)
    sd      t5, 232(sp)
    sd      t6, 240(sp)
    csrr    t0, sepc
    sd      t0, 248(sp)
    csrr    t0, sstatus
    sd      t0, 256(sp)

    mv      a0, sp
    call    trap_dispatch

    ld      t0, 248(sp)
    csrw    sepc, t0
    ld      t0, 256(sp)
    csrw    sstatus, t0
    ld      ra,   0(sp)
    ld      gp,  16(sp)
    ld      tp,  24(sp)
    ld      t0,  32(sp)
    ld      t1,  40(sp)
    ld      t2,  48(sp)
    ld      s0,  56(sp)
    ld      s1,  64(sp)
    ld      a0,  72(sp)
    ld      a1,  80(sp)
    ld      a2,  88(sp)
    ld      a3,  96(sp)
    ld      a4, 104(sp)
    ld      a5, 112(sp)
    ld      a6, 120(sp)
    ld      a7, 128(sp)
    ld      s2, 136(sp)
    ld      s3, 144(sp)
    ld      s4, 152(sp)
    ld      s5, 160(sp)
    ld      s6, 168(sp)
    ld      s7, 176(sp)
    ld      s8, 184(sp)
    ld      s9, 192(sp)
    ld      s10,200(sp)
    ld      s11,208(sp)
    ld      t3, 216(sp)
    ld      t4, 224(sp)
    ld      t5, 232(sp)
    ld      t6, 240(sp)
    addi    sp, sp, 264
    sret
"#
);

/// Cause codes this kernel knows by name (`scause`).
pub const CAUSE_ILLEGAL_INSTRUCTION: usize = 2;
pub const CAUSE_BREAKPOINT: usize = 3;
pub const CAUSE_LOAD_PAGE_FAULT: usize = 13;
pub const CAUSE_STORE_PAGE_FAULT: usize = 15;
pub const CAUSE_INTERRUPT_BIT: usize = 1 << 63;
pub const CAUSE_SUPERVISOR_TIMER: usize = 5;
pub const CAUSE_SUPERVISOR_EXTERNAL: usize = 9;

/// Bitmap of interrupt causes already reported as unhandled (bit = cause & 63),
/// so a source nobody expects names itself once instead of flooding the console.
static UNHANDLED_INTERRUPTS: AtomicU64 = AtomicU64::new(0);

/// Install `trap_entry` as the trap vector and enable supervisor interrupts.
///
/// This only opens the door: `sie` begins at 0, so no interrupt can arrive until
/// the driver that owns a source enables its bit *after* it has programmed the
/// source (see `timer::init`, `plic::init`). That ordering is the whole reason
/// `SIE` can be set here, early, without a race.
pub fn init() {
    crate::arch::riscv64::csr::write_stvec(trap_entry as usize);
    crate::arch::riscv64::csr::enable_interrupts();
}

extern "C" {
    fn trap_entry();
}

/// Called from `trap_entry` with the saved register file.
#[no_mangle]
pub extern "C" fn trap_dispatch(frame: *mut TrapFrame) {
    // SAFETY: `trap_entry` passes the pointer to the frame it just built on this
    // hart's stack, and it stays alive for the duration of this call.
    let frame = unsafe { &mut *frame };
    let scause = crate::arch::riscv64::csr::read_scause();
    let sepc = crate::arch::riscv64::csr::read_sepc();
    let stval = crate::arch::riscv64::csr::read_stval();

    if scause & CAUSE_INTERRUPT_BIT != 0 {
        let code = scause & !CAUSE_INTERRUPT_BIT;
        match code {
            // The timer is acknowledged by pointing firmware at the next absolute
            // deadline; there is no separate EOI. Everything the handler does is
            // bounded and silent (see `timer::on_tick` for the contract).
            CAUSE_SUPERVISOR_TIMER => crate::arch::riscv64::timer::on_tick(),
            // Claim, dispatch and complete; silent and bounded (see `plic`).
            CAUSE_SUPERVISOR_EXTERNAL => crate::arch::riscv64::plic::handle_external_interrupt(),
            _ => {
                // Reported once per cause: an interrupt is *enabled*, so a handler
                // that printed on every occurrence would turn a stray source into a
                // console flood that hides the message it is printing.
                let bit = 1u64 << (code & 63);
                if UNHANDLED_INTERRUPTS.fetch_or(bit, Ordering::Relaxed) & bit == 0 {
                    crate::kprintln!("[EXC] interrupt cause {code} (unhandled, reported once)");
                }
            }
        }
        // Only an interrupt returns: the epilogue below is `sret`, restoring the
        // interrupted context exactly where it stopped.
        return;
    }

    let name = match scause {
        CAUSE_ILLEGAL_INSTRUCTION => "illegal instruction",
        CAUSE_BREAKPOINT => "breakpoint",
        CAUSE_LOAD_PAGE_FAULT => "load page fault",
        CAUSE_STORE_PAGE_FAULT => "store page fault",
        _ => "exception",
    };
    // Serial only from here on: the exception may have been taken while the
    // framebuffer's writer lock was held (a fault inside the screen code), and a
    // console write that tried to draw would deadlock instead of logging.
    crate::console::serial_only();
    crate::kprintln!();
    crate::kprintln!("[EXC] {name} (scause={scause}, stval={stval:#x}, sepc={sepc:#x})");
    crate::kprintln!("[EXC] ra={:#x} sp={:#x}", frame.ra, frame.sp);
    // No way to continue safely: a fault in the kernel means the state that
    // caused it is still there. Print, then stop this hart.
    crate::kprintln!("[EXC] halting");
    crate::arch::riscv64::park_forever()
}
