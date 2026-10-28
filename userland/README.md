# `userland/` — статические RV64-программы для M6b

Три крошечные программы без рантайма и libc — ровно столько, чтобы проверить
точки входа ядра. Все написаны ассемблером внутри `core::arch::global_asm!` и
собраны в ELF заранее; ядро встраивает их через `include_bytes!`
(`src/user/mod.rs`), поэтому чистый checkout собирается **без** кросс-компилятора.

| Файл | Что делает | Что доказывает |
|---|---|---|
| `hello.rs` / `hello.elf` | `write(1, "hello from user mode\n")`, `sched_yield`, читает `.data` + `.bss`, `exit(42)` | вход в U-mode, два PT_LOAD (rw data/bss), `ecall`-ABI, возврат из `sched_yield` в U |
| `fault.rs` / `fault.elf` | `getpid`, затем `ld` с адреса 0 | fault из U убивает задачу, а не машину; `run_image` возвращает `Exit::Fault` |
| `spin.rs` / `spin.elf` | `rdtime`-цикл на 0.2 s (~20 тиков), `getpid`, `exit(7)` | таймерное прерывание, взятое в U-mode, обрабатывается и возвращается в U |
| `user.ld` | линковка: `_start` = `0x1_0000`, PT_LOAD-раскладка | совпадает с окном из `src/user/elf.rs` |

ELF-файлы **закоммичены** — это часть дерева, как шрифт `assets/font8x18.bin`.

## Пересборка

```sh
./userland/build.sh
```

Скрипт вызывает `rustup run nightly-2026-07-12 rustc --target
riscv64gc-unknown-none-elf` с `-C panic=abort -C relocation-model=static
-C opt-level=2`, линкует `rust-lld` по `user.ld`. Другой toolchain:
`TOOLCHAIN=nightly-... ./userland/build.sh`.

После изменения `.rs`/`.ld` пересобрать и **закоммитить ELF в том же коммите**:
ядро компилируется с `include_bytes!`, и старые бинари означают, что проверка
больше не соответствует исходнику.

## ABI

- Linux/RISC-V: номер syscall в `a7`, аргументы `a0..a2`, результат в `a0`:
  `write=64`, `exit=93`, `sched_yield=124`, `getpid=172`. Диспетчер —
  `src/user/syscall.rs`.
- `_start` — единственная точка входа; `sp` уже указывает на вершину 64 KiB
  user-стека; возврата из `_start` нет — только `exit`.
- Никакого `std`/libc: только `core::arch::global_asm!` и голые `ecall`.
- `rdtime` (`csrr …, time`) из U-mode работает потому, что OpenSBI оставляет
  `mcounteren.TM = 1`; это свойство фикстуры, а не обещание ABI ядра.

## Как это видно из шелла

```text
pagh:rv64> run demo
hello from user mode
run: demo exited with code 42
pagh:rv64> run fault
[user] fault killed: load page fault (cause=13, stval=0x0, sepc=0x1000a)
run: fault killed: load page fault
pagh:rv64> run spin
run: spin exited with code 7
```