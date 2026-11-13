# `src/` — карта кодовой базы ядра pagh-riscv

Крейт `pagh`: **бинарный**, `#![no_std]` + `#![no_main]`
(`src/main.rs:10-11`), один таргет — `riscv64gc-unknown-none-elf`
(`.cargo/config.toml:2`). Зависимости — только vendored стек TLS 1.3/X.509
(`Cargo.toml:8-55`, `vendor/`: 11 прямых пинов, 77 крейтов); остальной код свой.
`alloc` подключён (`extern crate alloc`, `src/main.rs:16`) и опирается на
`#[global_allocator]` в корне крейта (`src/main.rs:35-36`) — то есть на кучу из
`src/memory/heap.rs`. Линкуется `linker.ld`, точка входа `_start` на
`0x8020_0000`. Своего кода вне этого каталога нет; вместе с ним в ядро
компилируются только vendored-крейты из `vendor/`.

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
| `arch/riscv64/mmu.rs` | примитивы Sv39: флаги PTE, `PageTable`, `vpn`, `satp_for`/`satp_for_asid`/`satp_root`, `activate_satp`, `sfence` — вызывает `memory::paging` |
| `fdt.rs` | минимальный читатель flattened device tree |
| `memory/mod.rs` | фасад памяти: `init`, `stats`, `kernel_end`, `boot_stack`, `fdt_span` |
| `memory/pmm.rs` | битовая карта физических кадров 4 KiB (окно 8 GiB) |
| `memory/heap.rs` | free-list внутри 64 KiB-чанков, `GlobalAlloc` |
| `memory/selftest.rs` | семь неразрушающих проверок памяти (PMM, Sv39, куча, чанки) для `selftest` |
| `drivers/mod.rs` | фасад драйверов |
| `drivers/uart.rs` | NS16550: приём по прерыванию PLIC в кольцо, передача polled, запасной polled-путь |
| `drivers/fw_cfg.rs` | QEMU fw_cfg: селектор/данные + DMA |
| `drivers/ramfb.rs` | конфигурация устройства ramfb |
| `drivers/framebuffer.rs` | текстовый экран: тень, шрифт покрытия, статус-бар, software-курсор как часть кадра |
| `drivers/block.rs` | шов блочного слоя: трейт `BlockDevice`, один слот реестра, RAM-диск `MemBlock` |
| `drivers/virtio_mmio.rs` | virtio-blk на mmio-транспорте (modern, version 2), polled; окна — из DTB |
| `drivers/virtio_net.rs` | virtio-net (modern, две очереди, polled): MAC, RX/TX-кольца, кадры |
| `drivers/virtio_input.rs` | virtio-input: pointer (tablet/mouse) и keyboard (US-раскладка); polled из idle-цикла |
| `drivers/selftest.rs` | проверки блочного слоя и указателя (RAM round-trip, read-only диск, input-device) |
| `fs/mod.rs` | фасад ФС: ext2, политика формата, in-QEMU тесты |
| `fs/ext2/` | ext2: суперблок/группы/inode/dirent/alloc/symlink, mount, read/write, rename, mkfs; `fs_stat` отдаёт magic, блоки/inode'ы и UUID-префикс для `statfs` |
| `fs/format_policy.rs` | единственное решение, которое может стереть данные: пусто ли устройство |
| `fs/selftest.rs` | 11 неразрушающих проверок ext2/VFS на RAM-устройстве |
| `vfs/mod.rs` | `VfsNode`, ошибки, синтетический корень, таблица монтирований, `lookup_path`/`walk`, `fs_type`/`mount_snapshot` для `/proc/mounts`, `FsStat` (magic/blocks/inodes/fsid) для `statfs(43)`/`fstatfs(44)` |
| `vfs/proc.rs` | синтетический read-only `/proc`: `cpuinfo`, `meminfo`, `mounts`, `tasks`, `uptime`, `version`, `net/{dev,arp}`; per-pid `kthreads/<tid>/{status,stat}` и `<pid>/{status,stat,cmdline}` (pid = `getpid()`, `self` — симлинк на свою запись); рендер живого состояния при чтении (`read(offset==0)` — новый снимок, продолжение — тот же, `size()` — длина рендера); монтируется из `vfs::init` |
| `console.rs` | `kprint!`/`kprintln!`: один текст — два стока (UART и экран) |
| `shell.rs` | цикл консоли, редактор строки, команды |
| `arch/riscv64/switch.rs` | второй ABI ядра: `TaskContext` (ra/sp/s0–s11) и `task_switch` |
| `task/mod.rs` | фасад планировщика |
| `task/scheduler.rs` | таблица потоков, ready-queue, квант, стеки из PMM, `tick`/`yield`/`exit`/`reap`, `block_current`/`wake_task`/`kill_group`/`spawn_user_thread`, `State::Stopped` + `stop_current`/`continue_task`/`stopped_tasks` для job control, вытеснение таймером; `TaskInfo` (`group`/`satp` включительно), `task_info(id)` и `kthread_ids` — источник per-pid `/proc` |
| `task/selftest.rs` | пять проверок планировщика для `selftest` |
| `user/mod.rs` | `run_image`/`run_file`: новый root/ASID → ELF → user-стек → SysV-кадр `argc/argv/envp` → главный поток процесса → `sret` в U-mode → ожидание `threads == 0` → reap; `brk`-heap, mmap, `on_fault` (убивает процесс), `exit_thread`/`exit_group`, `fork`/`execve`/`wait4` (fork копирует и выросший стек, exec сбрасывает `stack_low`), очистка сирот, capture для ABI-теста; встроенные hello/fault/spin/argv/abi/exec-child/stack/stack-over/stack-fork/proc/statfs |
| `user/process.rs` | таблица процессов (≤ 6): pid/ppid, root/satp/asid, `name` и `cmdline` (bounded 128 B argv-снимок для `/proc/<pid>/cmdline`), image/heap-списки, mmap, `stack_base`/`stack_low` (дно выросшего user-стека), `threads`/`exit`/`signal`, `wait_tid`; `reserve`/`publish`/`with_slot_mut`/`slot_of`, `force_finish`/`ready_pids`/`live_pids`/`is_live` для очистки сирот и `/proc` |
| `user/signal.rs` | POSIX-подобные сигналы: `kill`/`tgkill`/`sigaltstack`/`rt_sigaction`/`rt_sigprocmask`/`rt_sigreturn`, per-process обработчики (fork копирует, exec сбрасывает в `SIG_DFL`), per-thread pending/blocked/altstack, кадр 736 B с `siginfo`/`ucontext`, `SA_SIGINFO`/`SA_ONSTACK`/`SA_NODEFER`/`SA_RESETHAND`/`SA_RESTART`, синхронные SIGSEGV/SIGILL/SIGBUS/SIGTRAP, job-control остановка/продолжение |
| `user/futex.rs` | статическая futex-очередь: 32 ожидающих на 16 адресов, ключ `(pid, uaddr)`, `wait`/`wake`/`clear_run` поверх `block_current`/`wake_task` |
| `user/elf.rs` | ELF64-загрузчик: валидация до первого кадра, `PT_LOAD` с `U=1`, потоковые сегменты кусками ≤ 512 B, откат кадров, `Loaded`/`ElfError` |
| `user/syscall.rs` | native ABI: `read`/`write`/`openat`/`close`/`lseek`/`fstat`/`statfs`/`fstatfs`/`getdents64`/`brk`/`clock_gettime`/`nanosleep`/`uname`/`readlinkat`/`exit`/`exit_group`/`set_tid_address`/`futex`/`sched_yield`/`getpid`/`getppid`/`gettid`/`clone`/`fork`/`execve`/`wait4` + сигналы `kill`/`tgkill`/`sigaltstack`/`rt_sigaction`/`rt_sigprocmask`/`rt_sigreturn` + мутации `mkdirat`/`unlinkat`/`symlinkat`/`linkat`/`renameat2`/`fchmodat`/`ftruncate` + сокеты `socket`/`bind`/`listen`/`accept`/`connect`/`sendto`/`recvfrom`/`setsockopt`/`getsockopt`/`shutdown` + энтропия/TLS: `getrandom(278)`, `tls_connect(1100)`/`tls_write(1101)`/`tls_read(1102)`/`tls_close(1103)`; per-process fd-таблица (включая `FdEntry::Socket`/`FdEntry::TcpConn`/`FdEntry::Tls`/`FdEntry::UdpSocket`/`Unsupported`) и таблица режимов, `copy_from_user`/`copy_to_user`, `sepc+4` |
| `user/stack.rs` | деманд-рост user-стека: `try_grow` из trap-диспетчера (load/store page fault ниже `stack_low`, не ниже `USER_STACK_FLOOR`, не дальше 64 KiB от `sp`), `release`, счётчики `stats`; потолок 1 MiB на процесс |
| `user/selftest.rs` | шестнадцать проверок user-mode для `selftest` (argv-строка, все syscall'ы ABI + потоки/futex + сигналы/altstack/siginfo/job control + fork/wait/exec-маркеры, ELF-матрица, fault, таймер, `user-stack-grows`/`user-stack-cap`/`user-stack-fork`, `user-proc-fs` — `userland/proc.elf` читает `/proc` через syscall'ы, включая per-pid/`self`/`kthreads` и reaped-child `-ENOENT`, `user-statfs` — `userland/statfs.elf` проверяет `statfs`/`fstatfs`, живой учёт блоков/inode'ов и матрицу ошибок, 100 KiB) |
| `net/mod.rs` | сетевой стек: состояние, ARP-кэш, `poll()`, диспетчер ARP/IPv4/ICMP/UDP/TCP, `ping`, DHCP-конфигурация, per-port очередь userland-UDP (`udp_bind`/`udp_take_into`/`udp_wait_into`) |
| `net/wire.rs` | чистые форматы Ethernet/ARP/IPv4/ICMP/UDP/TCP/DHCP/DNS и контрольные суммы (без устройства и часов) |
| `net/dhcp.rs` | DHCP-клиент DISCOVER/OFFER/REQUEST/ACK |
| `net/dns.rs` | stub-резолвер A-записей через UDP |
| `net/tcp.rs` | TCP: active-open клиент для HTTP/1.0 и пассивный open (`bind`/`listen`/`accept`, очередь ≤ 4, 8 слотов); `recv_nonblocking[_id]` для socket `recv`, `peer[_id]` для `recvfrom`/`accept` |
| `net/tls.rs` | TLS 1.3 клиент (fail-closed) и userland-сессии поверх него: `user_open`/`user_write`/`user_read`/`user_ready`/`user_shutdown`/`user_close`, таблица на 4 сессии |
| `net/selftest.rs` | двенадцать проверок сети для `selftest` (без NIC), включая fail-closed поверхность таблицы TLS-сессий |
| `pkg/mod.rs` | нативный установщик `pkg`: `repo`/`index`/`list`/`install`/`remove`/`installed`, HTTP поверх `net::tcp`, индекс в фиксированных массивах, шлюз «размер + SHA-256» до разбора `PPKG0001`, `/mnt/pkg/<name>.manifest` |
| `pkg/sha256.rs` | потоковый SHA-256 (FIPS 180-4) без аллокаций; `selftest()` для `pkg selftest` и `cfg(test)`-векторы |
| `sync.rs` | один нереентерабельный спинлок |

Подкаталоги документируют себя сами: [`arch/README.md`](arch/README.md),
[`user/README.md`](user/README.md), [`net/README.md`](net/README.md),
[`drivers/README.md`](drivers/README.md), [`memory/README.md`](memory/README.md),
[`fs/README.md`](fs/README.md), [`vfs/README.md`](vfs/README.md),
[`pkg/README.md`](pkg/README.md).
Устройство `src/memory/**` (PMM, куча, selftest, пределы, чего нет) — в README
подсистемы; `docs/architecture.md` (разделы 6–7) добавляет к нему то, что нужно
знать при чтении boot-пути: карту памяти и порядок фаз.

## Последовательность загрузки (фазы идут строго по порядку)

| # | Фаза | Код | Почему здесь |
|---|---|---|---|
| 1 | `_start`: OpenSBI запускает ровно один hart (id может быть не 0) | `arch/riscv64/boot.rs` | нельзя предполагать hart 0 |
| 2 | стек `sp = __stack_top`, 128 KiB в `.bss`, выравнивание 16 | `arch/riscv64/boot.rs:18-21, 44-56` | на входе `sp` произвольный; ниже стека статики таймера, переполнение — тихая порча |
| 3 | hart id → `BOOT_HART_ID` | `arch/riscv64/boot.rs:23-28` | `mhartid` из S-mode недоступен, а `a0` ещё цел |
| 4 | `call kmain(a0, a1)` | `arch/riscv64/boot.rs:30-31` | аргументы уже в нужных регистрах |
| 5 | `console::init()`: UART, затем попытка поднять экран (`framebuffer::init`) | `main.rs:53`, `console.rs:44-48` | без UART не видно, почему нет экрана; отсутствие экрана — не ошибка |
| 6 | баннер: hart, адрес DTB | `main.rs:55-60` | — |
| 7 | разбор DTB, ошибка фатальна | `main.rs:65-72`, `fdt.rs:64-98` | описание машины нельзя угадывать |
| 8 | память, timebase и адрес консоли — в лог | `main.rs:73-81` | — |
| 9 | PMM: пул кадров минус образ ядра, стек и DTB; печать статистики | `main.rs:83-107`, `memory/mod.rs:75-77` | `alloc` уже существует — аллокатор должен быть готов до первой аллокации |
| 10 | `paging::init`: identity-карта Sv39 (RAM + страница UART + окна virtio-mmio), `satp`; позже `create_user_space` копирует supervisor-часть в root процесса | `main.rs`, `memory/paging.rs` | все фазы после неё работают на карте, которую выбрало ядро; user-процесс получает её копию с `U=0` |
| 11 | `traps::init()`: `stvec` + `sstatus.SIE` | `main.rs`, `arch/riscv64/traps.rs` | ловушки раньше того, что может сбойнуть |
| 11a | `timer::init`: первый дедлайн через SBI, затем `sie.STIE` | `main.rs`, `arch/riscv64/timer.rs` | `sie` = 0 до готового compare-регистра |
| 11b | `plic::init` (DBT-база/source) → `uart::enable_rx_interrupt()` → `sie.SEIE` | `main.rs`, `arch/riscv64/plic.rs`, `drivers/uart.rs` | контроллер разводит линию раньше, чем UART её поднимает |
| 12 | `virtio_mmio::init(&tree)`: перебор окон из DTB, регистрация диска (нет диска — строка в лог) | `main.rs`, `drivers/virtio_mmio.rs` | нужна карта (MMIO) и ловушки (сломанное окно) |
| 12a | `virtio_input::init(&tree)`: указатель (tablet/mouse) и клавиатура, очереди событий, включение курсора при живом экране | `main.rs`, `drivers/virtio_input.rs` | тот же список окон и карта; нет устройств — строка в лог |
| 13 | `vfs::init()` + `init_fs()`: смонтировать `/mnt`, пустое устройство — отформатировать | `main.rs::init_fs` | куча уже есть (монтирование аллоцирует); без диска — строка и дальше |
| 13a | `net::init(&tree)`: проба virtio-net + DHCP-клиент (нет NIC — строка в лог) | `main.rs`, `src/net/`, `drivers/virtio_net.rs` | нужны DTB (окна транспорта), таймер (дедлайны DHCP) и куча (кольца) |
| 14 | `shell::run(tree)`: задача 0; команда `run` — вход в U-mode (M6b) | `main.rs`, `shell.rs` | планировщик, таймер и user-mode уже подняты; shell не возвращается |

Исходы вне фаз: `panic_handler` печатает `[PANIC]` и паркует hart
(`main.rs:116-130`); `halt` в шелле уходит в SBI SRST и выключает машину
(`shell.rs:221-227`, `arch/riscv64/sbi.rs:24-38`).

## Чего в `src/` нет

Ни одного из модулей x86-версии, кроме перечисленных выше: нет `vmm`
(Sv39 включён, но карта одна — identity; пользовательские страницы живут в ней же
через бит `U`, отдельного адресного пространства на процесс нет), `security/`,
`debug/`, `log.rs`, `provision.rs`, `selftest_lx.rs` (Linux-compat).
`pkg/` есть, но это не apt: плоский `.ppkg` по plain HTTP с проверкой размера и
SHA-256, без TLS, подписей и зависимостей (см. [`pkg/README.md`](pkg/README.md)).
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
| память в госте | команда `selftest` | `SELFTEST SUMMARY: 7/7`, `(block) 4/4` (или 3/4 + skip без диска, включая `input-device`), `ext2 summary: 11/11`, `(arch) 4/4`, `(task) 5/5`, `(user) 16/16` с `/mnt` (или 13/16 + skip `user-abi-syscalls`/`user-run-large-file`/`user-statfs`), `(net) 12/12` |
| диск в госте | `qemu-img create` + `-device virtio-blk-device` + `-global virtio-mmio.force-legacy=false` | пустой образ форматируется, `/mnt` монтируется, файл переживает перезагрузку |
| `pkg` E2E | `python3 tools/test_pkg.py` (порт 8000 свободен; `--release` — release-ядро) | собранный `.ppkg` + HTTP-сервер + QEMU с диском: index/install/run (код 42)/remove и отказ испорченному пакету по SHA-256; логи и pcap в напечатанном каталоге |

Оба профиля: `panic = "abort"` (`Cargo.toml:10-14`), release дополнительно
`opt-level = "s"`, `lto = true` (`Cargo.toml:15-16`). Panic — это смерть машины:
размотки нет, лог — единственная улика. Строки, которые печатает код из этого
каталога, построчно проверяет `.github/smoke.py`; менять их нужно вместе с
драйвером.