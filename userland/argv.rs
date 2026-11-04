//! The entry-stack fixture: proves that the kernel hands a program the
//! SysV-style `argc`/`argv`/`envp` frame the native ABI promises.
//!
//! The kernel enters U-mode with `a0 = argc`, `a1 = argv` and `sp` pointing at
//! `[argc][argv0..argvN-1][NULL][env0..envK-1][NULL][strings...]`. This program
//! is run from the selftest with `argv = [argv.elf, alpha, beta]` and
//! `env = [PWD=/]`; it validates every word of that frame (including that the
//! frame is 16-byte aligned and that `envp` is where `_start` will look for it),
//! prints exactly one line, and exits 0. A wrong stack is an exit code and a
//! FAIL, never a hang.
//!
//! All of it is `no_std` Rust over raw `ecall`s: the point is the ABI, not a
//! runtime. Build with `userland/build.sh` (see `userland/README.md`).

#![no_std]
#![no_main]

core::arch::global_asm!(
    r#"
    .section .text.entry, "ax", @progbits
    .globl _start
_start:
    /* a0 = argc, a1 = argv are already the kernel's entry registers; hand the
       initial sp over as the third argument so the fixture can check the frame
       layout, not only the two registers. */
    mv      a2, sp
    call    argv_main

    /* exit(argv_main(...)): the return code is the verdict. */
    li      a7, 93
    ecall

    /* exit(2) never returns; park if it somehow does. */
1:  j       1b
"#
);

/// `ecall` with the RISC-V Linux register convention (`a7` number, `a0..a2`
/// arguments, result in `a0`). The kernel returns `-errno` as a negative `isize`.
fn syscall(number: usize, a0: usize, a1: usize, a2: usize) -> isize {
    let result: isize;
    // SAFETY: `ecall` is the only door from U-mode into the kernel; it takes no
    // memory operand the compiler must preserve beyond the named registers, and
    // every argument is a plain integer. The kernel returns with the same `sp`
    // and callee-saved state it entered with, so the caller's frame is intact.
    unsafe {
        core::arch::asm!(
            "ecall",
            in("a7") number,
            inlateout("a0") a0 => result,
            in("a1") a1,
            in("a2") a2,
            options(nostack)
        )
    };
    result
}

/// `write(2)` to fd 1, retrying short writes. Returns false on any error.
fn write_bytes(bytes: &[u8]) -> bool {
    let mut offset = 0usize;
    while offset < bytes.len() {
        let written = syscall(
            64,
            1,
            bytes[offset..].as_ptr() as usize,
            bytes.len() - offset,
        );
        if written <= 0 {
            return false;
        }
        offset += written as usize;
    }
    true
}

fn write_str(text: &str) -> bool {
    write_bytes(text.as_bytes())
}

/// Print an unsigned decimal without `core::fmt` (no allocator here).
fn write_usize(mut value: usize) {
    let mut digits = [0u8; 20];
    let mut index = digits.len();
    loop {
        index -= 1;
        digits[index] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    let _ = write_bytes(&digits[index..]);
}

/// Print a NUL-terminated string, at most `limit` bytes (a wrong pointer must
/// fail the test, not run off the stack).
fn write_cstr(pointer: *const u8, limit: usize) {
    let mut length = 0usize;
    while length < limit {
        // SAFETY: the caller only passes pointers out of the entry frame, and
        // `length` is bounded before the read; a missing NUL simply stops at
        // `limit`.
        let byte = unsafe { *pointer.add(length) };
        if byte == 0 {
            break;
        }
        // SAFETY: as above: one byte inside the string, read before the bound.
        let one = unsafe { core::slice::from_raw_parts(pointer.add(length), 1) };
        let _ = write_bytes(one);
        length += 1;
    }
}

/// Compare a NUL-terminated string with `expected`.
fn cstr_eq(pointer: *const u8, expected: &[u8]) -> bool {
    for (index, byte) in expected.iter().enumerate() {
        // SAFETY: as in `write_cstr`: the pointer is an entry-frame string and
        // the read is bounded by the caller's expected literal.
        if unsafe { *pointer.add(index) } != *byte {
            return false;
        }
    }
    // SAFETY: one byte past the literal is the NUL a matching string has.
    unsafe { *pointer.add(expected.len()) == 0 }
}

/// The fixture body. Returns the exit code the selftest asserts:
/// 0 = the frame is exactly the ABI's, everything else names the broken word.
#[no_mangle]
pub extern "C" fn argv_main(argc: usize, argv: *const *const u8, sp: usize) -> i32 {
    // SAFETY: the kernel's entry contract guarantees `argv` points at `argc`
    // non-NULL string pointers followed by a NULL, and `sp` at the frame base.
    let argv0 = unsafe { *argv };
    let argv1 = if argc > 1 {
        // SAFETY: `argc > 1` means `argv[1]` is inside the table.
        unsafe { *argv.add(1) }
    } else {
        core::ptr::null()
    };
    let argv2 = if argc > 2 {
        // SAFETY: as above for `argv[2]`.
        unsafe { *argv.add(2) }
    } else {
        core::ptr::null()
    };
    // SAFETY: the table's NULL terminator is `argv[argc]`.
    let argv_end = unsafe { *argv.add(argc) };
    // `envp` is the word after the argv NULL terminator.
    let envp = unsafe { argv.add(argc + 1) };
    let env0 = unsafe { *envp };
    let env1 = unsafe { *envp.add(1) };

    // ── the layout checks (exit codes are the contract) ──
    let mut code = 0;
    if argc != 3 {
        code = 1;
    } else if !cstr_eq(argv0, b"argv.elf") {
        code = 2;
    } else if !cstr_eq(argv1, b"alpha") {
        code = 3;
    } else if !cstr_eq(argv2, b"beta") {
        code = 4;
    } else if !argv_end.is_null() {
        code = 5;
    } else if !cstr_eq(env0, b"PWD=/") || !env1.is_null() {
        code = 6;
    } else if sp & 0xf != 0 {
        code = 7;
    } else if unsafe { *(sp as *const usize) } != argc {
        code = 8;
    } else if argv as usize != sp + core::mem::size_of::<usize>()
        || envp as usize != sp + (argc + 2) * core::mem::size_of::<usize>()
    {
        code = 9;
    }

    // The line the selftest compares verbatim. It is printed only after the
    // checks above, so a wrong frame fails without producing a plausible line.
    if code == 0 {
        let printed = write_str("argv: argc=")
            && {
                write_usize(argc);
                true
            }
            && write_str(" argv[0]=")
            && {
                write_cstr(argv0, 32);
                true
            }
            && write_str(" argv[1]=")
            && {
                write_cstr(argv1, 32);
                true
            }
            && write_str(" argv[2]=")
            && {
                write_cstr(argv2, 32);
                true
            }
            && write_str(" env[0]=")
            && {
                write_cstr(env0, 32);
                true
            }
            && write_str("\n");
        if !printed {
            code = 10;
        }
    }
    code
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}