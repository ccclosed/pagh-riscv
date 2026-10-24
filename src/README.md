# `src/` — карта кодовой базы ядра pagh-riscv

Крейт `pagh`: **бинарный**, `#![no_std]` + `#![no_main]`
(`src/main.rs:10-11`), без единой зависимости (`Cargo.toml:8-9`), один таргет —
`riscv64gc-unknown-none-elf` (`.cargo/config.toml:2`). `alloc` подключён
(`extern crate alloc`, `src/main.rs:16`) и опирается на `#[global_allocator]`
в корне крейта (`src/main.rs:35-36`) — то есть на кучу из `src/memory/heap.rs`.
Линкуется `linker.ld`, точка входа `_start` на `0x8020_0000`. Всё, что
компилируется в это ядро, лежит в этом каталоге; чужих крейтов нет.

## Карта

| Путь | Роль |
|---|---|
| `main.rs` | `kmain`: фазы загрузки по порядку, баннер, `#[global_allocator]`, `heap_stats`, `panic_handler` |
| `arch/mod.rs` | фасад «спроси факт о машине здесь, а не в регистре» |
| `arch/riscv64/boot.rs` | `_start`: стек, парковка hart'ов, вызов `kmain`; `BOOT_HART_ID` |
| `arch/riscv64/csr.rs` | типизированный доступ к супервизорным CSR |
| `arch/riscv64/sbi.rs` | `ecall` в M-mode: SRST (выключение) |
| `arch/riscv64/traps.rs` | `stvec`, `trap_entry`, кадр на 264 байта, диспетчер |
| `arch/riscv64/mmu.rs` | примитивы Sv39: флаги PTE, `PageTable`, `vpn`, `satp_for`, `activate`, `sfence`; **пока никем не вызывается** (`satp` не пишется) |
| `fdt.rs` | минимальный читатель flattened device tree |
| `memory/mod.rs` | фасад памяти: `init`, `stats`, `kernel_end`, `boot_stack`, `fdt_span` |
| `memory/pmm.rs` | битовая карта физических кадров 4 KiB (окно 4 GiB) |
| `memory/heap.rs` | free-list внутри 64 KiB-чанков, `GlobalAlloc` |
| `memory/selftest.rs` | четыре неразрушающие проверки памяти для команды `selftest` |
| `drivers/mod.rs` | фасад драйверов |
| `drivers/uart.rs` | NS16550: polled-консоль |
| `drivers/fw_cfg.rs` | QEMU fw_cfg: селектор/данные + DMA |
| `drivers/ramfb.rs` | конфигурация устройства ramfb |
| `drivers/framebuffer.rs` | текстовый экран: тень, шрифт покрытия, статус-бар |
| `console.rs` | `kprint!`/`kprintln!`: один текст — два стока (UART и экран) |
| `shell.rs` | цикл консоли, редактор строки, команды |
| `sync.rs` | один нереентерабельный спинлок |

Подкаталоги документируют себя сами: [`arch/README.md`](arch/README.md),
[`drivers/README.md`](drivers/README.md), [`memory/README.md`](memory/README.md).
Устройство `src/memory/**` (PMM, куча, selftest, пределы, чего нет) — в README
подсистемы; `docs/architecture.md` (разделы 6–7) добавляет к нему то, что нужно
знать при чтении boot-пути: карту памяти и порядок фаз.

## Последовательность загрузки (фазы идут строго по порядку)

| # | Фаза | Код | Почему здесь |
|---|---|---|---|
| 1 | `_start`: hart 0 продолжает, остальные — в `wfi` | `arch/riscv64/boot.rs:13-16, 33-38` | до любого Rust-кода, поэтому нет гонок между hart'ами |
| 2 | стек `sp = __stack_top`, 64 KiB в `.bss`, выравнивание 16 | `arch/riscv64/boot.rs:18-21, 44-54` | на входе `sp` произвольный |
| 3 | hart id → `BOOT_HART_ID` | `arch/riscv64/boot.rs:23-28` | `mhartid` из S-mode недоступен, а `a0` ещё цел |
| 4 | `call kmain(a0, a1)` | `arch/riscv64/boot.rs:30-31` | аргументы уже в нужных регистрах |
| 5 | `console::init()`: UART, затем попытка поднять экран (`framebuffer::init`) | `main.rs:53`, `console.rs:44-48` | без UART не видно, почему нет экрана; отсутствие экрана — не ошибка |
| 6 | баннер: hart, адрес DTB | `main.rs:55-60` | — |
| 7 | разбор DTB, ошибка фатальна | `main.rs:65-72`, `fdt.rs:64-98` | описание машины нельзя угадывать |
| 8 | память, timebase и адрес консоли — в лог | `main.rs:73-81` | — |
| 9 | PMM: пул кадров минус образ ядра, стек и DTB; печать статистики | `main.rs:83-107`, `memory/mod.rs:75-77` | `alloc` уже существует — аллокатор должен быть готов до первой аллокации |
| 10 | `traps::init()`: `stvec` + `sstatus.SIE` | `main.rs:109-111`, `arch/riscv64/traps.rs:151-155` | ловушки раньше того, что может сбойнуть |
| 11 | `shell::run(tree)`, не возвращается | `main.rs:113`, `shell.rs:19-59` | возвращаться некуда: планировщика нет |

Исходы вне фаз: `panic_handler` печатает `[PANIC]` и паркует hart
(`main.rs:116-130`); `halt` в шелле уходит в SBI SRST и выключает машину
(`shell.rs:221-227`, `arch/riscv64/sbi.rs:24-38`).

## Чего в `src/` нет

Ни одного из модулей x86-версии, кроме перечисленных выше: нет `vmm`/Sv39
(`satp` не пишется), `fs/`, `vfs/`, `task/`, `net/`, `pkg/`, `security/`, `debug/`,
`log.rs`, `provision.rs`, `selftest_lx.rs` (Linux-compat). Нет host-tests: тесты,
которые есть, живут внутри ядра — `memory/selftest.rs` (команда `selftest`).
Подробный список — `README.md` → «Чего нет».

## Сборка и проверки

| Что | Команда | Артефакт |
|---|---|---|
| debug | `cargo build --locked` | `target/riscv64gc-unknown-none-elf/debug/pagh` |
| release | `cargo build --locked --release` | `target/riscv64gc-unknown-none-elf/release/pagh` |
| smoke (serial) | `python3 .github/smoke.py <kernel>` | exit 0 + серийный лог |
| smoke (+экран) | `python3 .github/smoke.py <kernel> --ramfb` | то же, с `-device ramfb` |
| память в госте | команда `selftest` | `SELFTEST SUMMARY: 4/4 passed` |

Оба профиля: `panic = "abort"` (`Cargo.toml:10-14`), release дополнительно
`opt-level = "s"`, `lto = true` (`Cargo.toml:15-16`). Panic — это смерть машины:
размотки нет, лог — единственная улика. Строки, которые печатает код из этого
каталога, построчно проверяет `.github/smoke.py`; менять их нужно вместе с
драйвером.