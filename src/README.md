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
| `arch/riscv64/traps.rs` | `stvec`, `trap_entry`, кадр на 264 байта, диспетчер исключений и прерываний |
| `arch/riscv64/timer.rs` | тик 100 Гц через SBI TIME: абсолютный дедлайн, `sleep_ticks`, шов `set_tick_handler` для M6 |
| `arch/riscv64/plic.rs` | PLIC из DTB: enable/priority/threshold, claim/complete, окна контекста S-mode |
| `arch/riscv64/selftest.rs` | четыре проверки таймера/PLIC/кольца UART для `selftest` |
| `arch/riscv64/mmu.rs` | примитивы Sv39: флаги PTE, `PageTable`, `vpn`, `satp_for`, `activate`, `sfence` — вызывает `memory::paging` |
| `fdt.rs` | минимальный читатель flattened device tree |
| `memory/mod.rs` | фасад памяти: `init`, `stats`, `kernel_end`, `boot_stack`, `fdt_span` |
| `memory/pmm.rs` | битовая карта физических кадров 4 KiB (окно 8 GiB) |
| `memory/heap.rs` | free-list внутри 64 KiB-чанков, `GlobalAlloc` |
| `memory/selftest.rs` | семь неразрушающих проверок памяти (PMM, Sv39, куча, чанки) для `selftest` |
| `drivers/mod.rs` | фасад драйверов |
| `drivers/uart.rs` | NS16550: приём по прерыванию PLIC в кольцо, передача polled, запасной polled-путь |
| `drivers/fw_cfg.rs` | QEMU fw_cfg: селектор/данные + DMA |
| `drivers/ramfb.rs` | конфигурация устройства ramfb |
| `drivers/framebuffer.rs` | текстовый экран: тень, шрифт покрытия, статус-бар |
| `drivers/block.rs` | шов блочного слоя: трейт `BlockDevice`, один слот реестра, RAM-диск `MemBlock` |
| `drivers/virtio_mmio.rs` | virtio-blk на mmio-транспорте (modern, version 2), polled; окна — из DTB |
| `drivers/virtio_net.rs` | virtio-net (modern, две очереди, polled): MAC, RX/TX-кольца, кадры |
| `drivers/selftest.rs` | проверки блочного слоя (RAM round-trip + read-only реального диска) |
| `fs/mod.rs` | фасад ФС: ext2, политика формата, in-QEMU тесты |
| `fs/ext2/` | ext2: суперблок/группы/inode/dirent/alloc/symlink, mount, read/write, rename, mkfs |
| `fs/format_policy.rs` | единственное решение, которое может стереть данные: пусто ли устройство |
| `fs/selftest.rs` | 11 неразрушающих проверок ext2/VFS на RAM-устройстве |
| `vfs/mod.rs` | `VfsNode`, ошибки, синтетический корень, таблица монтирований, `lookup_path`/`walk` |
| `console.rs` | `kprint!`/`kprintln!`: один текст — два стока (UART и экран) |
| `shell.rs` | цикл консоли, редактор строки, команды |
| `arch/riscv64/switch.rs` | второй ABI ядра: `TaskContext` (ra/sp/s0–s11) и `task_switch` |
| `task/mod.rs` | фасад планировщика |
| `task/scheduler.rs` | таблица потоков, ready-queue, квант, стеки из PMM, `tick`/`yield`/`exit`/`reap`, вытеснение таймером |
| `task/selftest.rs` | пять проверок планировщика для `selftest` |
| `user/mod.rs` | `run_image`: ELF → user-стек → задача → `sret` в U-mode → reap; `on_fault`; встроенные hello/fault/spin |
| `user/elf.rs` | ELF64-загрузчик: валидация до первого кадра, `PT_LOAD` с `U=1`, откат кадров, `Loaded`/`ElfError` |
| `user/syscall.rs` | `ecall`-диспетчер Linux/RISC-V: `write`/`exit`/`sched_yield`/`getpid`, проверка user-указателей, `sepc+4` |
| `user/selftest.rs` | семь проверок user-mode для `selftest` |
| `net/mod.rs` | сетевой стек: состояние, ARP-кэш, `poll()`, диспетчер ARP/IPv4/ICMP/UDP/TCP, `ping`, DHCP-конфигурация |
| `net/wire.rs` | чистые форматы Ethernet/ARP/IPv4/ICMP/UDP/TCP/DHCP/DNS и контрольные суммы (без устройства и часов) |
| `net/dhcp.rs` | DHCP-клиент DISCOVER/OFFER/REQUEST/ACK |
| `net/dns.rs` | stub-резолвер A-записей через UDP |
| `net/tcp.rs` | минимальный active-open TCP-клиент для HTTP/1.0 |
| `net/selftest.rs` | одиннадцать проверок сети для `selftest` (без NIC) |
| `sync.rs` | один нереентерабельный спинлок |

Подкаталоги документируют себя сами: [`arch/README.md`](arch/README.md),
[`user/README.md`](user/README.md), [`net/README.md`](net/README.md),
[`drivers/README.md`](drivers/README.md), [`memory/README.md`](memory/README.md),
[`fs/README.md`](fs/README.md), [`vfs/README.md`](vfs/README.md).
Устройство `src/memory/**` (PMM, куча, selftest, пределы, чего нет) — в README
подсистемы; `docs/architecture.md` (разделы 6–7) добавляет к нему то, что нужно
знать при чтении boot-пути: карту памяти и порядок фаз.

## Последовательность загрузки (фазы идут строго по порядку)

| # | Фаза | Код | Почему здесь |
|---|---|---|---|
| 1 | `_start`: OpenSBI запускает ровно один hart (id может быть не 0) | `arch/riscv64/boot.rs` | нельзя предполагать hart 0 |
| 2 | стек `sp = __stack_top`, 64 KiB в `.bss`, выравнивание 16 | `arch/riscv64/boot.rs:18-21, 44-54` | на входе `sp` произвольный |
| 3 | hart id → `BOOT_HART_ID` | `arch/riscv64/boot.rs:23-28` | `mhartid` из S-mode недоступен, а `a0` ещё цел |
| 4 | `call kmain(a0, a1)` | `arch/riscv64/boot.rs:30-31` | аргументы уже в нужных регистрах |
| 5 | `console::init()`: UART, затем попытка поднять экран (`framebuffer::init`) | `main.rs:53`, `console.rs:44-48` | без UART не видно, почему нет экрана; отсутствие экрана — не ошибка |
| 6 | баннер: hart, адрес DTB | `main.rs:55-60` | — |
| 7 | разбор DTB, ошибка фатальна | `main.rs:65-72`, `fdt.rs:64-98` | описание машины нельзя угадывать |
| 8 | память, timebase и адрес консоли — в лог | `main.rs:73-81` | — |
| 9 | PMM: пул кадров минус образ ядра, стек и DTB; печать статистики | `main.rs:83-107`, `memory/mod.rs:75-77` | `alloc` уже существует — аллокатор должен быть готов до первой аллокации |
| 10 | `paging::init`: identity-карта Sv39 (RAM + страница UART + окна virtio-mmio), `satp` | `main.rs` | все фазы после неё работают на карте, которую выбрало ядро |
| 11 | `traps::init()`: `stvec` + `sstatus.SIE` | `main.rs`, `arch/riscv64/traps.rs` | ловушки раньше того, что может сбойнуть |
| 11a | `timer::init`: первый дедлайн через SBI, затем `sie.STIE` | `main.rs`, `arch/riscv64/timer.rs` | `sie` = 0 до готового compare-регистра |
| 11b | `plic::init` (DBT-база/source) → `uart::enable_rx_interrupt()` → `sie.SEIE` | `main.rs`, `arch/riscv64/plic.rs`, `drivers/uart.rs` | контроллер разводит линию раньше, чем UART её поднимает |
| 12 | `virtio_mmio::init(&tree)`: перебор окон из DTB, регистрация диска (нет диска — строка в лог) | `main.rs`, `drivers/virtio_mmio.rs` | нужна карта (MMIO) и ловушки (сломанное окно) |
| 13 | `vfs::init()` + `init_fs()`: смонтировать `/mnt`, пустое устройство — отформатировать | `main.rs::init_fs` | куча уже есть (монтирование аллоцирует); без диска — строка и дальше |
| 13a | `net::init(&tree)`: проба virtio-net + DHCP-клиент (нет NIC — строка в лог) | `main.rs`, `src/net/`, `drivers/virtio_net.rs` | нужны DTB (окна транспорта), таймер (дедлайны DHCP) и куча (кольца) |
| 14 | `shell::run(tree)`: задача 0; команда `run` — вход в U-mode (M6b) | `main.rs`, `shell.rs` | планировщик, таймер и user-mode уже подняты; shell не возвращается |

Исходы вне фаз: `panic_handler` печатает `[PANIC]` и паркует hart
(`main.rs:116-130`); `halt` в шелле уходит в SBI SRST и выключает машину
(`shell.rs:221-227`, `arch/riscv64/sbi.rs:24-38`).

## Чего в `src/` нет

Ни одного из модулей x86-версии, кроме перечисленных выше: нет `vmm`
(Sv39 включён, но карта одна — identity; пользовательские страницы живут в ней же
через бит `U`, отдельного адресного пространства на процесс нет), `pkg/`
(apt), `security/`, `debug/`, `log.rs`, `provision.rs`, `selftest_lx.rs` (Linux-compat).
Нет host-tests: тесты, которые есть, живут внутри ядра — `memory/selftest.rs`,
`fs/selftest.rs`, `task/selftest.rs`, `user/selftest.rs` и остальные, — команда
`selftest`. Подробный список — `README.md` → «Чего нет».

## Сборка и проверки

| Что | Команда | Артефакт |
|---|---|---|
| debug | `cargo build --locked` | `target/riscv64gc-unknown-none-elf/debug/pagh` |
| release | `cargo build --locked --release` | `target/riscv64gc-unknown-none-elf/release/pagh` |
| smoke (serial) | `python3 .github/smoke.py <kernel>` | exit 0 + серийный лог |
| smoke (+экран) | `python3 .github/smoke.py <kernel> --ramfb` | то же, с `-device ramfb` |
| память в госте | команда `selftest` | `SELFTEST SUMMARY: 7/7`, `(block) 3/3` (или 2/3 + skip без диска), `ext2 summary: 11/11`, `(arch) 4/4`, `(task) 5/5`, `(user) 7/7`, `(net) 11/11` |
| диск в госте | `qemu-img create` + `-device virtio-blk-device` + `-global virtio-mmio.force-legacy=false` | пустой образ форматируется, `/mnt` монтируется, файл переживает перезагрузку |

Оба профиля: `panic = "abort"` (`Cargo.toml:10-14`), release дополнительно
`opt-level = "s"`, `lto = true` (`Cargo.toml:15-16`). Panic — это смерть машины:
размотки нет, лог — единственная улика. Строки, которые печатает код из этого
каталога, построчно проверяет `.github/smoke.py`; менять их нужно вместе с
драйвером.