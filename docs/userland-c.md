# C runtime and dynamic linking on pagh-riscv

The kernel loads static ELF64 programs. Two userland projects build on the native
ABI and are tested against **0.20.0+**:

## pagh-libc

[pagh-libc](https://github.com/ccclosed/pagh-libc) is a small C runtime:
`crt0`, syscall wrappers, `malloc`, `stdio`/`FILE`, `string`, `dirent`, `time`,
pthreads (clone/futex), signals, process-local `chdir`/`getcwd`, `fork`/`exec`/
`waitpid` and a buffered `fopen` family. It builds static ELFs at `0x10000` with
the xPack GCC 15.2 RISC-V toolchain (`./build.sh`), copies them into an ext2
image and runs them with `run /mnt/<name>`.

Live: `hello`, `cat`, `ls`, `env`, `threads` (counter=8), `signals`
(SIGUSR1 hits=1), `cwd` (relative open after `chdir`), `fileio` (two-line FILE
round-trip), `mutex` (4 threads -> 1000) and `proc` (fork+exec+wait).

## ld-pagh (dynamic linking)

`pagh-libc` also ships a userland dynamic loader:

- `libpagh-c.so` — shared libc (ET_DYN, LLD `-shared`);
- `hello_dyn.elf` — PIE executable with `DT_NEEDED libextra.so` +
  `libpagh-c.so`;
- `ld-pagh` — static loader: maps ET_DYN objects, resolves `.dynsym` across
  them, applies `R_RISCV_RELATIVE`/`R_RISCV_64`/`R_RISCV_JUMP_SLOT`, sets W^X
  page permissions, runs `DT_INIT`/`DT_INIT_ARRAY` constructors and calls
  `e_entry`.

```text
pagh:rv64> run /mnt/ld-pagh /mnt/hello_dyn
hello from a dynamically linked C program
argc=1 argv0=/mnt/hello_dyn extra=42
run: /mnt/ld-pagh exited with code 0
```

Limits: no TLS, symbol versioning, lazy binding or `dlopen`; the kernel itself
does not implement `PT_INTERP` — the loader is an ordinary static program.

## ABI notes that matter for C

- entry: `a0=argc`, `a1=argv`; envp is computed as `argv + (argc+1)*8` (a2 is
  not reliable);
- `sp` is 16-byte aligned; the stack starts at 64 KiB and grows on demand up to
  1 MiB per process (a load/store fault just below the current bottom maps the
  missing page); deeper recursion dies with a clean store page fault;
- `stat` is 24 bytes (`mode u32, kind u32, size u64, ino u64`);
- dirents: `ino u64`, `kind u8`, `name_len u8`, pad, name; records are
  `align8(12 + name_len)`;
- no `chdir` syscall: libc keeps a process-local cwd and rewrites relative paths;
- no `SIGCHLD`: `wait4` is a direct syscall; the shell reaps;
- fork is a deep copy (no COW), sockets are `-ENOTSUP` in the child.
