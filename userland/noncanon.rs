//! F-r2-rv-1 regression fixture: a non-canonical user pointer.
//!
//! `user_range_valid` used to walk the page tables with only the low 39 bits of
//! the address (`mmu::vpn` keeps nine bits per level), so `0xdead_0000_0000_0000
//! | path` validated against the mapped `path` page. The copy that followed
//! dereferenced the full 64-bit value, raised an S-mode load page fault
//! (`SPP = 1`) and the trap handler parked the hart: one `ecall` killed the
//! machine.
//!
//! On a fixed kernel both calls are ordinary syscalls: the canonical control
//! opens the file, the non-canonical one answers `-EFAULT`, and the process
//! exits with that value (the shell prints `exited with code 4294967282`). On a
//! vulnerable kernel the second call prints `[EXC] load page fault` and
//! `[EXC] halting` instead — the machine is gone, so the harness is what turns
//! that difference into a pass/fail. `tools/test_noncanon.py` is that harness.
#![no_std]
#![no_main]

unsafe fn sys(n: usize, a0: usize, a1: usize, a2: usize) -> isize {
    let ret: isize;
    core::arch::asm!(
        "ecall",
        inlateout("a0") a0 => ret,
        in("a1") a1, in("a2") a2,
        in("a7") n,
        options(nostack)
    );
    ret
}

fn print(text: &[u8]) {
    unsafe { sys(64, 1, text.as_ptr() as usize, text.len()) };
}

fn print_num(value: isize) {
    let mut buffer = [0u8; 24];
    let negative = value < 0;
    let mut digits = if negative {
        (value as i128).unsigned_abs()
    } else {
        value as u128
    };
    let mut index = buffer.len();
    if digits == 0 {
        index -= 1;
        buffer[index] = b'0';
    }
    while digits > 0 {
        index -= 1;
        buffer[index] = b'0' + (digits % 10) as u8;
        digits /= 10;
    }
    if negative {
        index -= 1;
        buffer[index] = b'-';
    }
    unsafe {
        sys(
            64,
            1,
            buffer.as_ptr().add(index) as usize,
            buffer.len() - index,
        )
    };
}

static PATH: [u8; 16] = *b"/mnt/hello.elf\0\0";

/// Sv39 hole (bits 63..39 do not repeat bit 38), OR'd over a mapped user
/// address so only the canonicality check can reject it.
const NONCANONICAL: usize = 0xdead_0000_0000_0000;

#[no_mangle]
pub extern "C" fn poc_main() -> ! {
    // Control: the same path without the prefix is a normal open on both
    // kernels, which is what proves the fault comes from the prefix alone.
    let canonical = unsafe { sys(56, (-100isize) as usize, PATH.as_ptr() as usize, 0) };
    print(b"C: canonical openat fd=");
    print_num(canonical);
    print(b"\n");

    let alias = NONCANONICAL | PATH.as_ptr() as usize;
    print(b"F-r2-rv-1: openat(non-canonical path)\n");
    let result = unsafe { sys(56, (-100isize) as usize, alias, 0) };
    print(b"N: non-canonical openat ret=");
    print_num(result);
    print(b"\n");
    unsafe { sys(93, result as usize, 0, 0) };
    loop {}
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

core::arch::global_asm!(
    r#"
    .section .text.entry, "ax", @progbits
    .globl _start
_start:
    call poc_main
1:  j 1b
"#
);