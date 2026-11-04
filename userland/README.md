# `userland/` — статические RV64-программы для M6b и native ABI

Пять крошечных программ без рантайма и libc — ровно столько, чтобы проверить
точки входа ядра и контракт `pagh-api/docs/abi.md`. Все собраны в ELF заранее;
ядро встраивает их через `include_bytes!` (`src/user/mod.rs`), поэтому чистый
checkout собирается **без** кросс-компилятора.

| Файл | Что делает | Что доказывает |
|---|---|---|
| `hello.rs` / `hello.elf` | `write(1, "hello from user mode\n")`, `sched_yield`, читает `.data` + `.bss`, `exit(42)` | вход в U-mode, два PT_LOAD (rw data/bss), `ecall`-ABI, возврат из `sched_yield` в U |
| `fault.rs` / `fault.elf` | `getpid`, затем `ld` с адреса 0 | fault из U убивает задачу, а не машину; `run_image` возвращает `Exit::Fault` |
| `spin.rs` / `spin.elf` | `rdtime`-цикл на 0.2 s (~20 тиков), `getpid`, `exit(7)` | таймерное прерывание, взятое в U-mode, обрабатывается и возвращается в U |
| `argv.rs` / `argv.elf` | проверяет `a0=argc`, `a1=argv`, `sp`, `envp = argv+argc+1`, NULL-терминаторы и выравнивание; печатает `argv: argc=3 argv[0]=argv.elf argv[1]=alpha argv[2]=beta env[0]=PWD=/` | SysV-кадр входа и точную строку, которую selftest сравнивает побайтно |
| `abi.rs` / `abi.elf` | `write`, `openat` (существующий файл, создание, `O_APPEND`, `O_TRUNC`), `read`, `fstat`, `lseek`, `getdents64`, `readlinkat`, `brk` с паттерном, `clock_gettime`, `nanosleep`, `uname`, `getpid`, `close`, `-EFAULT`/`-ENOSYS`/`-EBADF`/`-ENOMEM`; без `/mnt` выходит 77 после no-fs половины | весь native ABI без libpagh: файлы, каталоги, символические ссылки, heap, время, ошибки |
| `user.ld` | линковка: `_start` = `0x1_0000`, PT_LOAD-раскладка | совпадает с окном из `src/user/elf.rs` |

ELF-файлы **закоммичены** — это часть дерева, как шрифт `assets/font8x18.bin`.

## Пересборка

```sh
./userland/build.sh
```

Скрипт вызывает `rustup run nightly-2026-07-12 rustc --target
riscv64gc-unknown-none-elf` с `-C panic=abort -C relocation-model=static
-C opt-level=2 -C strip=debuginfo`, линкует `rust-lld` по `user.ld`. Другой
toolchain: `TOOLCHAIN=nightly-... ./userland/build.sh`. `strip=debuginfo`
обязателен: без него `abi.elf` уносит ~740 KiB debug-секций, которые всё равно
не загружаются и только раздувают `include_bytes!`; на остальные фикстуры флаг
не влияет (проверено хешем).

После изменения `.rs`/`.ld` пересобрать и **закоммитить ELF в том же коммите**:
ядро компилируется с `include_bytes!`, и старые бинари означают, что проверка
больше не соответствует исходнику.

## ABI

- Linux/RISC-V номера: `read=63`, `write=64`, `openat=56`, `close=57`,
  `getdents64=61`, `lseek=62`, `fstat=80`, `readlinkat=78`, `nanosleep=101`,
  `clock_gettime=113`, `uname=160`, `brk=214`, `exit=93`, `sched_yield=124`,
  `getpid=172`. Регистры: `a7` — номер, `a0..a5` — аргументы, `a0` — результат
  (`-errno`); диспетчер — `src/user/syscall.rs`.
- Вход: `a0 = argc`, `a1 = argv`, `sp` — 16-выровненный SysV-кадр
  `[argc][argv…][NULL][envp…][NULL][строки]`; `argv[0]` — путь как набран,
  `envp = argv + argc + 1`. У `hello`/`fault`/`spin`/`abi` `argc = 1`; `argv.elf`
  запускается selftest'ом с тремя аргументами и `PWD=/`.
- Никакого `std`/libc: `core::arch::global_asm!` для `_start` и голые `ecall`.
  `abi.rs`/`argv.rs` — `no_std` Rust без аллокатора; `panic_handler` паркует
  программу, поэтому проверки сделаны возвратными кодами, а не паниками.
- `rdtime` (`csrr …, time`) из U-mode работает потому, что OpenSBI оставляет
  `mcounteren.TM = 1`; это свойство фикстуры, а не обещание ABI ядра.
- Пределы: 16 fd, путь ≤ 256 B, heap ровно 1 MiB (`brk`), `read(0)` неблокирующий
  (`-EAGAIN`), `getdents64` — снимок на `openat`.

## Как это видно из шелла

```text
pagh:rv64> run demo
hello from user mode
run: demo exited with code 42
pagh:rv64> run demo alpha beta     # у фикстуры argc=1: слова игнорируются
hello from user mode
run: demo exited with code 42
pagh:rv64> run fault
[user] fault killed: load page fault (cause=13, stval=0x0, sepc=0x1000a)
run: fault killed: load page fault
pagh:rv64> run spin
run: spin exited with code 7
pagh:rv64> selftest
SELFTEST user-abi-argv PASS
SELFTEST user-abi-syscalls PASS     # с /mnt; без него SKIP
```