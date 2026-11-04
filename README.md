<h1 align="center">pagh-riscv</h1>

<p align="center">
  Небольшое 64-битное ядро для RISC-V (RV64) на Rust: <code>#![no_std]</code>,
  без единой внешней зависимости, загрузка через OpenSBI, запуск в QEMU.
</p>

<p align="center">
  <img alt="Language: Rust" src="https://img.shields.io/badge/language-Rust-orange.svg">
  <img alt="Target: riscv64gc-unknown-none-elf" src="https://img.shields.io/badge/target-riscv64gc--unknown--none--elf-blue.svg">
  <img alt="no_std" src="https://img.shields.io/badge/%23!%5Bno__std%5D-yes-informational.svg">
  <img alt="Boot: OpenSBI S-mode" src="https://img.shields.io/badge/boot-OpenSBI%20S--mode-purple.svg">
  <img alt="Milestone: M1-M7 done" src="https://img.shields.io/badge/milestone-M1%E2%80%93M7%20done-brightgreen.svg">
  <img alt="License: MIT" src="https://img.shields.io/badge/license-MIT-green.svg">
</p>

---

## English summary

`pagh-riscv` is the 64-bit RISC-V port of the [pagh](https://github.com/ccclosed/Pagh-OS)
kernel: a small `#![no_std]` Rust kernel that boots under OpenSBI on QEMU's `virt`
machine, reads the machine description from the flattened device tree, allocates
physical frames from a bitmap pool, serves `alloc` from a kernel heap that grows in
64 KiB chunks, brings up a ramfb screen with an 8×18 coverage font next to the serial
console, installs a trap vector that names its cause instead of resetting silently,
and drops into an interactive serial shell with an in-kernel self-test. On top of
that it has a 100 Hz timer, a PLIC-driven UART, a virtio-mmio disk with ext2/VFS,
preemptive kernel threads, a virtio-net NIC with its own TCP/IP stack (DHCP, DNS,
ICMP, a minimal TCP client) and — since M6b — **user mode**: a static ELF runs with
the `U` bit set, enters through `sret`, talks to the kernel through an `ecall`
ABI and is killed (not the machine) when it faults. **All of these are verified on
a live boot**; what is genuinely missing — per-process page tables, a journal,
SMP, TLS/apt — is listed plainly in [Чего нет](#чего-нет). The documentation is deliberately checkable: every claim in
[`docs/architecture.md`](docs/architecture.md) cites the file and line it comes from,
and the CI smoke test asserts the exact boot log this README shows.

```sh
cargo build                              # riscv64gc-unknown-none-elf, pinned nightly
./run.sh                                 # QEMU-окно при наличии дисплея, иначе serial; Ctrl-A X — выход
DISK=/tmp/pagh.img ./run.sh              # + virtio-диск: пустой образ будет отформатирован и смонтирован в /mnt
NO_MOUSE=1 ./run.sh                      # без virtio-tablet: путь «указателя нет»
python3 tools/run.py --keys "help\n" --shot /tmp/shot.png --check-font   # screen + font
python3 tools/run.py --mouse 900 300 --shot /tmp/cursor.png             # + software-курсор в (900,300)
```

Docs: [architecture](docs/architecture.md) · [roadmap](docs/roadmap.md) ·
[screenshots](docs/screenshot.md) · [contributing](CONTRIBUTING.md) ·
[agents](AGENTS.md) · [crate map](src/README.md).

---

## Скриншот

![Консоль pagh-riscv: команда help на экране 1280×800](docs/shell.png)

Указатель M8: `virtio-input` tablet на virtio-mmio, курсор — software-слой в
теневом буфере. Позиция ниже задана командой
`python3 tools/run.py --mouse 900 300 --shot …`: стрелка стоит ровно в
запрошенном пикселе, а текст под ней печатается обычным образом (курсор
перерисовывается поверх кадра, не «замораживая» клетку).

![software-курсор в (900, 300)](docs/cursor.png)

Диск M5: virtio-mmio-устройство найдено через DTB, пустой образ отформатирован,
ext2 смонтирован в `/mnt`; `ls`/`cat`/`stat` работают, `disk` показывает ёмкость и
живую проверку сектора 0.

![ext2 на virtio-диске: ls, cat, stat](docs/fs.png)

Планировщик M6a: `spawn 3` завёл три потока, `tasks` показывает снимок таблицы
(id, имя, состояние, переключения, счётчик работы, стек), `yield 48` прогнал тики
и переключения; `idle` держит hart в `wfi`, счётчики демо-потоков растут и без
единого `yield`, потому что тик таймера вытесняет их из trap-контекста.

User mode M6b: `run demo` исполняет статический ELF в U-mode (`write`,
`sched_yield`, `exit(42)`), `run fault` намеренно падает по load page fault —
ядро печатает причину и **оставляет машину живой**, `run spin` крутится в U-mode
0.2 с, чтобы таймер успел вытеснить задачу, и выходит с кодом 7. Native ABI для
libpagh: `run <path> [args...]` передаёт `argc`/`argv`/`envp`, а `read`/`openat`/
`fstat`/`getdents64`/`brk`/`clock_gettime`/`uname`/`readlinkat` проверены
встроенным `userland/abi.elf` в `selftest`.

![user mode: run demo, run fault, run spin](docs/user.png)

Сеть M7: `net` показывает MAC и DHCP-lease, `ping 10.0.2.2` проходит через
собственный ARP/ICMP-путь, `dns example.com` разрешает имя через DNS из lease, а
`http` открывает TCP-соединение собственным клиентом.

![network: net, ping, dns](docs/net.png)

![Потоки ядра: spawn, tasks, yield](docs/tasks.png)

Это не макет и не рендер на хосте: кадр нарисован самим ядром в теневом буфере и
отдан устройству `ramfb`, а снят из работающего QEMU через `screendump`. Команда
воспроизведения — та же, что проверяет пиксели:

```sh
python3 tools/run.py --keys "help\n" --shot docs/shell.png --check-font --scan-all
```

`--check-font` сверяет каждый символ на экране с `assets/font8x18.bin` (ассет
шрифта 8×18) и падает на одном отличающемся пикселе; в этом снимке проверены все
6880 клеток — ни одна не содержит пикселей вне ассета. Внизу — статус-бар
(`pagh OS  RISC-V (RV64)` … `ramfb 1280x800`).

Серийный лог одного прогона smoke-драйвера в режиме `--ramfb` (сокращён только лог
OpenSBI перед первой строкой ядра). Те же строки в том же порядке проверяет CI:

```text
framebuffer  ramfb 1280x800 stride 5120 cells 100x37

========================================
   pagh OS — RISC-V (RV64)
========================================
boot hart   0
device tree 0x8fe00000
memory      0x80000000..0x90000000 (256 MiB)
timebase    10000000 Hz
console     NS16550 @ 0x10000000
pmm         62934/65536 frames free (256 MiB tracked), heap 0 KiB in 0 chunk(s)

Type 'help' for the command list.

pagh:rv64> mem
memory 0x80000000..0x90000000 (256 MiB)
pmm    62802 free of 65536 frames (256 MiB), pool 0x80000000..0x90000000
heap   0 B used, 0 B mapped from the pool in 0 chunk(s) of 64 KiB
sv39   on  satp=0x8000000000080a28, root 0x80a28000, identity 0x80000000..0x90000000 (RWX below 0x80a28000, RW above), 65537 pages, 1 device
pagh:rv64> hart
hart 0
pagh:rv64> time
time 3950155 ticks (~0 s at 10000000 Hz)
pagh:rv64> echo smoke ok
smoke ok
pagh:rv64> nosuchcmd
nosuchcmd: unknown command (try 'help')
pagh:rv64> run demo alpha beta
hello from user mode
run: demo exited with code 42
pagh:rv64> help
commands:
  help          this list
  mem           device tree range, frame pool, heap and the page table
  selftest      run the in-QEMU memory self-tests
  time          the `time` CSR (timebase ticks) and its frequency
  hart          the hart this kernel is running on
  echo <text>   print the text back
  clear         clear the screen (ANSI)
  halt          power the machine off (SBI SRST)
pagh:rv64> halt
halt: powering off
```

Проверки памяти запускаются из шелла (`selftest`) и печатают вердикт построчно —
это фрагмент другого прогона, той же сборки:

```text
pagh:rv64> selftest
[selftest] free frames before: 62802 of 65536
SELFTEST frames-roundtrip PASS
SELFTEST frame-pattern PASS
SELFTEST sv39-map-roundtrip PASS
SELFTEST sv39-translate PASS
[selftest] heap: 8256 B in use, 65536 B mapped, 1 chunk(s)
SELFTEST heap-vec PASS
heap: double free of 0x80a73090 ignored
heap: refusing to free 0x802302b8: not a heap block
SELFTEST heap-defenses PASS
SELFTEST SUMMARY: 7/7 passed
[selftest] PMM hygiene: free frames 62802 -> 62802
```

Числа кадров зависят от размера ядра и от `-m`; проверяется не они, а то, что
строка есть, что все проверки PASS и что ledger до и после совпадает.

---

## Что работает сегодня

Статус — по коду в дереве, а не по замыслу. «Готово» подтверждено живым запуском.

| Подсистема | Статус | Что именно есть | Где |
|---|---|---|---|
| Загрузка | готово | `_start` ставит стек и вызывает `kmain(a0, a1)`; OpenSBI запускает ровно один hart (id не обязательно 0), остальные держит остановленными | `src/arch/riscv64/boot.rs` |
| Модель привилегий | готово | S-mode под OpenSBI, M-mode не трогаем; U-mode для user-программ через `sret` (`SPP=0`), выключение через SBI SRST | `src/arch/riscv64/{sbi,traps}.rs`, `src/user/mod.rs` |
| Device tree | готово | пять вопросов: `/memory`, `timebase-frequency`, окна `virtio,mmio`, PLIC `reg`, UART `interrupts`; ошибка разбора фатальна | `src/fdt.rs` |
| Ловушки | готово | `stvec` → `trap_entry` (обмен `sscratch` различает U/S), диспетчер: прерывания возвращаются, исключение из S-mode печатает и паркует hart, исключение из U-mode завершает только задачу | `src/arch/riscv64/traps.rs`, `src/user/mod.rs` |
| Консоль | готово | NS16550 по `0x1000_0000`, 8N1; приём — по прерыванию PLIC в кольцевой буфер (polled-путь остаётся запасным), передача по-прежнему polled; один текстовый поток — два стока | `src/drivers/uart.rs`, `src/console.rs` |
| Экран | готово | ramfb 1280×800, stride 5120, сетка 100×37, шрифт покрытия 8×18, статус-бар; кадр отдаётся на каждый `kprint!`; software-курсор — часть кадра (см. строку «Мышь») | `src/drivers/{fw_cfg,ramfb,framebuffer}.rs` |
| Мышь, клавиатура и курсор (M8) | готово | virtio-input на том же virtio-mmio: **tablet** (абсолютные оси, диапазон из `CFG_ABS_INFO`) или mouse как fallback, и **keyboard** (US-раскладка, shift/caps/backspace/enter/tab) — оба устройства QEMU `virt` выставляет отдельными virtio-mmio-узлами с одним device id, драйвер различает их по битовым картам возможностей. Курсор 12×19 — часть кадра; клавиатурные байты шелл читает из idle-цикла рядом с UART, поэтому окно QEMU становится полноценной консолью. Опроса из idle-цикла достаточно (100 Гц); своего прерывания у устройств нет | `src/drivers/virtio_input.rs`, `src/drivers/framebuffer.rs` |
| Шелл | готово | редактор строки (строки длиннее 128 байт отклоняются целиком, а не обрезаются), idle-цикл на 100 Гц, в котором опрашиваются UART-кольцо, сеть, указатель и клавиатура; одна и та же консоль работает и через serial, и через окно QEMU; команды памяти/диска/ФС/сети, `run <path>`, `run demo` / `run fault` / `run spin`, `sleep`, `time`, `halt` (полный список — в таблице ниже) | `src/shell.rs`, `src/user/mod.rs`, `src/net/`, `src/drivers/virtio_input.rs` |
| Физическая память (PMM) | готово | битовая карта 4 KiB-кадров окна 8 GiB (покрывает QEMU `virt` с RAM выше 4 GiB), `set = свободен`, выделение одиночное и непрерывным блоком | `src/memory/pmm.rs` |
| Куча ядра, `alloc` | готово | `#[global_allocator]` поверх PMM-кадров: чанки по 64 KiB, free-list, целиком свободный чанк возвращается в PMM; `Vec` проверен `selftest` | `src/memory/{heap,selftest}.rs` |
| Страницы (Sv39) | готово | identity-карта 4 KiB-страниц, `satp` включён: `[RAM base, __kernel_end)` RWX, выше — RW **без X**, страницы устройств RW без U; таблицы берутся из PMM, пустые возвращаются при `unmap`. User-страницы живут в этой же карте с `U=1`; отдельного `satp` на процесс нет | `src/memory/paging.rs`, `src/arch/riscv64/mmu.rs`, `src/user/elf.rs` |
| Таймер | готово | тик 100 Гц через SBI TIME: абсолютный дедлайн (+10 мс на прерывание, без накопления дрейфа), `sie.STIE`, `sleep`/`uptime` в тиках; холостой цикл — `wfi` | `src/arch/riscv64/timer.rs` |
| Прерывания / PLIC | готово | PLIC из DTB (`riscv,plic0`), контекст S-mode, UART source из DTB (10 на QEMU `virt`), claim/complete, счётчики и `overrun`; диспетчер различает timer/external и возвращается через `sret` | `src/arch/riscv64/plic.rs`, `src/arch/riscv64/traps.rs` |
| Блочное устройство | готово | virtio-mmio (modern virtio 1.x, version 2), polled: окна транспорта находятся через DTB, read/write/flush, опрос завершения с ограничением и FAILED после таймаута; `-global virtio-mmio.force-legacy=false` обязателен (иначе QEMU даёт legacy-транспорт) | `src/drivers/block.rs`, `src/drivers/virtio_mmio.rs` |
| Файловая система (ext2) | готово | чтение/запись файлов (direct + single/double indirect), truncate, каталоги, create/unlink/mkdir/rmdir/rename, симлинки; совместимо с образом хостового `mkfs.ext2 -t ext2`; форматируется только пустое устройство (MBR/GPT/чужие данные — отказ с причиной); журнала/WAL нет | `src/fs/ext2/`, `src/fs/format_policy.rs` |
| VFS | готово | `VfsNode`, синтетический корень, монтирование в `/mnt`, два примитива разрешения путей (`lookup_path` без последней симлинки / `walk` с переходом и бюджетом) | `src/vfs/mod.rs` |
| Планировщик, потоки ядра (M6a) | готово | контекстный switch (callee-saved ABI), до 8 потоков, round-robin, квант 8 тиков, стек 16 KiB + guard из PMM, вытеснение тиком таймера из trap-контекста (включая вытеснение U-mode), `tasks`/`spawn`/`yield`; exit возвращает кадры | `src/arch/riscv64/switch.rs`, `src/task/` |
| User mode, syscalls, ELF, native ABI (M6b) | готово | `sret` в U-mode (`SPP=0`, `SPIE=1`, `SUM=0`, `sscratch`-контракт); вход `a0=argc`, `a1=argv`, SysV-кадр `argc/argv/envp` с `argv[0]` как набран и `PWD=<cwd>`; `ecall`-диспетчер: `read`/`write`/`openat`/`close`/`lseek`/`fstat`/`getdents64`/`brk`/`clock_gettime`/`nanosleep`/`uname`/`readlinkat`/`exit`/`sched_yield`/`getpid`, `sepc+4`, таблица 16 fd на прогон, `copy_from_user`/`copy_to_user` с проверкой `U/R/W` и `SUM` вокруг копии, heap-окно 1 MiB с возвратом кадров; ELF64-загрузчик с откатом кадров и **потоковым чтением** `ImageSource` (сегменты кусками ≤ 512 B, без буфера размером с файл), `run <path> [args...]` до 1 MiB и фикстуры `demo`/`fault`/`spin`/`argv`/`abi`; fault из U убивает только задачу; таймер вытесняет U-mode | `src/user/`, `userland/` |
| Сеть (M7) | готово | virtio-net (modern, polled) + свой стек: Ethernet/ARP/IPv4/ICMP/UDP/TCP, DHCP-клиент, DNS-резолвер, `net`/`dhcp`/`ping`/`dns`/`http`; без smoltcp и внешних крейтов | `src/net/`, `src/drivers/virtio_net.rs` |
| Пакеты (`pkg`) | MVP | нативный установщик (не apt): плоский `index` и `.ppkg` с репозитория по **plain HTTP** (`http://IP:port/`, в QEMU хост — `10.0.2.2`), потоковая загрузка в `/mnt/pkg` кусками 4 KiB, шлюз «размер + SHA-256 из индекса» **до** разбора `PPKG0001`; `kind=app` → `/mnt/bin/<name>`, `applet-bundle` → `/mnt/bin/paghbox` + симлинки; манифест в `/mnt/pkg/<name>.manifest`; TLS/OpenPGP/apt по-прежнему нет | `src/pkg/` |
| apt / TLS / OpenPGP | **нет** | apt намеренно не подключён, пока нет vendored TLS + проверки подписей Debian; `pkg` — отдельный нативный путь для портов | — |
| Тесты | готово | host-тестов нет; in-QEMU `selftest` — 7 памяти + 4 блока/указателя + 11 ext2/VFS + 4 прерываний/таймера + 5 планировщика + 10 user-mode (ABI-фикстуры `argv`/`abi`, 10/10 с диском, 8/10 + 2 SKIP без `/mnt`) + 11 сети (идемпотентность по ledger'у), QEMU-smoke в шести режимах (serial, ramfb, диск-fresh, диск-existing, net, keyboard), установка пакета проверяется отдельным тестом `pkg`; проверка пикселей `tools/font_check.py` | `src/*/selftest.rs`, `src/net/selftest.rs`, `src/pkg/`, `.github/smoke.py`, `tools/` |
| CI | готово | `fmt`, debug/release build, QEMU-smoke в шести режимах: serial, ramfb, диск-fresh (формат/монтирование/запись), диск-existing (повторная загрузка читает файл), net (DHCP + ping шлюза), keyboard (virtio-keyboard печатает `help`/`halt` через монитор QEMU); во всех приложен virtio-tablet, в keyboard — ещё и virtio-keyboard | `.github/workflows/ci.yml` |

---

## Быстрый старт

```sh
rustup target add riscv64gc-unknown-none-elf   # nightly запинен в rust-toolchain.toml
cargo build                                    # debug-ядро, линкуется linker.ld
./run.sh                                       # QEMU virt + OpenSBI: окно и serial-консоль одновременно
```

`run.sh` — канонический способ запуска (Linux/QEMU). Он собирает ядро и выполняет
по сути это:

```sh
qemu-system-riscv64 -machine virt -m 256M -device ramfb \
                    -kernel target/riscv64gc-unknown-none-elf/debug/pagh \
                    -display none -serial mon:stdio -no-reboot
```

`-device ramfb` — это и есть экран: у машины `virt` нет VGA, а ядро находит
устройство через `fw_cfg`. По умолчанию поверхность рисуется **off-screen**
(`-display none`), поэтому `screendump` и `tools/run.py` работают без графической
сессии. `./run.sh` при наличии дисплея (`DISPLAY`/`WAYLAND_DISPLAY`) открывает
окно QEMU с заголовком `pagh-riscv`, а `--headless` оставляет только serial;
`run.sh` подключает
и клавиатуру (`-device virtio-keyboard-device`): у `virt` нет PS/2, поэтому ввод
из окна идёт как virtio-input и попадает в тот же шелл, что и serial — можно
пользоваться любым из двух. Если запустить QEMU без `-device ramfb`, ядро остаётся
serial-консолью и честно печатает
`framebuffer  ramfb: no such fw_cfg item — serial console only`.

Сеть включена по умолчанию: `run.sh` добавляет `-netdev user -device
virtio-net-device`, ядро получает `10.0.2.15/24` по DHCP, шлюз `10.0.2.2`
(он же loopback хоста, поэтому `http 10.0.2.2 <port>` достучится до слушателя на
хосте) и DNS `10.0.2.3`. `NO_NET=1 ./run.sh` — машина без сетевой карты, путь
«NIC отсутствует» тоже проверяется smoke-тестом.

| Аргумент / переменная | По умолчанию | Смысл |
|---|---|---|
| `--window` | авто: включено при наличии дисплея | показать экран окном QEMU (`-display gtk`) |
| `--headless` | авто: выключено при наличии дисплея | serial-only: рисовать off-screen |
| `PROFILE` | `debug` | `release` — собрать и запустить release-ядро |
| `MEMORY` | `256M` | объём RAM у `virt` |
| `BIOS` | — (QEMU `-bios default`) | внешняя прошивка, например `BIOS=/path/opensbi.bin` |
| `DISK` | — | образ virtio-mmio диска; отсутствующий создаётся пустым (64 MiB), пустой форматируется ядром и монтируется в `/mnt` |
| `NO_NET` | `0` | `1` — не подключать virtio-net (проверка отсутствия NIC и старых сценариев) |
| `NO_MOUSE` | `0` | `1` — не подключать virtio-tablet (проверка отсутствия указателя: курсора нет, всё остальное работает) |

Выход из QEMU — `Ctrl-A`, затем `X`. Изнутри гостя: команда `halt` (SBI SRST,
машина действительно выключается, QEMU завершается с кодом 0).

Автоматические проверки «ядро живо» — те же, что в CI:

```sh
cargo build
python3 .github/smoke.py target/riscv64gc-unknown-none-elf/debug/pagh           # только serial
python3 .github/smoke.py target/riscv64gc-unknown-none-elf/debug/pagh --ramfb   # + экран
python3 tools/run.py --keys "help\n" --shot /tmp/shot.png --check-font          # пиксели против ассета
```

M6b вживую (те же команды понимает шелл):

```sh
# после prompt'а pagh:rv64>
run demo        # hello from user mode + run: demo exited with code 42
run demo a b    # то же: у фикстуры по контракту argc=1, лишние слова не мешают
run fault       # [user] fault killed: load page fault ... ; шелл жив
run spin        # 0.2 s в U-mode (таймер вытесняет) -> exit 7
selftest        # + SELFTEST user-*: 10/10 с /mnt (без диска 8/10 + 2 SKIP)
```

### Native ABI для libpagh

`run <path> [args...]` входит в U-mode по контракту
`pagh-api/docs/abi.md`: `a0 = argc`, `a1 = argv`, `sp` выровнен на 16 байт и
указывает на SysV-кадр `[argc][argv0..][NULL][env0..][NULL][строки]`.
`argv[0]` — путь **как набран** в шелле, окружение — `PWD=<cwd>`; `envp`
программа восстанавливает из `sp` (`argv + argc + 1`), отдельного регистра под
него нет. Аргументы после пути делятся по пробелам: кавычек и экранирования в
шелле нет, поэтому аргумент с пробелом передать нельзя. У встроенных фикстур
`argc = 1` (`argv = [demo]`), лишние слова после `run demo` игнорируются.

| nr | вызов | семантика |
|---:|---|---|
| 63 | `read` | fd 0 — консоль: UART-кольцо, затем virtio-клавиатура, иначе `-EAGAIN` (ядро **никогда не блокирует**); файл — с offset; каталог — `-EISDIR` |
| 64 | `write` | fd 1/2 — консоль; файл — по offset, `O_APPEND` пишет в конец; fd 0 — `-EBADF` |
| 56 | `openat` | `dirfd` игнорируется, путь только абсолютный; `O_RDONLY/WRONLY/RDWR`, `O_CREAT`, `O_TRUNC`, `O_APPEND`; `mode` принимается и **игнорируется** (в VFS нет `set_mode`, ext2 даёт 0o644) |
| 57 | `close` | освобождает дескриптор; 0/1/2 тоже можно закрыть — до конца прогона |
| 62 | `lseek` | SET/CUR/END, знаковый 64-битный offset; консоль и каталог — `-EINVAL` |
| 80 | `fstat` | нативный `Stat` (24 B): режим, тип, размер, ino из `VfsNode`; синтетические узлы — 0o755/0o644 |
| 61 | `getdents64` | снимок детей каталога на `openat`, записи `Dirent` (12 B заголовок + имя, выравнивание 8); курсор двигается только за скопированные записи |
| 214 | `brk` | окно 1 MiB по фиксированному адресу (см. ниже); `brk(0)` — текущий break; возвращаемое значение всегда выровнено вверх на страницу, рост/сжатие постранично, за окном — `-ENOMEM` |
| 113 | `clock_gettime` | id 0/1 — тиковое время ядра (`Timespec`, разрешение 10 ms), другие id — `-EINVAL` |
| 101 | `nanosleep` | `Timespec` проверяется и округляется **вверх** до тика; `rem` игнорируется; ждёт в `wfi` с разрешёнными прерываниями |
| 160 | `uname` | 5×16 байт: `pagh`, `pagh-riscv`, `0.10.0`, версия пакета, `riscv64` |
| 78 | `readlinkat` | цель символической ссылки без NUL; не ссылка — `-EINVAL` |
| 172 | `getpid` | id текущей задачи |
| 124 | `sched_yield` | добровольное переключение |
| 93 | `exit` | `Exit::Code(a0)`, не возвращается |

Пределы и отличия, которые важно знать userland'у: 16 дескрипторов включая
0/1/2 (кончились — `-ENOSPC`); путь ≤ 256 байт с NUL; `argv`+`env` ≤ 64 строк и
≤ 16 KiB; heap — ровно 1 MiB в `0x2000_0000..0x2010_0000`, кадры под него
возвращаются в PMM после прогона (включая fault); `mmap`/`fork`/`exec` нет.
`getdents64` при буфере меньше одной записи возвращает 0 — как конец каталога;
буфер на 264 байта покрывает самое длинное имя ext2. Неизвестный номер —
`-ENOSYS`; плохой указатель — `-EFAULT`, а не page fault.

### Как управлять шеллом

Промпт `pagh:rv64> `, строка до 128 байт (более длинная отвергается целиком),
backspace работает, всё остальное — команды:

| Команда | Что делает |
|---|---|
| `help` | список команд |
| `mem` | память из device tree **и** статистика PMM и кучи (свободно/всего кадров, диапазон пула, байты кучи) |
| `selftest` | in-QEMU self-tests: 7 памяти, 4 блока/указателя, 11 ext2/VFS, 4 таймера/PLIC, 5 планировщика, 10 user-mode (без `/mnt` два SKIP — `user-run-large-file` и `user-abi-syscalls`, итог 8/10); печатает ledger свободных кадров до и после каждого блока |
| `disk` | блочное устройство: имя, ёмкость, живая проверка чтения сектора 0 |
| `ls [path]` | листинг каталога (каталоги с `/`) |
| `cat <path>` | содержимое файла |
| `stat <path>` | путь, имя, тип, режим, размер, inode, ссылки |
| `cd [path]` / `pwd` | текущий каталог (лексическое склеивание, без `..` из монтирования) |
| `mkdir <path>` / `touch <path>` | создать каталог / пустой файл |
| `write <path> <text>` | заменить содержимое файла текстом |
| `rm [-r] <path>` | удалить файл, каталог — с `-r` |
| `cp <src> <dst>` / `mv <src> <dst>` | копировать / переименовать |
| `ln [-s] <target> <path>` | жёсткая или символическая ссылка |
| `ramdisk [sectors]` | **только для тестов**: отформатировать RAM-диск и смонтировать его в `/mnt` |
| `run <path> [args...]` | исполнить статический RV64 ELF из VFS в U-mode: файл читается **потоково** (ELF-заголовок, program headers и сегменты кусками ≤ 512 B), без буфера размером с файл; `args` уходят в `argv[1..]`, `argv[0]` — путь как набран; образ ≤ 1 MiB, один user-образ за раз |
| `run demo` / `run fault` / `run spin` | встроенные фикстуры: hello (exit 42), падение по load page fault (задача убита, машина жива), 0.2 s спина в U-mode (exit 7); `argc = 1`, лишние слова игнорируются |
| `net` | MAC, `ip/mask`, gateway, DNS и срок DHCP-lease |
| `dhcp` | повторить DHCP-обмен (DISCOVER/OFFER/REQUEST/ACK) и напечатать новый lease |
| `ping <ip> [n]` | до 4 ICMP echo через свой ARP/IPv4/ICMP-путь; печатает `seq`, время и итог |
| `dns <name>` | A-запись через DNS-сервер из lease |
| `http <ip> <port> <path>` | HTTP/1.0 GET через TCP-клиент; ответ печатается в консоль |
| `pkg repo [URL]` / `pkg index` / `pkg list` | показать/задать репозиторий (по умолчанию `http://10.0.2.2:8000/`), скачать и разобрать индекс, показать пакеты с `[installed]` |
| `pkg install <name>` / `pkg remove <name>` / `pkg installed` | скачать, проверить размер+SHA-256, установить в `/mnt/bin`; удалить по манифесту; список установленного |
| `time` | два счётчика: CSR `time` (аппаратный) и тики ядра; uptime, число прерываний таймера, статистика PLIC/UART |
| `hart` | id hart'а, на котором работает ядро |
| `echo <text>` | печатает текст (слова через один пробел) |
| `clear` | очистка экрана ANSI-последовательностью (чистит и экран, и терминал) |
| `sleep <secs>` | ждать столько секунд тиков (`wfi`, не спин) |
| `tasks` | потоки ядра: id, имя, состояние, переключения, работа, стек |
| `spawn [n]` | запустить n демо-потоков (по умолчанию 3; 0 — остановить) |
| `yield [n]` | прогнать n тиков планировщика и показать число переключений |
| `halt` | выключение машины через SBI SRST |

Истории, автодополнения и стрелок нет — это следующий уровень шелла, а не
забытая деталь.

### Пакеты: `pkg` (нативный установщик)

`pkg` ставит в `/mnt` один статический RV64 ELF из `.ppkg`, скачанного с
плоского HTTP-репозитория. Это **не apt**: ни Debian-метаданных, ни `.deb`, ни
solver'а зависимостей, ни сжатия, ни OpenPGP; TLS тоже нет. Репозиторий — это
файл `index` и каталог `pkg/*.ppkg`; формат описан в `docs/package-format.md`
репозитория [`pagh-programs`](https://github.com/ccclosed/pagh-programs).

```sh
# на хосте (в pagh-repo): зеркало для QEMU user network
./serve.sh                                   # python3 -m http.server 8000 -d .

# в госте, после prompt'а pagh:rv64>
pkg repo http://10.0.2.2:8000/               # 10.0.2.2 — loopback хоста
pkg index                                    # pkg: index 1 package(s)
pkg list                                     # hello 1.0.0 13555 B
pkg install hello                            # pkg: installed hello 1.0.0
run /mnt/bin/hello                           # hello from user mode / exited with code 42
pkg installed                                # pkg: hello 1.0.0
pkg remove hello                             # pkg: removed hello
```

**Шлюз — размер и SHA-256.** Пакет пишется в `/mnt/pkg/<name>.ppkg` потоково,
кусками 4 KiB, и те же байты считаются в SHA-256. Пока счётчик и дайджест не
совпали с индексом, в `/mnt/bin` не создаётся ничего; при несовпадении
скачанный файл удаляется. Только после шлюза читаются `PPKG0001`, длины и
манифест, а payload копируется в `/mnt/bin/<name>` (для `applet-bundle` —
`paghbox` плюс симлинки из `commands`). Манифест `/mnt/pkg/<name>.manifest`
пишется последним, и `pkg remove` работает по нему.

Это проверка **целостности**, а не подписи: зеркало, которое отдаёт другой
пакет с тем же размером и хешем, дизайном не ловится. HTTPS и OpenPGP — следующий
майлстоун; до него `pkg` честно говорит «only plain http:// is supported
(there is no TLS yet)» на `https://` и не притворяется apt.

Ограничения MVP: до 64 пакетов в индексе, IPv4-хост в URL (без DNS), один
пакет за раз, `payload ≤ 8 MiB`, до 32 команд в `applet-bundle`, `/mnt`
обязателен. `pkg` без аргументов
печатает usage; `pkg selftest` проверяет SHA-256 по векторам FIPS.

Полный E2E (сборка `.ppkg` из `userland/hello.elf`, HTTP-сервер, QEMU с
диском, включая отказ испорченному пакету):

```sh
python3 tools/test_pkg.py
```

---

## Архитектура

Подробно, с ссылками на файлы и строки — [`docs/architecture.md`](docs/architecture.md).
Здесь — короткая версия.

**Путь загрузки.** OpenSBI (M-mode) передаёт управление в S-mode на `_start` с
`a0` = hart id и `a1` = адрес DTB, без стека и с выключенными прерываниями.
Дальше фазы идут строго по порядку, каждая опирается на предыдущую:

| # | Фаза | Код |
|---|---|---|
| 1 | `_start`: OpenSBI запускает ровно один hart (любого id); стек и `kmain` | `src/arch/riscv64/boot.rs` |
| 2 | стек: `sp = __stack_top`, 64 KiB в `.bss`, выравнивание 16 | `src/arch/riscv64/boot.rs` |
| 3 | захват hart id в `BOOT_HART_ID` (пока он в `a0`) | `src/arch/riscv64/boot.rs` |
| 4 | `call kmain(a0, a1)` | `src/arch/riscv64/boot.rs` |
| 5 | консоль: `uart::init()` + попытка поднять экран (`console::init` → `framebuffer::init`) | `src/main.rs`, `src/console.rs` |
| 6 | баннер: hart, адрес DTB | `src/main.rs` |
| 7 | разбор DTB; ошибка → печать и остановка, без угадывания | `src/main.rs`, `src/fdt.rs` |
| 8 | память и timebase из DTB — в лог | `src/main.rs` |
| 9 | PMM: пул кадров по `/memory` минус образ ядра, стек и блоб DTB | `src/main.rs`, `src/memory/` |
| 10 | `traps::init()`: `stvec` + `sstatus.SIE` — до всего, что может сбойнуть | `src/main.rs`, `src/arch/riscv64/traps.rs` |
| 11 | `shell::run(tree)` — цикл консоли, не возвращается | `src/main.rs`, `src/shell.rs` |

**Привилегии.** Ядро живёт в S-mode; M-mode принадлежит прошивке, и обращаться
туда можно только через SBI (`ecall`) — именно так работает `halt`. M-mode CSR
из S-mode недоступны физически: `csrr …, mhartid` — это illegal instruction, и
ядро узнало это на своём опыте (см. `docs/architecture.md`, раздел 2). `satp`
включает Sv39 с identity-картой (страницы 4 KiB, без user-бита); U-mode нет.

**Устройства.** Единственное описание машины — device tree: ядро читает из него
память и частоту timebase, отказываясь угадывать при ошибке разбора. Адреса UART
и `fw_cfg` продублированы константами только потому, что консоль нужна раньше
разбора (у обоих в коде написано, почему, и оба падают громко, а не молча).

**Сеть.** `virtio_net` — тот же modern virtio-mmio (две очереди, polled), а над
ним свой стек: `wire.rs` знает байты и контрольные суммы, `mod.rs` — ARP-кэш,
IPv4, ICMP и UDP, а `dhcp`/`dns`/`tcp` — маленькие клиенты. Всё синхронно:
`net::poll()` вызывается из сетевых команд и idle-цикла шелла, а ожидание —
опрос до дедлайна в тиках, поэтому сеть не требует ни потока, ни прерывания.
Детали и честные границы — [`src/net/README.md`](src/net/README.md).

**Память.** Ядро линкуется на `0x8020_0000` (`linker.ld`), `.bss` — 8 МиБ:
две поверхности экрана (тень и scanout), битовая карта PMM, стек 64 KiB и
статики; `__kernel_end` выровнен на 4 KiB. `.bss` не обнуляется кодом ядра:
обнуляет ELF-загрузчик QEMU (NOBITS-сегмент) — и это существенно, потому что
полярность карты PMM («бит установлен = кадр свободен») держится именно на нулях.
Полная карта — в `docs/architecture.md`, раздел 6.

**Ловушки.** `trap_entry` сохраняет 33 регистра в кадр на 264 байта; смещения —
это ABI, они совпадают с порядком `sd` в ассемблере. Исключение печатает
`[EXC] <имя> (scause=…, stval=…, sepc=…)` и останавливает hart; прерывание
возвращается через `sret`. Паники в обработчике нет и быть не должно: при
`panic = "abort"` она убила бы машину, оставив кадр единственной уликой.

---

## Дорожная карта

| # | Майлстоун | Статус |
|---|---|---|
| M1 | Загрузка под OpenSBI, DTB, ловушки, UART-консоль и шелл | **готов** |
| M2 | Физический аллокатор кадров, Sv39, куча ядра, `alloc` | **готов** (identity Sv39 с W^X, куча и `alloc`; page-fault ловушки — M6b для U-mode) |
| M3 | Таймер через SBI, PLIC, консоль на прерываниях | **готов** (100 Гц, PLIC, UART RX в кольцо) |
| M4 | ramfb + шрифт покрытия Fira Code, съёмка скриншотов | **готов** (экран, шрифт и снимок проверены) |
| M5 | virtio-mmio блочное устройство, ext2, VFS | **готов** (virtio-mmio, ext2 rw + формат пустого диска, VFS, шелл-команды; журнала нет) |
| M6 | Задачи, syscalls, Linux-compat-слой из x86-ветки | M6a (планировщик/потоки) — **готов**; M6b (user mode, syscalls, ELF) — **готов**; Linux-compat — нет |
| M7 | Сеть: virtio-net и свой TCP/IP-стек | **готов** (Ethernet/ARP/IPv4/ICMP/UDP/TCP, DHCP, DNS, `net`/`ping`/`dns`/`http`; TLS/apt — следующий шаг) |

Что считается «сделано» для каждого майлстоуна и что в него не входит —
[`docs/roadmap.md`](docs/roadmap.md).

---

## Чего нет

Этот список — часть документации, а не извинение. Если подсистемы здесь нет,
значит её нет в коде.

* **Нет адресного пространства на процесс.** Sv39 включён, но карта одна:
  identity для RAM, плюс user-окно с `U=1` (образ, стек, heap) в той же таблице.
  `satp.ASID = 0`, VMM/mprotect/разделяемых страниц нет; два user-процесса друг
  от друга не защищены — только юзер от ядра. `run_image` держит «одна программа
  за раз».
* **Нет `mmap`, `mprotect`, `fork`/`exec`, сигналов и динамического ELF.**
  `brk` есть, но это одно фиксированное окно 1 MiB (`0x2000_0000`), а не общий
  менеджер памяти; `read(0)` неблокирующий (`-EAGAIN`), `getdents64` отдаёт
  снимок каталога на момент `openat`, `openat(... O_CREAT)` игнорирует `mode`
  (в VFS нет `set_mode`).
* **Нет `fork`/`exec`, сигналов и динамического ELF.** Загрузчик понимает только
  статические `ET_EXEC`/`ET_DYN` без интерпретатора; userland — голые `ecall`.
  `run <path>` читает файл *ядром* потоково, частями не больше 512 B, и грузит
  его в user-окно; предел образа — 1 MiB (`MAX_IMAGE_BYTES`), одновременно
  исполняется один user-образ, отдельного адресного пространства процесса и
  отображения `/mnt` в него нет.
* **Нет журнала/WAL в ext2.** Сбой между двумя записями может оставить операцию
  недоделанной; crash-consistency не заявляется.
* **Один блочный слот, нет разделов/hot-plug.** Второй диск — изменение дизайна.
* **Нет TLS, подписей пакетов и apt.** Нативный `pkg` говорит по plain HTTP и
  проверяет только размер + SHA-256 из индекса: это целостность, а не
  аутентификация источника. TLS-стек и OpenPGP-проверка подписей — следующий
  майлстоун; `apt` намеренно не подключается, пока их нет: «скачали и поставили
  без проверки» — это ровно то, чего fail-closed apt не делает.
* **TCP-клиент минимальный.** Одно active-open соединение, без
  retransmission-таймера (кроме двух SYN), out-of-order reassembly, congestion
  control и прослушивания портов; IPv6, фрагментация и несколько интерфейсов не
  поддерживаются.
* **Нет SMP.** Один hart, остальные запаркованы в `_start`; планировщик и локи
  это предполагают.
* **Нет восстановления после S-mode исключения.** Сбой в ядре — печать и
  остановка hart. Сбой в U-mode (page fault, illegal instruction) завершает
  только задачу.
* **Нет host-тестов.** Логика ядра покрыта in-QEMU `selftest` и smoke-тестом;
  чистая логика (`src/fdt.rs`) не покрыта ничем.
* **Нет поддержки реального железа.** Адреса UART и `fw_cfg` захардкожены,
  поддержан только QEMU `virt` с `-kernel` + OpenSBI.
* **Сборка даёт предупреждения** (неиспользуемые диагностические хелперы,
  приведение функции к числу в `traps.rs`); CI их не гейтит.
* **LICENSE-файла нет**, хотя `Cargo.toml` объявляет MIT.

## Устройство репозитория

```text
src/main.rs                    boot-фазы по порядку, баннер, panic_handler
src/arch/riscv64/boot.rs       _start: стек, парковка hart'ов, вызов kmain
src/arch/riscv64/csr.rs        типизированный доступ к CSR
src/arch/riscv64/traps.rs      trap_entry, кадр на 264 байта, диспетчер
src/arch/riscv64/sbi.rs        ecall в M-mode (SRST)
src/arch/riscv64/mmu.rs        примитивы Sv39 (PTE, PageTable, satp, sfence)
src/memory/paging.rs           identity-карта ядра, map/unmap/translate, satp
src/fdt.rs                     минимальный читатель flattened device tree
src/memory/{mod,pmm}.rs        пул физических кадров (битовая карта 4 KiB)
src/memory/heap.rs             куча: free-list поверх чанков по 64 KiB
src/memory/selftest.rs         in-QEMU self-tests памяти (selftest)
src/user/mod.rs                run_image: ELF -> user-стек -> задача -> sret в U-mode
src/user/elf.rs                ELF64-загрузчик: валидация, PT_LOAD с U=1, откат кадров
src/user/syscall.rs            ecall-диспетчер: read/write/openat/close/lseek/fstat/getdents64/brk/time/uname/readlinkat
src/user/selftest.rs           in-QEMU self-tests user mode (10 проверок)
src/net/mod.rs                 сетевой стек: poll, ARP-кэш, диспетчер кадров, ping
src/net/wire.rs                чистые форматы Ethernet/ARP/IPv4/ICMP/UDP/TCP/DHCP/DNS
src/net/{dhcp,dns,tcp}.rs      DHCP-клиент, DNS-резолвер, минимальный TCP-клиент
src/net/selftest.rs            in-QEMU self-tests сети (11 проверок)
src/net/README.md              дизайн стека: опрос, wire, DHCP/DNS/TCP, чего нет
userland/{hello,fault,spin,argv,abi}.rs статические RV64-фикстуры + user.ld + build.sh
userland/README.md             как пересобрать и что доказывает каждая фикстура
src/memory/README.md           дизайн памяти: кадры, куча, чего нет
src/drivers/uart.rs            NS16550, polled
src/drivers/fw_cfg.rs          QEMU fw_cfg: селектор/данные + DMA
src/drivers/ramfb.rs           конфигурация устройства ramfb
src/drivers/framebuffer.rs     текстовый экран: тень, шрифт, статус-бар
src/drivers/virtio_net.rs      virtio-net: две очереди, polled, MAC, RX/TX-кадры
src/drivers/virtio_input.rs    virtio-input: tablet/mouse, события → координаты указателя
src/console.rs                 kprint!/kprintln!: UART + экран
src/shell.rs                   редактор строки и команды
src/sync.rs                    один нереентерабельный спинлок
docs/architecture.md           загрузка, кадр ловушки, привилегии, карта памяти
docs/roadmap.md                майлстоуны M1–M7 и их критерии
docs/shell.png                 снимок экрана из работающего QEMU
docs/user.png                  снимок run demo / run fault / run spin
docs/net.png                   снимок net / ping / dns через собственный стек
assets/font8x18.bin            шрифт покрытия 8×18 (8 бит на пиксель)
tools/run.py                   запуск + screendump + проверка шрифта
tools/gen_font.py              офлайн-растеризация шрифта в ассет
tools/font_check.py            сверка пикселей снимка с ассетом
.github/smoke.py               QEMU-smoke: serial и ramfb режимы
.github/workflows/ci.yml       fmt + debug/release build + smoke
run.sh                         единственный лаунчер (QEMU virt + OpenSBI)
linker.ld                      один сегмент, точка входа _start, 0x8020_0000
```

Правило репозитория: **каждая подсистема документирует себя** — у `src/<подсистема>/`
должен быть README, совпадающий с кодом (`src/README.md`, `src/arch/README.md`,
`src/drivers/README.md`, `src/memory/README.md`). Изменение поведения без изменения
документации здесь считается ошибкой.

## Качество и CI

| Гейт | Команда |
|---|---|
| Форматирование | `cargo fmt --all -- --check` |
| Debug-сборка | `cargo build --locked` |
| Release-сборка | `cargo build --locked --release` |
| Дымовой тест: serial | `python3 .github/smoke.py target/riscv64gc-unknown-none-elf/debug/pagh` |
| Дымовой тест: экран | `python3 .github/smoke.py target/riscv64gc-unknown-none-elf/debug/pagh --ramfb` |
| Дымовой тест: сеть | `python3 .github/smoke.py target/riscv64gc-unknown-none-elf/debug/pagh --net` |

Всё это выполняется в CI (`.github/workflows/ci.yml`) на запиненном nightly из
`rust-toolchain.toml`: `fmt` и обе сборки — отдельными job'ами, а smoke-режимы —
шагами одного job'а. `--ramfb` добавляет `-device ramfb`, так что поднятие экрана
проверяется без дисплея; `--net` добавляет virtio-net на QEMU user network и
проверяет DHCP, `net` и ping шлюза, не завися от интернета раннера. Smoke-тест ждёт
баннер (UART очищает FIFO при инициализации, поэтому команды, отправленные
раньше, теряются), шлёт `mem`, `hart`, `time`, `echo`, неизвестную команду,
`help` и `halt`, и требует, чтобы после `halt` QEMU завершился с кодом 0.
Сетевой режим шлёт `net`, `ping 10.0.2.2` и `halt` и требует строку
`ping: reply from 10.0.2.2`.

## Родословная

Второе ядро в семье: x86-64-версия — [ccclosed/Pagh-OS](https://github.com/ccclosed/Pagh-OS)
(Limine/UEFI, ext2 + WAL, свой TCP/IP, apt, framebuffer-GUI, Linux-compat
userland). Идея та же — маленькое `#![no_std]`-ядро, которое сразу даёт
работающую консоль, — но машина описывает себя device tree вместо firmware-таблиц,
а весь код здесь написан заново под RV64. Часть майлстоунов (M4–M6) — это
сознательное повторение x86-пути на новой архитектуре.

Ядро написано ИИ под надзором человека. Лицензия — MIT (`Cargo.toml`).