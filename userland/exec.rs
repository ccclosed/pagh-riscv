//! The `execve` target: a tiny static program the ABI fixture replaces itself
//! with. It exits 0 only when the kernel passed the expected `argv`/`envp`
//! frame; any mismatch is a distinct non-zero code.
//!
//! Built by `userland/build.sh` into `exec-child.elf`, embedded in the kernel
//! (`user::EXEC_CHILD_ELF`) and written by the selftest to `/mnt/exec-child`.

#![no_std]
#![no_main]

core::arch::global_asm!(
    r#"
    .section .text.entry, "ax", @progbits
    .globl _start
_start:
    /* envp = argv + (argc + 1) * 8 */
    addi    t0, a0, 1
    slli    t0, t0, 3
    add     a2, a1, t0
    call    exec_main
    li      a7, 93
    ecall
1:  j       1b
"#
);

fn syscall3(number: usize, a0: usize, a1: usize, a2: usize) -> isize {
    let result: isize;
    // SAFETY: the kernel reads only the registered arguments and returns in a0.
    unsafe {
        core::arch::asm!(
            "ecall",
            in("a7") number,
            inlateout("a0") a0 => result,
            in("a1") a1,
            in("a2") a2,
        )
    };
    result
}

fn write(text: &[u8]) {
    let _ = syscall3(64, 1, text.as_ptr() as usize, text.len());
}

/// NUL-terminated UTF-8 comparison against `expected`.
fn same(text: *const u8, expected: &[u8]) -> bool {
    if text.is_null() {
        return false;
    }
    let mut index = 0usize;
    for &want in expected {
        // SAFETY: the kernel built the entry stack; a string there is NUL
        // terminated inside the mapped stack, which this loop stops at.
        if unsafe { *text.add(index) } != want {
            return false;
        }
        index += 1;
    }
    // SAFETY: the terminator is inside the same string.
    unsafe { *text.add(index) == 0 }
}

/// True when `envp` contains `needle` (a full `NAME=value` string).
fn env_has(envp: *const usize, needle: &[u8]) -> bool {
    for index in 0..8 {
        // SAFETY: the kernel built a NULL-terminated envp table on this
        // process's stack; the loop stops at that NULL.
        let pointer = unsafe { *envp.add(index) } as *const u8;
        if pointer.is_null() {
            return false;
        }
        if same(pointer, needle) {
            return true;
        }
    }
    false
}

#[no_mangle]
pub extern "C" fn exec_main(argc: usize, argv: *const usize, envp: *const usize) -> i32 {
    if argc < 2 || argv.is_null() || envp.is_null() {
        return 91;
    }
    // SAFETY: `argv[0..argc]` and `envp[0]` are the pointer tables the kernel
    // built at the top of this process's stack; argc came from the same frame.
    let (arg0, arg1, env0) = unsafe {
        (
            *argv as *const u8,
            *argv.add(1) as *const u8,
            *envp as *const u8,
        )
    };
    if !same(arg0, b"exec-child") || !same(arg1, b"exec-arg") {
        return 92;
    }
    if !same(env0, b"PWD=/mnt") {
        return 93;
    }
    write(b"abi: exec ok argc=2 argv=exec-arg env=PWD=/mnt\n");
    // The optional second invocation proves `execve` resets the signal table:
    // the parent ignored SIGUSR1 before forking, so if the ignore survived the
    // exec this self-kill is a no-op and the process exits 0. With the required
    // reset to SIG_DFL the process dies from signal 10 instead.
    if env_has(envp, b"PAGH_EXEC_SIGNAL=1") {
        let pid = syscall3(172, 0, 0, 0);
        let _ = syscall3(129, pid as usize, 10, 0);
    }
    0
}
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // A panic in this fixture is a failure: exit 94 rather than loop forever, so
    // the selftest can tell it from a bad argv frame.
    let _ = syscall3(93, 94, 0, 0);
    loop {
        core::hint::spin_loop();
    }
}
