# Roadmap pagh-riscv

Семь майлстоунов: M1 — то, что уже работает; M2–M7 — по возрастанию. Для каждого
сказано, что именно считается «сделано» (проверяемо), что есть в дереве сегодня и
что в этот майлстоун **не** входит. Майлстоун не считается закрытым, пока его
критерии не подтверждены живым запуском и не отражены в документации.

## Сводка

| # | Майлстоун | Статус | Критерий «сделано» в одну строку |
|---|---|---|---|
| M1 | Загрузка под OpenSBI, DTB, ловушки, UART-консоль и шелл | **готов** | ядро поднимается на `virt`, печатает память/timebase из DTB, шелл отвечает на команды, `halt` выключает машину |
| M2 | Физический аллокатор кадров, Sv39, куча ядра, `alloc` | **готов** (page-fault ловушки — задел M6) | `satp` включён, `alloc` работает, и всё это видно в логе |
| M3 | Таймер через SBI, PLIC, консоль на прерываниях | **готов** | прерывание таймера приходит и обрабатывается, внешние прерывания разведены |
| M4 | ramfb + шрифт покрытия Fira Code, съёмка скриншотов | **готов** | экран рисует ядро, шрифт сверен с ассетом, `docs/shell.png` снимается скриптом |
| M5 | virtio-mmio блочное устройство, ext2, VFS | **готов** | диск читается и пишется, ФС монтируется, файл доступен из шелла и переживает перезагрузку |
| M6 | Задачи, syscalls, Linux-compat из x86-ветки | **готов (M6a + M6b)** | планировщик переключает задачи, ELF из userland исполняется в U-mode через syscall-слой |
| M7 | Сеть: virtio-net и свой TCP/IP | **готов** | DHCP, ping, DNS и TCP-клиент работают живьём на QEMU user network; `selftest net 11/11` |

«Готов» = критерии подтверждены живым запуском; «почти готов» = большая часть
подтверждена, остаток назван.

---

## M1 — загрузка, DTB, ловушки, шелл (готов)

| Критерий | Как проверяется | Подтверждение |
|---|---|---|
| `_start` паркует не-нулевые hart'ы и вызывает `kmain` | `src/arch/riscv64/boot.rs:13-38`; в логе `boot hart   0` | живой запуск |
| Стек 64 KiB, выравнивание 16 | `src/arch/riscv64/boot.rs:18-21, 44-54` | `nm`: `__stack_bottom … __stack_top` |
| DTB читается: память и timebase | `src/main.rs:65-81`, `src/fdt.rs:160-191` | `memory 0x80000000..0x90000000 (256 MiB)`, `timebase 10000000 Hz` |
| Ошибка DTB фатальна и громкая | `src/main.rs:67-71` | текст `fdt: refusing to guess the machine layout` |
| Вектор ловушек установлен | `src/main.rs:109-111`, `src/arch/riscv64/traps.rs:151-155` | кадр и диспетчер — `docs/architecture.md`, раздел 4 |
| Шелл: команды и редактор строки | `src/shell.rs:21-59, 140-250` | `.github/smoke.py`: `mem`, `hart`, `time`, `echo`, неизвестная команда, `help`, `halt` |
| `halt` действительно выключает машину | `src/shell.rs:221-227`, `src/arch/riscv64/sbi.rs:24-38` | QEMU завершается с кодом 0 (проверка в `smoke.py`) |
| Нет паники в обработчике ловушек | `src/arch/riscv64/traps.rs:4-8, 195-201` | исключение печатает `[EXC] … halting` и паркует hart |
| CI зелёный | `.github/workflows/ci.yml` | `fmt`, debug/release build, smoke в двух режимах (serial и `--ramfb`) |

Открытый хвост M1: host-тестов нет, чистая логика (`fdt.rs`) не покрыта. Для
состояния ядра тесты уже появились — это M2 (`selftest`).

## M2 — PMM, куча, `alloc`, Sv39 (готов)

| Критерий «сделано» | Статус | Подтверждение |
|---|---|---|
| PMM: карта физических кадров строится по DTB, а не по константе | **готово** | `src/memory/pmm.rs`; лог `pmm         62940/65536 frames free (256 MiB tracked), heap 0 KiB in 0 chunk(s)` |
| Пул не отдаёт кадры ядра, стека и DTB | **готово** | три зарезервированных диапазона (`src/main.rs:87-96`); `selftest` frames-roundtrip/frame-pattern PASS |
| Куча ядра + `alloc`: `Vec`/`String`/`Box` доступны | **готово** | `#[global_allocator]` (`src/main.rs:35-36`), `src/memory/heap.rs`; `SELFTEST heap-vec PASS` |
| Проверки не разрушают состояние | **готово** | `SELFTEST SUMMARY: 6/6 passed`, ledger `PMM hygiene: free frames 62802 -> 62802` в двух прогонах подряд |
| Sv39: своя таблица страниц, `satp` записан, ядро продолжает работать | **готово** | `src/memory/paging.rs` + `src/arch/riscv64/mmu.rs`; лог `sv39        on, satp=0x…`, `SELFTEST sv39-map-roundtrip PASS`, `sv39-translate PASS`; W^X проверен счётчиком `sv39-pages`/`device` |
| Ловушки page fault'ов осмысленны (не «halting» на первом же) | перенесено в M6 | ветки 13/15 в `traps.rs` печатают `[EXC]` и паркуют hart. Это честно для ядра: падать некуда, пользовательских адресных пространств ещё нет — осмысленный `page fault` нужен ровно тогда, когда появятся задачи (M6) |

Что **не** входит в M2: swap, huge pages, copy-on-write, demand paging (это придёт с
задачами в M6), NUMA, memory-mapped I/O как абстракция.

Осторожно при стыковке с M4 и M5: DMA-путь `fw_cfg` передаёт **физический** адрес
(предупреждение в шапке `src/drivers/fw_cfg.rs`). Как только Sv39 встанет между
ядром и RAM, эти строки придётся превратить в трансляцию — иначе ramfb и virtio
получат мусор.

## M3 — таймер через SBI, PLIC, консоль на прерываниях (готов)

| Критерий «сделано» | Статус | Подтверждение |
|---|---|---|
| Таймер программируется через SBI TIME (не через `mtimecmp` — он M-mode) | **готово** | `src/arch/riscv64/timer.rs`: `set_timer` (EID 0x54494D45), абсолютный дедлайн +10 мс на прерывание (дрейф не копится), `time` CSR для счёта; лог `timer 10 ms ticks (100 Hz, timebase 10000000 Hz)` |
| `sie` включает нужные источники, `stvec` их обрабатывает | **готово** | `sie.STIE`/`sie.SEIE` после инициализации; диспетчер в `traps.rs` различает `scause` с битом прерывания: timer (5), external (9); возврат только через `sret`, исключения по-прежнему печатают и паркуют hart |
| PLIC разведён: внешние прерывания доходят | **готово** | `src/arch/riscv64/plic.rs`: база и UART source из DTB, S-mode context, enable/priority/threshold, claim/complete, счётчики и unknown-source; лог `plic 0xc000000 context 1, uart irq 10 (rx interrupt on)`; в `time` видно `plic N claims, N completed` |
| Консоль не теряет байты при прерываниях | **готово** | приём — IRQ → кольцевой буфер → `uart::wait_byte` (`wfi`); smoke-driver шлёт пачку команд после баннера и проходит в четырёх режимах; `0 uart rx overrun(s)` в логе; polled-путь остаётся запасным |
| Критические секции маскируют прерывания | **готово** | `uart::put_byte` под `csr::without_interrupts` (коммит «mask interrupts around the UART transmitter lock»); правило записано в `docs/architecture.md` §12.1 и AGENTS.md |
| Сон и uptime считаются от тиков, холостой цикл не жжёт CPU | **готово** | `sleep 1` → `uptime 1010 ms (101 timer ticks at 100 Hz, 101 timer interrupts)`, `wfi` в `sleep_ticks` и в ожидании ввода |
| Счётчики прерываний проверяются машинно | **готово** | `SELFTEST timer-ticks / timer-interrupts / uart-ring / plic-uart-line PASS` + `SELFTEST SUMMARY (arch): 4/4 passed`; два прогона в одной загрузке дают один ledger |

Контекст PLIC нельзя было зашивать константой: threshold/claim-страницы живут по
`CONTEXT_BASE + context * CONTEXT_STRIDE`, и первый же M3-боот получил
`store page fault (stval=0xc201000)` на записи в *M-mode* контекст firmware, потому
что в карту попала страница контекста 0, а ядро работает в контексте 1.
`plic::windows()` теперь выводит окно из того контекста, который реально
используется, а `main.rs` мапит ровно три окна драйвера (priority, enable,
context), а не весь регион контроллера.

Шов для M6: `timer::set_tick_handler(fn())` вызывается из обработчика прерывания с
замаскированными прерываниями; контракт (ни аллокаций, ни печати, ни взятия
спинлоков, которые может держать прерванный контекст) записан в шапке `timer.rs` и
в AGENTS.md. Планировщик подключает туда `scheduler::tick()`.

Не входит в M3: SMP-таймеры, tickless, watchdog, per-hart прерывания, virtio-
прерывания (virtio-mmio остаётся polled), приоритеты/вложенные прерывания.

## M4 — ramfb, шрифт покрытия, скриншоты (готов)

| Критерий | Подтверждение |
|---|---|
| `fw_cfg` читает `etc/ramfb`, ядро настраивает framebuffer | лог `framebuffer  ramfb 1280x800 stride 5120 cells 100x37` |
| Консоль блитит шрифт покрытия, а не «кирпичи» | `tools/font_check.py --scan-all`: 6880 клеток, ни одна не содержит пикселей вне `assets/font8x18.bin` |
| Скриншот снимается скриптом воспроизводимо | `python3 tools/run.py --keys "help\n" --shot docs/shell.png --check-font --scan-all` → exit 0 |
| Верх README показывает реальный экран | `docs/shell.png` в репозитории, снят из работающего QEMU |
| Путь экрана под CI | `.github/smoke.py --ramfb` (без дисплея) |

Известный хвост M4 (вне зоны правки подсистемы): `run.sh` не передаёт
`-device ramfb`, поэтому «из коробки» лаунчер показывает только serial; README
описывает флаг явно. Не входит в M4: GPU-ускорение, смена разрешений «на лету»,
TTF-парсер (только офлайн-растетризованный ассет), окна и прикладная графика.

## M5 — virtio-mmio, ext2, VFS (готов)

| Критерий «сделано» | Статус | Подтверждение |
|---|---|---|
| virtio-mmio блочное устройство найдено по DTB и читает секторы | **готово** | `src/drivers/virtio_mmio.rs`; лог `virtio-blk: virtio-blk0 at 0x…: 131072 sectors (64 MiB)`; `disk` в шелле читает сектор 0; `SELFTEST block-memblock/block-device/block-heap-hygiene PASS` |
| запись на устройство и `flush` | **готово** | `BlockDevice::write_block`/`flush`; запись проверяется на RAM-`MemBlock` (диск в selftest только читается); `write`/`cp`/`mkfs` на реальном образе |
| ext2 монтируется, читается и пишется | **готово** | `src/fs/ext2/`; лог `fs          ext2 on /mnt, 4096 B blocks, …`; `ls/cat/stat/write/cp/mv/mkdir/rm/ln` из шелла |
| ФС совместима с хостовым ext2 | **готово** | образ `mkfs.ext2 -t ext2` + файл, записанный `debugfs`, читается ядром; `e2fsck -fn` после загрузки чист |
| данные переживают перезагрузку | **готово** | второй запуск того же образа монтирует его (без формата) и читает файл; это же делает CI (`smoke.py --disk … --fresh`, затем без `--fresh`) |
| устройство не портится: формат только пустого | **готово** | `src/fs/format_policy.rs`: нули → формат; MBR/GPT/чужое/битый ext2 → отказ с причиной; `SELFTEST ext2-policy PASS` |
| VFS отделяет ФС от шелла | **готово** | `src/vfs/mod.rs`: `VfsNode`, синтетический корень, `/mnt`, `lookup_path` vs `walk` |
| два прогона `selftest` дают один ledger | **готово** | `PMM hygiene: free frames N -> N` оба раза; 7 memory + 3 block + 11 ext2 проверок |
| CI проверяет M5 end-to-end | **готово** | `.github/workflows/ci.yml`: пустой образ → формат/монтирование/запись, затем повторная загрузка того же образа читает файл |

Внутри гостя это проверяется командой `selftest` (RAM-устройства, ничего не
портит) и руками — `DISK=/tmp/pagh.img ./run.sh`, затем `ls /mnt`, `write`, `cat`;
из хоста — `mkfs.ext2 -q -t ext2 -F /tmp/host.img && debugfs -w -R "write файл …"`.

Не входит в M5 (честно): **журнал/WAL** x86-версии (сбой между двумя записями
может оставить операцию недоделанной), procfs/ramfs, права/uid/gid/времена,
несколько дисков (в `block.rs` один слот), hot-plug и разделы, кэш страниц и
отложенная запись, `fsck`, квоты, разреженные файлы и тройной indirect.

## M6 — задачи, syscalls, Linux-compat (готов: M6a + M6b)

| Критерий «сделано» | Статус | Подтверждение |
|---|---|---|
| Контекстный switch — ABI, а не «сохранить побольше» | **готово** | `src/arch/riscv64/switch.rs`: `TaskContext` (`ra`, `sp`, `s0..s11`), смещения asm берутся из layout'а, `const _` проверяет размер; trap frame не трогается |
| Потоки ядра со своими стеками из PMM | **готово** | `src/task/scheduler.rs`: 8 слотов, стек 16 KiB + guard 4 KiB (5 кадров), `spawn`/`exit`/`reap`; `SELFTEST task-exit-returns-stack PASS` (62650 -> 62645 -> 62650) |
| Round-robin и вытеснение по таймеру | **готово** | квант 8 тиков, готовые потоки FIFO; одна строка `timer::set_tick_handler(tick_from_timer)`; `sleep 1` двигает счётчики демо-потоков без `yield` (258 → 358 тиков, 32 → 44 переключения) |
| Планировщик не аллоцирует и не берёт лок через switch | **готово** | `schedule()` выбирает под локом, отпускает и только потом `task_switch`; тик/switch без аллокаций; контракт в `src/task/README.md` |
| Проверки машинные и идемпотентные | **готово** | `SELFTEST task-* 5/5`: interleave (130 switch-out), exit-stack, layout/guard, canary, idle; ledger `62650 -> 62650` в двух прогонах |
| User mode + syscall'ы + ELF из шелла | **готово** | M6b: `sret` с `SPP=0`/`SPIE=1`/`SUM=0`, `sscratch`-контракт ловушек, `ecall`-диспетчер (`write`/`exit`/`sched_yield`/`getpid`, `sepc+4`, проверка user-указателей по PTE U\|R), ELF64-загрузчик с откатом кадров, `run <path>` и фикстуры `demo`/`fault`/`spin`; `SELFTEST user-* 7/7`, ledger 62626 → 62626 дважды; `run demo` → 42, `run fault` → killed, `run spin` → 7 (таймер-in-U) |
| Прерывание таймера, взятое в U-mode | **готово** | `trap_entry` U-путь + `SPP`-разбор эпилога; счётчик `traps::user_timer_ticks()` растёт на `run spin` (0.2 s в U-mode), selftest это проверяет |
| Linux-compat слой из x86 | **нет** | после M6b и только когда есть userland, которому он нужен |

Не входит в M6: SMP-планирование, приоритеты, реальное время, блокирующий I/O,
несколько адресных пространств на поток (одна Sv39-карта, один ASID), сигналы и
`fork`/`exec`. Это записано в `README.md` → «Чего нет».

Релиз **0.5.0** — это M6b: версия в `Cargo.toml`/`Cargo.lock`, тег `0.5.0`,
GitHub release, скриншот `docs/user.png`.



## M7 — сеть: virtio-net и свой стек (готов)

Сеть сделана целиком своя: modern virtio-mmio драйвер, wire-слой с контрольными
суммами, ARP/IPv4/ICMP/UDP/TCP, DHCP-клиент, DNS-резолвер и минимальный
TCP-клиент для HTTP/1.0. Ни smoltcp, ни внешних крейтов.

| Критерий «сделано» | Статус | Подтверждение |
|---|---|---|
| Драйвер virtio-net (RX/TX, modern) | **готово** | `src/drivers/virtio_net.rs`: feature negotiation (`VERSION_1`+`MRG_RXBUF`), 12-байтовый header, 8 RX-буферов, синхронный TX, MAC из конфига; `virtio-net: virtio-net0 at …` в логе |
| Свой wire-слой | **готово** | `src/net/wire.rs`: Ethernet/ARP/IPv4/ICMP/UDP/TCP/DHCP/DNS + RFC 1071 и pseudo-header; `SELFTEST net-*` |
| DHCP-клиент | **готово** | `src/net/dhcp.rs` DISCOVER/OFFER/REQUEST/ACK; лог `net         DHCP ip 10.0.2.15/255.255.255.0 gw 10.0.2.2 dns 10.0.2.3` |
| ICMP echo / ARP | **готово** | `ping 10.0.2.2` → `4/4 replies`; ARP-кэш 8 записей, ответы на request'ы |
| DNS-резолвер | **готово** | `dns example.com` → A-запись через DNS из lease |
| TCP-клиент | **готово** | `src/net/tcp.rs`: handshake/seq/ack/FIN, один active-open сокет; `http 10.0.2.2 <port> /` получает ответ от сервера на хосте |
| Логика проверяется без железа | **готово** | `SELFTEST net-* 11/11` (без NIC): checksum-вектор, round-trip заголовков, отказ на порче, DHCP/DNS из фиксированных байт |
| CI и живой прогон | **готово** | `smoke.py --net` (DHCP+net+ping+halt) в CI; `selftest` дважды с одинаковым PMM-ledger; `docs/net.png` проверен `font_check` |

Не входит в M7: TLS/apt (следующий майлстоун, нужен крипто-слой), IPv6,
фрагментация, UDP-сокеты как публичный API, прослушивание TCP, несколько
интерфейсов/соединений, обновление lease по таймеру, прерывания NIC.

Релиз **0.6.0** — это M7: версия в `Cargo.toml`/`Cargo.lock`, тег `0.6.0`,
GitHub release, скриншот `docs/net.png`.

## Дальше (после M7)

1. **TLS + apt**: vendored `embedded-tls` с локальным патчем (как на x86),
   OpenPGP-проверка `InRelease`/`Release.gpg` и дайджестов `.deb`; только после
   этого `apt` становится доступен на RISC-V.
2. **Userland-API и Rust-рантайм** (`pagh-api` + `pagh-tools`, отдельные репо):
   расширение Linux-compat syscall-слоя (mmap/brk, futex, poll, sockets, время),
   crt0/`libpagh`, затем порты `curl`/`ripgrep`, переписанные под API.
3. Свой **загрузчик для x86-64** (в репозитории Pagh-OS) — замена Limine.
4. Журнал/WAL в ext2, SMP, сетевые помелочи (renewal, несколько сокетов).
