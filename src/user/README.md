# `src/user/` — user mode, native syscall ABI и ELF

Первое место, где код исполняется с U-битом в PTE: `sret` в U-mode,
`ecall`-диспетчер, ELF64-загрузчик и маленькие статические программы из
[`userland/`](../../userland/README.md). Каждый процесс получает **свою**
Sv39-карту: `paging::create_user_space` копирует supervisor-отображения ядра, а
загрузчик добавляет в этот root только страницы процесса. Контракт входа и
syscall'ов заморожен в `pagh-api/docs/abi.md`; ниже — как он реализован и где
пределы.

## Карта

| Файл | Роль |
|---|---|
| `mod.rs` | `run_image(bytes, name)` / `run_image_with_args` для фикстур и `run_file(path)` / `run_file_with_args` / `run_file_with_argv0` для `run <path>` — весь сценарий запуска: новый root/ASID → ELF в него → user-стек → SysV-кадр `argc/argv/envp` → `spawn_in_group_satp` главного потока → `enter_user` (`a0=argc`, `a1=argv`, `sret`) → ожидание `Process::threads == 0` → `reap` → снятие маппингов (включая выросший стек), fd и heap-кадров; `FileSource` поверх `VfsNode` (потоковое чтение), `Exit`/`UserError`, `on_fault` (убивает процесс), `brk`, `mmap`, `exit_thread`/`exit_group`/`process_id`/`parent_id`, `fork`/`execve`/`wait4` (fork копирует и выросшие страницы стека, exec сбрасывает `stack_low`), очистка сирот, capture для ABI-теста, встроенные фикстуры `HELLO_ELF`/`FAULT_ELF`/`SPIN_ELF`/`ARGV_ELF`/`ABI_ELF`/`EXEC_CHILD_ELF`/`STACK_ELF`/`STACK_OVER_ELF` |
| `process.rs` | таблица процессов: `MAX_PROCESSES = 6`, `Process { pid, ppid, root, satp, asid, name, cmdline, entry/sp/argc/argv, main_tid, threads, exit, signal, heap, mmap, stack_base, stack_low, wait_tid, ready }`; `cmdline` — bounded 128-байтный снимок argv при `run`/`execve` (источник `/proc/<pid>/cmdline`, `fork` копирует), `Free -> Live -> Zombie -> Free`, `reserve`/`publish`/`slot_of`/`with_slot_mut`, отдельные статические `IMAGES`/`HEAPS`, `ready_pids`/`live_pids`/`is_live` и `force_finish`; `kill_group` работает по `group == pid` |
| `stack.rs` | деманд-рост user-стека: `try_grow(scause, stval, user_sp)` из trap-диспетчера мапит и обнуляет страницы от `Process::stack_low` вниз до адреса fault'а (load/store, не ниже `USER_STACK_FLOOR`, не дальше `FAULT_SLACK = 64 KiB` от `sp`, ни одна страница диапазона не занята), `release(root, low)` возвращает выросшие кадры при teardown, `stats()` (`growths`/`pages`/`refused`/`refused_frames`) — счётчики для selftest; потолок `USER_STACK_MAX_SIZE = 1 MiB`, `MAX_GROWTH_PAGES = 240` |
| `signal.rs` | POSIX-подобные сигналы: per-process таблица `sigaction` (1..=31, строка привязана к слоту процесса), per-thread `pending`/`blocked`/`altstack`/активный кадр; `fork_process` копирует обработчики/blocked/altstack, `exec_process` сбрасывает обработчики в `SIG_DFL`, гасит pending и alt-стек, `clear_process` освобождает строку, `kill`/`tgkill` (129/131), `sigaltstack` (132), `rt_sigaction`/`rt_sigprocmask`/`rt_sigreturn` (134/135/139), кадр 736 B в user-памяти с 128-байтным `siginfo_t` и 288-байтным `ucontext_t`, `SA_SIGINFO`/`SA_ONSTACK`/`SA_NODEFER`/`SA_RESETHAND`/`SA_RESTART`, `deliver_pending` на границе kernel→user, `deliver_fault` для SIGILL/SIGSEGV/SIGBUS/SIGTRAP и job control через `scheduler::stop_current`/`continue_task`; счётчики и `live_handlers`/`live_threads`/`live_pending` для selftest |
| `futex.rs` | статическая очередь ожидающих `(run, uaddr) -> tid`: 32 слота на 16 адресов, `wait` (регистрация → `scheduler::block_current` → снятие), `wake` (`wake_task`, не больше N), `clear_run` на teardown; лок очереди маскирует прерывания и **не** держится через блокировку; порядок `futex → scheduler` |
| `elf.rs` | ELF64-загрузчик: `ImageSource` (`len` + `read_at`) и потоковое чтение — заголовок, таблица program headers (≤ 8) и сегменты кусками ≤ 512 B в уже замапленные страницы, **без буфера размером с образ**; `load` — в активный root, `load_in(root, …)` — в root процесса; валидация заголовка/сегментов до первого кадра, `PT_LOAD`-страницы `U=1`, нулевой `.bss`-хвост, откат кадров при любой ошибке; `Loaded { root, entry, pages(), release() }` (Vec, не inline-массив), `from_pages` для fork; типизированные `ElfError`/`SourceError` |
| `syscall.rs` | `dispatch(frame)`: `read(63)`, `write(64)`, `openat(56)`, `close(57)`, `lseek(62)`, `fstat(80)`, `getdents64(61)`, `brk(214)`, `clock_gettime(113)`, `nanosleep(101)`, `uname(160)`, `readlinkat(78)`, `exit(93)`, `exit_group(94)`, `set_tid_address(96)`, `futex(98)`, `sched_yield(124)`, `getpid(172)`, `getppid(173)`, `gettid(178)`, `clone(220)`, `fork(1000)`, `execve(221)`, `wait4(260)`, сигналы `kill(129)`, `tgkill(131)`, `sigaltstack(132)`, `rt_sigaction(134)`, `rt_sigprocmask(135)`, `rt_sigreturn(139)` + мутации `mkdirat(34)`, `unlinkat(35)`, `symlinkat(36)`, `linkat(37)`, `fchmodat(53)`, `ftruncate(46)`, `renameat2(276)` + сокеты `socket(198)`, `bind(200)`, `connect(203)`, `sendto(206)`, `recvfrom(207)`, `setsockopt(208)`, `getsockopt(209)`, `shutdown(210)`; per-process таблица 16 fd (`FdTables`; `fork` копирует, `execve` сохраняет), per-process таблица режимов для `fchmodat`/`fstat`, `copy_from_user`/`copy_to_user` (проверка `U/R/W` по PTE, `SUM` только вокруг копии), `-errno`/`-ENOSYS`, `ecall → sepc + 4`, счётчики для selftest |
| `selftest.rs` | тринадцать проверок для `selftest`: `user-abi-argv` (точная строка из фикстуры), `user-abi-syscalls` (все вызовы + секции потоков/фьютексов, сигналов и процессов + мутационная половина: mkdir/rmdir, rename, link, symlink, chmod, ftruncate, unlink; маркеры `abi: fork ok`/`wait ok`/`exec ok`/`exec wait ok`, с диском exit 0 и «все имена убраны», без `/mnt` — SKIP после no-fs половины), accept фикстуры, reject-матрица (14 мутаций заголовка/сегментов), отказ `run_image` на битом ELF, round-trip `hello`, fault-изоляция, запуск после fault, таймер-in-U, `user-stack-grows` (160 KiB цепочка возвращается, маркер `stack: deep recursion ok`, счётчики роста сдвинулись ≥ 16 страниц, ledger цел), `user-stack-cap` (1.2 MiB цепочка умирает `Exit::Fault` с `cause = store page fault`, рост ровно на 240 страниц, отказ зафиксирован, ledger цел), `user-stack-fork` (цепочка форкается на 30-м уровне, когда ~14 страниц уже выращены; ребёнок доходит до конца на копии, родитель ждёт `wait4` и получает pid; счётчики `forks`/`waits` сдвинулись, ledger цел) и `user-run-large-file` (100 KiB образ с нулевым хвостом через `run_file`) |

## Вход в U-mode и начальный стек

`run_source` после маппинга 64 KiB user-стека строит SysV-кадр:

```text
sp -> [argc (8 B)][argv0 … argvN-1][NULL][env0 … envK-1][NULL][строки argv/env]
```

* строки растут вниз от `USER_TOP = 0x4000_0000`, `sp` вычисляется из их суммы,
  выравнивается вниз на 16 байт и указывает на `argc`;
* каждая строка пишется через `paging::translate` в **физический** кадр (как
  сегменты ELF), `SUM` не включается: это S-mode, пишущий через identity-карту,
  а не ядро, читающее user-указатель;
* кадр стека при маппинге обнуляется: PMM-кадр мог раньше быть kernel-стеком,
  и U-mode не должен читать его остатки;
* `argv[0]` — путь **как набран** в шелле (`run_file_with_argv0(open_path,
  argv0, …)`), `env` шелла — `PWD=<cwd>`; у фикстур `argc = 1` и `PWD=/`;
* `enter_user(entry, sp, argc, argv)` кладёт `a0 = argc`, `a1 = argv`, `sp` и
  делает `sret`; `envp` libpagh считает сам как `argv + argc + 1`.

Пределы кадра: `argv`+`env` ≤ 64 строк и ≤ 16 KiB, строка не может содержать
NUL (запрос отклоняется до маппинга, `UserError::ArgumentsTooLong`). Шелл режет
строку по пробелам: кавычек и экранирования нет.

## Растущий user-стек (`stack.rs`)

64 KiB — это **стартовое** окно, а не предел. `Process::stack_low` хранит
текущее дно (изначально `USER_STACK_BASE = 0x3fff_0000`); U-mode load/store page
fault ниже дна обрабатывает `stack::try_grow` **до** доставки сигнала:

* причина — только load/store page fault (instruction fetch не растёт: страницы
  `U|R|W`, не X, и повторный fetch зациклился бы);
* адрес не ниже `USER_STACK_FLOOR = USER_TOP - USER_STACK_MAX_SIZE`, где
  `USER_STACK_MAX_SIZE = 1 MiB` — потолок на процесс: 16 начальных кадров +
  `MAX_GROWTH_PAGES = 240` страниц роста (960 KiB);
* адрес не дальше `FAULT_SLACK = 64 KiB` ниже прерванного `sp` — дикий указатель
  далеко под стеком не «доращивает» его на всё окно;
* ни одна страница диапазона `page..stack_low` не замаплена: загрузчик вправе
  положить `PT_LOAD` в это окно (ему разрешено всё ниже `USER_STACK_BASE`), и
  такая страница сохраняется, а fault остаётся фатальным;
* PMM выдаёт кадры — иначе рост останавливается, уже замапленное остаётся
  учтённым в `stack_low`, а fault отклоняется.

Принятый fault: кадры обнуляются через identity-карту, мапятся
`Flags::user_rw()`, `stack_low` сдвигается вниз, `sepc` не меняется — эпилог
повторяет faulting-инструкцию. Ничего не печатается; счётчики `stack::stats()`
(`growths`, `pages`, `refused`, `refused_frames`) читает selftest.

Отказ — это обычный `on_fault`: рекурсия сверх 1 MiB упирается в страницу ниже
`USER_STACK_FLOOR`, которая не мапится никогда, и прогон умирает
`[user] <name> killed: store page fault`. То есть guard не «почти» есть, а есть:
он стоит на странице ниже потолка, а не на 64 KiB.

Учёт и teardown:

* `fork` копирует `stack_low..USER_STACK_BASE` постранично
  (`copy_grown_stack`) в нового ребёнка — он наследует ровно тот стек, что был
  у родителя;
* `execve` собирает новый стек и ставит `stack_low = USER_STACK_BASE`;
* `release_process_memory` (раннер, `wait4`, `cleanup_orphans`, `execve`) зовёт
  `stack::release(root, stack_low)` **до** освобождения таблиц: каждая
  замапленная страница диапазона снимается и возвращается в PMM, немапленные
  пропускаются. Инвариант «в `stack_low..USER_STACK_BASE` лежат только страницы
  роста» держится проверкой всего диапазона до первой аллокации.

## Таблица файловых дескрипторов

По одной таблице на процесс (`FdTables { rows: [FdTable; MAX_PROCESSES] }`),
16 слотов каждая; `run_source` сбрасывает новую строку перед спавном,
`wait4`/раннер освобождает её после последнего потока (включая fault).
`fork` копирует строку: файлы — `Arc`+offset, pipe — та же ring с +1 ссылкой
на конец, любой сокет — `FdEntry::Unsupported` (TCP-состояние, listener и
UDP-порт остаются у родителя; чтение/запись/`poll` в ребёнке — `-ENOTSUP`).
`execve` строку не
трогает. Offset файла **копируется**, а не делится между процессами — это
документированное отличие от POSIX open-file-description: две записи после fork
двигают каждая свой offset.

| fd | что это |
|---|---|
| 0 | консольный ввод: `uart::try_get_byte()` → `virtio_input::try_key()`, иначе `-EAGAIN` |
| 1, 2 | консольный вывод (один UART/экран) |
| 3..15 | `Arc<dyn VfsNode>` + offset + flags `openat`; каталог хранит снимок детей |
| 3..15 | один слот может быть `FdEntry::Socket { id, connected }`: TCP-сокет, `read`/`write`/`close` маршрутизируются в `net::tcp` |
| 3..15 | `FdEntry::Tls { session, tcp }`: тот же fd после `tls_connect` (1100). `read`/`write`/`send`/`recv`/`poll`/`shutdown` работают с **открытым текстом**, запись/чтение шифрует `net::tls`; `close` освобождает сессию без `close_notify`, `tls_close` (1103) — с ним |

`close` освобождает узел и отпускает pipe-ссылку; закрыть 0/1/2 можно, и до
конца процесса это `-EBADF`. Лок таблицы **не держится** через VFS/block: `Arc`
и offset копируются, I/O идёт без лока, новый offset публикуется после
(`O_APPEND` пересчитывает его от `node.size()`). Это важно, потому что блочный
драйвер опрашивает устройство и может уйти в `wfi`. `release_fds` собирает
pipe-ссылки и признак сокета под локом, а отпускает их/закрывает TCP уже без
него (закрытие сокета шлёт FIN и может опрашивать NIC).

### TLS-дескрипторы (`tls_connect` 1100 — `tls_close` 1103)

`tls_connect(tcp_fd, host, host_len, flags)` берёт **уже подключённый** потоковый
сокет, выполняет на нём рукопожатие TLS 1.3 через тот же `net::tls`, что и
shell'овый `https` (цепочка до закоммиченного CA-бандла, SAN по `host`, clock
gate, `CertificateVerify` + счётчик `VERIFIED_HANDSHAKES` как вторая проверка), и
заменяет запись fd на `FdEntry::Tls { session, tcp }`. Разрешение имени и
`connect` — дело программы: в ABI нет ни DNS, ни `getaddrinfo` (в libpagh есть
`dns::resolve` поверх UDP). `flags` обязан быть 0 — **нет флага, отключающего
проверку**, и это записано в контракте, а не только в коде.

Сессия живёт в `net::tls` (`MAX_USER_SESSIONS = 4`, у каждой два record-буфера
по 16640 B и окно открытого текста 16 KiB на куче, ~50 KiB). Блокирующая
операция **вынимает** сессию из таблицы (спинлок нельзя держать через
`block_on`, который спит на тике) и возвращает обратно; слот при этом остаётся
занятым, поэтому вторая одновременная операция над той же сессией получает
`-EAGAIN`, а не второй читатель одного потока записей. `fork`/`dup` сессию не
копируют — в копии `FdEntry::Unsupported` (`-ENOTSUP`).

Провалившееся рукопожатие закрывает соединение (RFC 8446 §6.1): сокет
освобождается, fd остаётся «надгробием» (`-ENOTSUP` на I/O, `close` освобождает).
`tls_read` при `close_notify` возвращает 0, а при обрыве TCP **без** него —
`-ECONNRESET`: обрезанный поток не должен выглядеть как дочитанный.

Энтропия для userland — `getrandom(278)`: тот же пул `src/entropy.rs`, что и у
TLS, до 64 KiB за вызов (больше — короткое чтение), `flags != 0` → `-EINVAL`,
непросеянный пул → `-EAGAIN` без единого записанного байта.

## User heap (`brk`)

Окно ровно 1 MiB: `0x2000_0000..0x2010_0000` — выше максимального образа
(1 MiB от `0x1_0000`), ниже user-стека (`0x3fff_0000`) и вне окон устройств
(PLIC `0x0c00_0000`, UART/virtio `0x1000_0000`). `brk(0)` возвращает текущий
break; любое возвращённое значение выровнено вверх на страницу (bump-аллокатор
libpagh использует его как новый предел); рост мапит кадры PMM постранично (`Flags::user_rw`, кадры обнуляются),
сжатие снимает маппинг и сразу возвращает кадры; за `USER_HEAP_TOP` — `-ENOMEM`,
частичный рост откатывается. Список кадров лежит в `process::HEAPS` (отдельно от
`Process`, чтобы fork/exec не таскали 2 KiB через kernel-стек); раннер,
`wait4` или `execve` вызывает `release_heap` до освобождения root, поэтому
кадры возвращаются в PMM до выхода из `run_source`, в том числе после fault.

Окно не вытесняет образ: если хитрый ELF заранее замапил страницу в этом
диапазоне, `paging::map` вернёт `AlreadyMapped`, `brk` — `-ENOMEM`, и уже
замапленные этой попыткой страницы откатятся.

## Syscall'ы и указатели

Номера и семантика — `pagh-api/docs/abi.md`; отличия, о которых должен знать
userland, перечислены в конце. Проверка указателей — `copy_from_user` /
`copy_to_user`: каждая страница диапазона обязана иметь `U=1` и `R=1` (плюс
`W=1` для записи), `SUM` включается ровно вокруг копии и восстанавливается из
точного значения `sstatus`. Поэтому запись в `.rodata` — `-EFAULT`, а не
S-mode page fault (тот напечатал бы и остановил hart). Неизвестный номер —
`-ENOSYS` со счётчиком; счётчики успешных вызовов (`reads`, `opens`, `brks`,
`clocks`, …) читает selftest.

Особые случаи:

* `read(0)` **никогда не блокирует**: нет байта — `-EAGAIN`, libpagh повторяет;
  шелл в это время не опрашивает ввод.
* `nanosleep` — единственный syscall, который включает прерывания: `sleep_ticks`
  ждёт тики, а в trap-контексте `SIE = 0`, поэтому тики не шли бы. Локов на это
  время не удерживается, вложенный trap-кадр живёт на стеке задачи.
* `getdents64` отдаёт снимок каталога, сделанный при `openat`; курсор двигается
  только за целиком скопированные записи (12-байтный заголовок + имя, padding
  до 8). Если в буфер не влезает даже одна запись, возвращается 0 — с точки
  зрения вызывающего это неотличимо от конца каталога; 264 байта покрывают
  максимальное имя ext2.
* `openat`/`mkdirat` принимают `mode`, но не применяют: в `VfsNode` нет
  `set_mode`, ext2 создаёт файл с 0o644, каталог — с 0o755. Это
  задокументированное ограничение, а не забытый код.
* `fchmodat` — компромисс: `mode & 0o7777` кладётся в статическую таблицу
  прогона (ключ — `fs_ino`), `fstat` отдаёт эти биты до конца программы; inode
  на диске не меняется, права ни на что не влияют. Синтетический узел (`ino 0`)
  — `-ENOTSUP`; семнадцатый chmod в прогоне — `-ENOSPC`.
* Мутации путей (`mkdirat`/`unlinkat`/`symlinkat`/`linkat`/`renameat2`) —
  `read_user_path` + `read_path` (UTF-8, абсолютный) → `parent_and_leaf` →
  `walk(parent, true)` → один вызов VFS. `unlinkat`/`linkat` не разыменовывают
  последнюю ссылку, `fchmodat` разыменовывает; неизвестный бит `flags` —
  `-EINVAL`. `openat(O_CREAT)` при нехватке fd удаляет созданный файл, а
  `O_TRUNC` выполняет после установки fd и откатывает её при ошибке.
* `clock_gettime` и `nanosleep` работают от 100 Гц тика: разрешение 10 ms,
  `nsec` всегда кратен миллиону.
* **Сокеты — TCP-клиент, TCP-сервер и UDP.** `socket` принимает `AF_INET` + 0;
  `SOCK_STREAM` — один активный TCP-клиент (второй активный — `-ENOSPC`; listener
  и принятые соединения — отдельные виды дескриптора), `SOCK_DGRAM` — UDP-сокет
  (можно несколько); `SOCK_NONBLOCK` — бит типа (UDP `recvfrom`, stream
  `accept`). `connect` валидирует 16-байтный `sockaddr_in` через
  `copy_from_user` (length, family, port BE) и внутри `net::tcp::connect` крутит
  `net::poll()` до handshake; на UDP — `-ENOTSUP`. TCP `send` синхронный (ждёт
  ACK), `recv` **неблокирующий** (`-EAGAIN` — повтор, 0 — EOF). TCP-сервер:
  `bind(200)` резервирует локальный порт (`0.0.0.0`/lease, 0 — эфемерный),
  `listen(201)` зажимает backlog в `1..=4`, `accept(202)` пишет peer-адрес в
  `sockaddr_in` и возвращает `FdEntry::TcpConn`; очередь на 4, восемь слотов
  соединений (клиент + принятые), по 4 KiB приёма. UDP: `bind(200)` даёт
  локальный порт, `sendto` требует адрес и payload ≤ 1024 B (иначе
  `-EMSGSIZE`), `recvfrom` без `MSG_DONTWAIT` блокируется на опросе NIC с
  бюджетом ~10 s и заполняет `src`; per-port очередь — 4 дейтаграммы,
  старая вытесняется. `connect`/`send`/`shutdown`/`close`/`accept`/блокирующий
  `recvfrom` на время ожидания включают прерывания — иначе в trap-контексте
  тики стоят, дедлайн не работает и планировщик не может вытеснить застрявшую
  программу; `net::poll()` при этом вызывается на каждом шаге, потому что шелл
  в это время не в idle-цикле. `read`/`write`/`close`/`poll` на socket fd идут
  теми же путями (`read` на UDP — `recv` без source, `write` — `-EDESTADDRREQ`).
  `shutdown` закрывает TCP оба направления, но оставляет fd (последующий
  `read` → 0, `write` → `-EPIPE`); `close` освобождает слот, UDP — ещё и порт.
  UDP-`connect`, IPv6, DNS-вызов и TLS отсутствуют. Программа, вышедшая с
  открытым сокетом, закрывает его (и освобождает UDP-порт/bind-резерв) в
  `release_fds` при teardown прогона; `fork`/`dup` дают ребёнку/копии
  `-ENOTSUP` (второй владелец соединения/listener'а не создаётся).

## Процессы, потоки и фьютексы

Процесс — это один Sv39-root (ASID), один `Process` в статической таблице и
группа потоков с `group == pid`. Главный поток создаёт `run_source`; остальные —
`clone(flags, stack, entry, arg)` (220); `fork` (1000) создаёт **новый процесс**
с копией user-страниц, fd и обработчиков сигналов, а `execve` заменяет образ,
стек, heap и mmap существующего процесса.
`CLONE_VM|CLONE_THREAD` обязательны, `CLONE_FS/FILES/SIGHAND` принимаются (всё
общее и так), неизвестный бит — `-EINVAL`, `CLONE_SETTLS` — `-ENOTSUP`.
`stack` проверяется: 16-выровнен и 16 байт под ним лежат в user-странице
`U|R|W`; `entry` — user-страница `U|R|X` с 2-байтным выравниванием (RVC).
Родитель получает tid, ребёнок начинает U-исполнение на `entry(arg)`; из syscall
он не возвращается (классическое «в ребёнке 0» относится к libc-обёртке, не к
явному `entry`). `entry` тоже не должен возвращаться: трамплина в U-mode нет
(`ra = 0` — fault и конец прогона); поток завершается `exit`/`exit_group`.
`gp`/`tp` наследуются, TLS нет.

**Что общее у потоков процесса:** свой root/ASID и user-карта, fd-таблица,
heap `brk`, mmap-регионы, таблица `fchmodat`, таблица `sigaction`, код и данные
образа, pid (`getpid`), ppid, код выхода процесса (код главного). **Что своё:**
tid, запись в task-таблице, kernel-стек 24 KiB (96 KiB в debug) + 4 KiB guard из PMM,
сохранённый `TaskContext`/trap frame, user-регистры, blocked/pending/alt-стек;
user-стек передаёт вызывающий, ядро его не выделяет и не освобождает. Каждый
поток несёт в `Task::satp` root своего процесса; `scheduler::install_satp`
переключает `satp` перед `task_switch` и делает полный `sfence.vma` при смене
root (ASID — метка TLB, но не разрешение не flush'ить).

**Вход нового потока** — не второй layout: `spawn_user_thread` кладёт на верх
kernel-стека обычный `TrapFrame` (`sp=stack`, `a0=arg`, `sepc=entry`,
`sstatus` как у `enter_user`) и строит контекст `{ ra = trap_return, sp =
frame_base }`. `trap_return` — тот же эпилог, что у прерванного U-потока:
восстанавливает GPR/`sepc`/`sstatus`, берёт user `sp` из кадра, переarm'ит
`sscratch`, `sret`. Второго `sret`/layout'а нет.

**Жизненный цикл.** `exit` (93) завершает только вызывающий поток; код главного
запоминается в `main_code`, код не-главного отбрасывается. Последний поток
ставит `Exit::Code(main_code.unwrap_or(код последнего))`, и `run_source`
возвращается. `exit_group` (94) делает `kill_group`: `Ready`/`Blocked`/`Stopped` →
`Exited` (очередь чистится, стеки забирает `reap`), `threads = 0`, код —
главного или вызывающего. Fault любого потока убивает весь процесс
(`Exit::Fault`), а не одну задачу: иначе упавший с захваченным userspace-локом
поток оставил бы остальных в вечном wait. `Process` не снимается, пока
`threads != 0`; раннер/`wait4` освобождает fd, mmap, heap и futex-очередь только
после `threads == 0` и `reap`.

**Futex** (98) — только `WAIT`/`WAKE`. `FUTEX_WAIT` сверяет `*uaddr` с `val`
через `copy_from_user`; не равно — `-EAGAIN`; иначе вставка в статическую
таблицу, `block_current(deadline)` и снятие. `FUTEX_WAKE` будит не больше `val`
через `scheduler::wake_task`. `FUTEX_PRIVATE_FLAG` принимается (и ничего не
меняет: очередь и так на процесс), `WAKE_OP`/`REQUEUE`/PI — `-ENOSYS`. `uaddr`
4-выровнен. Таймаут — `Timespec`, округляется вверх до тика; без аппаратного
таймера timed wait сразу возвращает `-ETIMEDOUT` (иначе дедлайн некому
проверять). Возможные ответы: 0, `-EAGAIN`, `-ETIMEDOUT`, `-ENOMEM` (17-й
адрес), `-EINVAL` (адрес/op/время), `-EFAULT` (указатель). Таблица: **32
ожидающих на 16 адресов**. Ключ — `(pid, uaddr)`, pid монотонный, поэтому запись
убитого процесса не разбудит следующий; `clear_run` чистит её на teardown,
`execve` — тоже (образ новый, старые ожидания не должны дожить).

**Lock order:** futex-очередь → планировщик. Очередь не удерживается через
`block_current` (вставка/снятие — короткие критические секции с замаскированными
прерываниями), `wake_task` только ставит поток в ready-queue. Syscall trap
приходит с `SIE = 0`, поэтому между проверкой слова, вставкой и блокировкой
другой поток не может вклиниться; тик сканирует только `wake_at` под
scheduler-локом и futex-лок не берёт.

**Тесты.** `userland/abi.rs` (секция threads): `gettid`/`set_tid_address`,
матрица ошибок `clone`, два потока с общим atomic-счётчиком, futex
wait/wake-rendezvous, `exit` не-главного, `exit_group` с заблокированным
выжившим; без `/mnt` — «главный вышел первым», tail-поток переживает его, run
возвращает код главного. Кернел проверяет рост `clones`/`futex_waits`/
`futex_wakes`/`exit_groups` и `futex::live_waiters() == 0`. Маркер —
`abi: threads ok`.

## Процессы: `fork`, `execve`, `wait4`

Номера: `fork = 1000` (pagh-native, в asm-generic его нет; `clone(0,0,0,0)` —
алиас), `execve = 221`, `wait4 = 260`, `getppid = 173`. `fork` возвращает pid
ребёнка родителю и `0` ребёнку, обе стороны продолжают с `sepc + 4`; при ошибке
— `-EAGAIN` (нет слота/кадров/памяти) или `-EINVAL`.

**`fork` — глубокая копия, не COW.** `paging::create_user_space` даёт ребёнку
свой root и ASID, затем копируются все mapped user-страницы: image (`Loaded`),
64 KiB начального стека, выросшие страницы стека (`snapshot.stack_low ..
USER_STACK_BASE`), `brk`-heap, `mmap`-регионы. Копия идёт по parent root через
`paging::translate_in`; флаги страницы (включая X) сохраняются. Ребёнок получает
копию fd-строки и обработчиков сигналов, но не pending и не чужие потоки.
Родитель и ребёнок после возврата — два независимых процесса; общий у них
только физический pipe-ring (через refcount). Копирование идёт с выключенными
прерываниями в trap-контексте, поэтому ребёнок не может исполниться, пока
`fork` не опубликовал его `Process` и `fork_process` не создал signal-строку.

**`execve(path, argv, envp)`** абсолютного пути: читает ELF в **новый** root,
собирает новый стек и SysV-кадр, затем атомарно подменяет образ. Старые image,
heap, mmap и user-стек (включая выросшие страницы) освобождаются, `stack_low`
ставится на `USER_STACK_BASE`, `root`/`satp`/`asid` заменяются, но
`pid`, `ppid`, fd-таблица и `blocked`-маска сохраняются. Все обработчики
сбрасываются в `SIG_DFL`, alt-стек выключается, pending гасится; sibling-потоки
ретируются (`kill_group`), `threads = 1`, `wait_tid = 0`. На успехе
`dispatch` **не** пишет `a0` и не двигает `sepc`: `TrapFrame` уже указывает на
`entry` с `a0 = argc`, `a1 = argv`. Любая ошибка (`-ENOENT`, `-ENOEXEC`,
`-EISDIR`, `-E2BIG`, `-EFAULT`, `-ENOMEM`) не меняет старый образ. Пределы
образа те же, что у `run`: ≤ 1 MiB, статический `ET_EXEC`/`ET_DYN` без
интерпретатора.

**`wait4(pid, *status, options, rusage)`** принимает `options & !WNOHANG == 0`,
`pid > 0` — конкретный ребёнок, `0`/`-1` — любой, `pid < -1` — `-EINVAL`.
Возвращает pid зомби, `0` при `WNOHANG`, если ребёнок жив, `-ECHILD` при
отсутствии детей, `-EINVAL` на плохие аргументы, `-EFAULT` на плохой `status`.
Статус — POSIX-подобный: `code << 8` для `Exit::Code`, номер сигнала в младших
7 битах для смерти от сигнала/fault (core-бита нет). Ожидание блокирует
вызывающий поток (`block_current`) и просыпается `notify_parent` из
`exit`/`exit_group`/`on_signal_death`/`on_fault`; `SIGCHLD` при этом **не**
генерируется и `siginfo.si_status` остаётся 0 — это документированный разрыв,
а не «wait через сигнал».

**Сироты.** Init-процесса нет: если foreground-процесс вышел, не дождавшись
детей (или родитель ребёнка уже reaped), раннер `run_source` вызывает
`cleanup_orphans` — `kill_group`, `force_finish`, `reap` и освобождение
fd/сигналов/страниц для каждого оставшегося `Process`. Поэтому `run` не
возвращает приглашение, пока таблица процессов и PMM-ledger не пусты. Это
отличается от POSIX (там сироту усыновляет init), но является единственным
честным поведением без init.

**ASID и TLB.** ASID 0 — ядро; процесс получает монотонный ненулевой 16-битный
тег из `paging::next_asid` (после 65535 — оборот и полный `sfence.vma`).
Несмотря на ASID, `mmu::activate_satp` всегда делает `sfence_vma_all` при смене
root: тег — пространство имён TLB, а не обещание, что старая трансляция не
переживёт переиспользование кадра таблицы. Один процесс — один root/ASID,
таблица `MAX_PROCESSES = 6`; шесть root'ов стоят примерно 3 MiB page-table
кадров на 256 MiB конфигурации, седьмой параллельный `fork` — `-EAGAIN`.

## Сигналы (POSIX-подобные)

`src/user/signal.rs`. Это нативный ABI, не Linux: `struct sigaction` — это
`{ handler, flags, mask, restorer }` (32 B, порядок pagh), `struct sigaltstack`
— `{ ss_sp, ss_flags, pad, ss_size }` (24 B), маска — 8 B, сигнал `n` — бит
`n-1`, `sigsetsize = 8`.

* **Где что живёт.** Обработчики и `SIG_IGN` — в таблице процесса
  (`ProcessSignals`, строка на слот; `begin_process` при старте,
  `clear_process` после последнего потока). `pending`-биты, blocked-маска,
  alt-стек, глубина `alt_active`, restart-флаг и указатель на активный кадр — у
  каждого потока. С каждым pending-битом хранятся `si_pid`/`si_uid`/`si_code`
  отправителя, потому что доставка отложена до границы kernel→user.
  `kill(pid > 0)` с `pid == getpid()` адресует главный поток процесса (или
  первый живой, если главный вышел), другой положительный pid обязан быть tid
  этого же процесса; `kill(0)`/`kill(-1)` — все живые потоки (включая
  остановленные). `tgkill` требует `tgid == getpid()`. Межпроцессный `kill`
  не реализован (нет модели прав/process group): чужой pid — `-ESRCH`.
  Неизвестный tid — `-ESRCH`, signum вне 0..=31 — `-EINVAL`, `sig = 0` —
  проверка существования.
* **Наследование.** `fork` копирует действия обработчиков, blocked-маску и
  altstack-настройки вызывающего потока; pending-биты и активный кадр не
  копируются (`ThreadSignal::pending = 0`, `frame = 0`, `alt_active = 0`).
  `execve` ставит **все** действия в `SIG_DFL` (включая бывший `SIG_IGN` — это
  документированное отличие от POSIX), выключает alt-стек, гасит pending и
  кадры, но **сохраняет** blocked-маску. `fork`/`exec` меняют только таблицу
  процесса: поток-инициатор продолжает с тем же tid.
* **Доставка.** Пометка pending; если `pending & !blocked` непусто, ядро зовёт
  `scheduler::wake_task` (futex-wait просыпается и вернёт `-EINTR`). Обработчик
  ставится только на границе kernel→user: после syscall'а или после любого
  trap'а из U-mode, включая тик. Один сигнал на границу; вложенность идёт через
  `prev_frame` в кадре. `nanosleep` проверяет pending раз в тик и тоже
  возвращает `-EINTR`. Заблокированный сигнал остаётся pending.
* **Кадр (736 B, ниже `sp` или верха alt-стека, 16-выровнен):** `TrapFrame`
  (264), blocked-маска (8), magic (8), restorer (8), `siginfo` 128 B,
  `prev_frame` (8), signum (8), `ucontext` 288 B, frame flags (8: бит 0 —
  переключились на alt-стек), reserved (8). Каждая страница проверяется на
  `U|R|W` до `copy_to_user`; плохой `sp` — `on_signal_death("bad signal
  stack")`, размапленный alt-стек — `"bad alternate signal stack"`, не kernel
  fault. Обработчик входит с `a0 = signum`, `a1 = &siginfo`,
  `a2 = &ucontext` (только при `SA_SIGINFO`, иначе 0), `ra = restorer`,
  `sp = frame`; `ret` из обработчика попадает в restorer, тот делает
  `ecall 139`.
* **`siginfo_t` (128 B).** 0 — signo, 4 — errno (0), 8 — code (`SI_USER=0`,
  `SI_KERNEL=0x80`, `SI_TKILL=-6`, `SEGV_MAPERR=1`, `BUS_ADRALN=1`,
  `TRAP_BRKPT=1`), 16..24 — `si_pid`+`si_uid` для `kill`/`tgkill` либо
  `si_addr` для fault, 24..28 — `si_status` (child-exit SIGCHLD; без
  `fork`/`wait` явный `kill(SIGCHLD)` даёт `SI_USER` и 0), дальше padding.
* **`ucontext_t` (288 B, pagh).** 0 — flags (0), 8 — link на предыдущий
  `ucontext`, 16 — `uc_sigmask` (маска внутри обработчика), 24 — `mcontext`:
  `gregs[32]` (x0..x31, x2 = прерванный sp) + `pc` (прерванный sepc). Это
  read-only снимок: `rt_sigreturn` восстанавливает `TrapFrame`, а не правки
  user-памяти.
* **`rt_sigreturn`.** Проверяет `U|R`-диапазон (736 B), magic, signum, frame
  flags и что `sp` равен активному кадру; иначе `-EFAULT` (плохой указатель)
  или `-EINVAL` (нет кадра / чужой кадр). Восстанавливает `TrapFrame`,
  blocked-маску, `prev_frame` и глубину alt-стека, затем `sstatus` собирается
  заново (`SPP=0`, `SIE=0`, `SUM=0`, `SPIE=1`) — кадр из user-памяти не может
  запросить S-mode. `sepc` не сдвигается.
* **`sigaltstack`.** `SS_DISABLE=2` выключает, `SS_ONSTACK=1` репортится, пока
  обработчик на стеке; `ss_size >= MINSIGSTKSZ = 4096`, весь диапазон `U|R|W`,
  иначе `-EINVAL`/`-EFAULT`. Стек per-thread; `SA_ONSTACK` без установленного
  стека — fallback на обычный (POSIX разрешает). Вложенный сигнал на уже
  активном alt-стеке пишется ниже текущего `sp`, а не на тот же верх.
* **Флаги.** `SA_RESTORER=0x04000000` обязателен для функции; допустимы
  `SA_SIGINFO=4`, `SA_ONSTACK=0x08000000`, `SA_RESTART=0x10000000`,
  `SA_NODEFER=0x40000000`, `SA_RESETHAND=0x80000000`, остальные биты —
  `-EINVAL`. `SA_NODEFER` не блокирует доставленный сигнал, `SA_RESETHAND`
  сбрасывает disposition в `SIG_DFL` при входе. `SA_RESTART` реализован только
  для `futex`/`nanosleep`: `dispatch` не пишет `-EINTR` в `a0` и не двигает
  `sepc`, поэтому после `rt_sigreturn` тот же `ecall` повторяется с исходными
  аргументами; таймаут при этом полный, а не остаток.
* **Default actions.** `SIG_DFL` для фатальных сигналов (SIGTERM/SIGINT/SIGQUIT/
  SIGABRT/SIGSEGV/SIGILL/SIGFPE/SIGBUS/SIGKILL/…) завершает **прогон** через
  `on_signal_death` (`kill_group`, `threads = 0`, `Exit::Fault`);
  SIGCHLD/SIGCONT/SIGURG/SIGWINCH игнорируются. SIGSTOP/SIGTSTP/SIGTTIN/
  SIGTTOU с `SIG_DFL` вызывают `scheduler::stop_current`: поток уходит в
  `State::Stopped`, не планируется, сохраняет стек и signal-состояние; SIGCONT
  чистит pending stop и делает `Stopped -> Ready`. `kill(0)`/`kill(-1)`
  останавливает все потоки процесса — tty/process-group нет, возобновить
  извне некому (документированный предел). SIGKILL/SIGSTOP нельзя поймать
  (`rt_sigaction` — `-EINVAL`) или заблокировать. Синхронные SIGILL/SIGTRAP/
  SIGBUS/SIGSEGV доставляются установленному обработчику вместо `on_fault`
  (`si_addr = stval`); без обработчика поведение прежнее. `SIGFPE` аппаратного
  источника не имеет, `sigqueue`/`sival_ptr`/очередей сигналов нет.

## Раскладка адресов

Раскладка одна на процесс, но сама карта у каждого своя:

```text
0x0000_0000 … 0x0000_ffff   не отображено (включая 0 — на этом стоит fault-фикстура)
0x0001_0000 … 0x3fff_0000   окно образа: PT_LOAD-страницы с U=1 (≤ 256 страниц)
0x2000_0000 … 0x2010_0000   user heap (brk), 1 MiB, страницы по требованию
0x3000_0000 … 0x3f00_0000   mmap-окно (поиск свободных страниц, не резерв)
0x3ff0_0000                 USER_STACK_FLOOR: страница ниже не мапится никогда
0x3ff0_0000 … 0x3fff_0000   выращенная часть user-стека (≤ 960 KiB = 240 страниц)
0x3fff_0000 … 0x4000_0000   начальный user-стек: 64 KiB = 16 кадров, U=1
0x4000_0000                 stack_top; начальный sp ниже, на SysV-кадре
0x8000_0000 …               RAM ядра: identity-карта, страницы без U
```

`paging::create_user_space` копирует в root процесса **только supervisor**
листья kernel-root'а (RAM identity map, UART, PLIC, virtio-окна) и ни одной
страницы с `U=1`; загрузчик добавляет surface процесса уже в его root. Поэтому
два процесса не видят user-страниц друг друга, хотя оба root'а содержат одну и
ту же kernel-карту с `U=0`. Окно ниже RAM, но численно пересекается с PLIC
(`0x0c00_0000`) и UART (`0x1000_0000`) в карте ядра: ELF, который просит эти
адреса, получает `ElfError::AddressInUse` от page-table walk, а не «почти
работает». Heap-окно лежит между образом и стеком и с устройствами не
пересекается.

## Загрузчик: потоковое чтение

`elf::load` принимает `ImageSource` (`len` + `read_at`) и читает только то, что
описывает ELF: 64-байтный заголовок, таблицу program headers (≤ 8) и каждый
`PT_LOAD` кусками не больше 512 B прямо в уже замапленные user-страницы. Буфера
размером с образ нет, поэтому `run <path>` принимает файлы до 1 MiB
(`MAX_IMAGE_BYTES`) независимо от того, что куча выдаёт чанки по 64 KiB. 512 B,
а не страница, — запас по стеку: `run_file` идёт по 128 KiB boot-стеку шелла, и
4-килобайтный буфер в самой глубокой функции раньше оставлял меньше сотен байт
до статиков; boot-стек увеличен, потому что переполнение уже затирало
`timer::TICK_HANDLER`/`paging::SPACE`. Хвост файла после
последнего `PT_LOAD` не читается и не валидируется — именно это проверяет
`user-run-large-file` на 100 KiB. Короткий источник даёт
`ElfError::Truncated`, отказ чтения — `ElfError::Source`/`UserError::Source`;
все страницы, замапленные до ошибки, откатываются. Встроенные фикстуры идут
через тот же загрузчик с `impl ImageSource for [u8]`.

## Контракты (инварианты)

1. **Один root и один ASID на процесс.** `Process.root/satp/asid` — источник
   истины; `Task.satp` каждого потока указывает на root его процесса, а
   `scheduler::install_satp` переключает `satp` перед `task_switch`. Ядро живёт
   в ASID 0 и в kernel-карте; процесс получает копию supervisor-отображений, а
   user-страницы — только в своём root. `MAX_PROCESSES = 6`; несколько
   процессов могут исполняться одновременно (`fork`), но foreground `run`
   один — шелл ждёт его.
2. **В U-mode ведёт один `sret`-путь.** `enter_user` (главный поток) собирает
   `sstatus` в Rust (`SIE=0, SPP=0, SPIE=1, SUM=0`) и пишет его `csrw sstatus`,
   затем `sscratch = sp`, `sepc = entry`, `a0 = argc`, `a1 = argv`,
   `sp = initial sp`, `sret`. Новый `clone`-поток не строит второй layout: его
   `TaskContext.ra` — экспортированный `traps::trap_return`, а `sp` — база
   `TrapFrame` на его kernel-стеке; эпилог восстанавливает тот же кадр и делает
   `sret`. Биты SPIE (5) и SPP (8) не влезают в 5-битный immediate
   `csrsi`/`csrci` — только `csrw sstatus`.
3. **`sscratch` — часть контракта ловушки.** Пока выполняется S-mode,
   `sscratch == 0`; перед входом в U-mode туда кладётся `sp` ядра. `trap_entry`
   первым делом делает `csrrw sp, sscratch, sp`: ненулевой `sp` = ловушка из
   U-mode (user `sp` сохраняется в слот 8 кадра), ноль = ловушка из S-mode.
   Эпилог различает пути по **сохранённому** `sstatus.SPP` и для U-пути заново
   вооружает `sscratch` (база кадра + 264) до восстановления user `sp`.
4. **Слот 8 кадра имеет двойной смысл:** S-mode — база кадра, U-mode — прерванный
   user `sp`. Ассемблер, `TrapFrame` и `docs/architecture.md` §4.1 меняются одним
   коммитом.
5. **`ecall` возвращается на `sepc + 4`.** `dispatch` обязан сдвинуть `sepc`.
6. **User-указатели не доверяются.** `copy_from_user`/`copy_to_user` проверяют
   каждую страницу по PTE (`U=1`, `R=1`, для записи ещё `W=1`) **до** копии;
   `SUM` включается вокруг `copy_nonoverlapping` и восстанавливается из точного
   прежнего значения. Адрес ядра, дырка или read-only user-страница → `-EFAULT`,
   а не page fault.
7. **Fd-таблица — на процесс, heap — на root.** Строка `FdTables` сбрасывается
   перед спавном, `fork` её копирует, `execve` сохраняет, `wait4`/раннер
   освобождает; лок не держится через VFS/block. `brk`-кадры лежат в
   `process::HEAPS`, принадлежат процессу и возвращаются в PMM до освобождения
   его root (раннером, `wait4` или `execve`).
8. **Fault из U-mode убивает процесс, не машину.** Исключение с сохранённым
   `sstatus.SPP == 0` (кроме `ecall`) уходит в `on_fault`: одна строка
   `[user] <name> killed: …`, запись `Exit::Fault`, `kill_group` для остальных
   потоков и `scheduler::exit`; раннер или `wait4` освобождает страницы,
   kernel-стеки, fd и heap. Исключение из S-mode по-прежнему печатает и паркует
   hart.
9. **Сигнальный кадр — user-память, но контекст — trap frame.** Сохраняется
   ровно тот `TrapFrame`, который собрал `trap_entry`; порядок полей не меняется,
   а `rt_sigreturn` восстанавливает его целиком и **не** делает `sepc + 4`.
   `sstatus` не копируется из кадра (только пересобирается), `sstatus.SPP` всегда
   0 на возврате в U-mode. Кадр — 736 B: `TrapFrame`, маска, magic, restorer,
   `siginfo`, `prev_frame`, signum, 288-байтный `ucontext`, frame flags; менять
   смещения можно только вместе с `docs/architecture.md` и фикстурой. Все
   страницы кадра проверяются `U|R|W` до записи; ошибка — смерть прогона, а не
   S-mode fault.
10. **Сигнальный лок — лист.** `process → signal table` (pid процесса читается
    до лока); `kill` резолвит цели под scheduler-локом и отпускает его до
    signal-лока; `wake_task`/`continue_task` зовутся без signal-лока;
    `copy_to_user`/`copy_from_user` — без лока. Все взятия маскируют прерывания:
    `interrupted()` вызывается из `nanosleep` с `SIE = 1`. Pending и blocked —
    разные битовые карты: доставка только `pending & !blocked`, снятие pending —
    в момент доставки.
11. **`Process` не снимается через блокирующее ожидание.** Таблица берётся на
    короткие обновления; `wait_for_process` читает `threads` и крутит
    `yield_now` без неё; teardown делает раннер или `wait4` (trap-пути только
    читают/пишут запись). Запись освобождается только когда `threads == 0`:
    `clone`-ребёнок ещё исполняется в этом адресном пространстве.
11. **Сокетный syscall сам опрашивает NIC.** Пока user-программа работает,
    шелл стоит в `wait_for_run_end` и `net::poll()` не вызывает; поэтому
    `connect`/`send`/`shutdown`/`close` крутят опрос внутри `net::tcp` и на
    время ожидания включают прерывания (как `nanosleep`), не держа локов.
    Соединение одно, `recv` неблокирующий, sockaddr валидируется
    `copy_from_user` до изменения состояния; второй `socket()` — `-ENOSPC`.
12. **Alt-стек и job control — состояние потока, но переключает планировщик.**
    `sigaltstack` валидирует весь диапазон `U|R|W` и хранит его в
    `ThreadSignal`; доставка с `SA_ONSTACK` выбирает верх стека и пишет кадр
    туда, а frame flags/`alt_active` помнят, что `rt_sigreturn` должен снять
    уровень. Остановка (`SIGSTOP`/`SIGTSTP`/`SIGTTIN`/`SIGTTOU` + `SIG_DFL`)
    делается только из контекста самой останавливаемой задачи в
    `default_action` (`scheduler::stop_current`), а не из `kill`: sender не
    трогает чужое `TaskContext`. SIGCONT — `continue_task` плюс очистка pending
    stop; `wake_task` не должен будить `Stopped`-поток.

## Пределы (честно)

- user-стек: 64 KiB начального окна + деманд-рост до **1 MiB на процесс**
  (`USER_STACK_MAX_SIZE`, 240 страниц роста, дно `USER_STACK_FLOOR =
  0x3ff0_0000`); ниже дна — guard, поэтому рекурсия сверх лимита умирает
  `store page fault`, а не растёт дальше. Растёт только стек: `brk`/`mmap`
  по-прежнему выделяются сразу, COW нет;
- образ ≤ 1 MiB (`MAX_IMAGE_BYTES`) / 256 страниц / 8 `PT_LOAD` / 8 program
  headers (`elf.rs`); больше — `ElfError::ImageTooLarge`, отказ без маппинга;
- `run <path>` читает файл **потоково** через `ImageSource` кусками ≤ 512 B;
  файл в 100 KiB и образ до 1 MiB грузятся без heap-буфера размером с файл;
  нулевой хвост после последнего `PT_LOAD` игнорируется;
- 16 fd на **процесс** (кончились — `-ENOSPC`), путь ≤ 256 B, кадр
  `argv/env` ≤ 64 строк и ≤ 16 KiB, heap ровно 1 MiB; `fork` копирует fd-строку,
  `execve` сохраняет её;
- `mode` в `openat`/`mkdirat` игнорируется (нет `set_mode` в VFS);
  `fchmodat`-режим не переживает прогон; `getdents64` при слишком маленьком
  буфере возвращает 0; `lseek` на каталоге — `-EINVAL`;
  `clock_gettime`/`nanosleep` имеют разрешение тика 10 ms;
- одновременно живых процессов ≤ 6 (`MAX_PROCESSES`), у каждого свой Sv39
  root и 16-битный ASID; `fork` — **глубокая копия** всех user-страниц, не COW:
  память и время растут линейно, зато нет write-fault пути; седьмой
  параллельный процесс — `-EAGAIN`;
- `fork` не копирует pending-сигналы и чужие потоки; `execve` сбрасывает все
  обработчики в `SIG_DFL` (даже `SIG_IGN`), выключает alt-стек и сохраняет
  blocked-маску; fd/pipe наследуются, сокет в ребёнке — `-ENOTSUP`, offset
  файла копируется, а не делится; `SIGCHLD` не генерируется, `wait4` будит
  родителя напрямую; сирот убивает и хоронит раннер foreground-прогона (init
  отсутствует); нет блокирующего `read(0)`;
- сигналы — 1..=31, один pending-бит на сигнал, один кадр 736 B на активный
  обработчик; есть `sigaltstack`/`SA_ONSTACK`, `SA_SIGINFO` с полным
  `siginfo_t`/`ucontext_t`, `SA_NODEFER`/`SA_RESETHAND`, `SA_RESTART` только для
  `futex`/`nanosleep` (полный таймаут заново); SIGKILL/SIGSTOP не ловятся и не
  блокируются; job control — только per-thread `Stopped`/SIGCONT, без tty и
  process-group; `sigqueue`/`sival_ptr`/очередей и генератора SIGFPE нет;
  доставка асинхронная — не позже тика, futex-wait прерывается,
  `connect`/`send` сигналом не прерываются;
- потоков ≤ 14 на процесс (16 слотов планировщика − `shell`/`idle`), у каждого
  kernel-стек 16 KiB + guard; TLS (`CLONE_SETTLS` — `-ENOTSUP`), PI/requeue
  фьютексы, `join`/`waitid` и приоритеты отсутствуют; futex-таблица 32/16;
  `exit_group` снимает `Ready` и `Blocked`, но не может прервать поток, который
  в этот момент исполняется (на одном hart его нет — это текущий syscall);
- TLS-сессий одновременно ≤ 4 (`MAX_USER_SESSIONS`), каждая ~50 KiB кучи до
  `tls_close`/`close`; повторный `tls_connect` на уже TLS-дескрипторе —
  `-EISCONN`, параллельный вызов из другого потока — `-EAGAIN`; своего
  TLS-*сервера* нет (только клиент), а `tls_get`-обёртки нет — программа сама
  решает, что слать;
- один **активный** TCP-клиент, один listener и таблица из восьми соединений
  (`AF_INET`/`SOCK_STREAM`, IPv4-литерал); backlog ≤ 4, принятые соединения —
  отдельные `FdEntry::TcpConn`; UDP — до 8 bind, 4 дейтаграммы на порт,
  payload ≤ 1024 B, один интерфейс; без UDP-`connect`, IPv6 и DNS (DNS — в
  libpagh поверх UDP). TLS есть, но только как клиент на готовом TCP-сокете.
  TCP `recv` неблокирующий, `accept` без `SOCK_NONBLOCK` ждёт до 30 s; UDP
  `recvfrom` блокируется (бюджет ~10 s) или `-EAGAIN` с `MSG_DONTWAIT`;
  `shutdown` без half-close; ребёнок после `fork`/`dup` видит сокет как
  `-ENOTSUP`.

## Как проверяется

```text
run demo         # hello: write + sched_yield + exit 42
run demo a b     # то же: у фикстуры argc = 1, лишние слова игнорируются
run fault        # getpid, затем load с адреса 0 -> killed: load page fault; машина жива
run spin         # 0.2 s в U-mode (~20 timer interrupts из U), getpid, exit 7
selftest         # SELFTEST user-*: 15/15 с /mnt (13/15 + 2 SKIP без диска), ledger одинаковый
                 # user-tls-abi: getrandom(278) + fail-closed матрица 1100..1103
                 # user-proc-fs: userland/proc.elf читает /proc через openat/read/getdents64,
                 #   включая /proc/self (свой pid), живой forked peer по pid, /proc/kthreads/0
                 #   (shell) и reaped child -> -ENOENT
                 #   (маркеры "tls: getrandom ok" / "tls: abi ok" / "tls: ok")
                 # user-stack-grows: 160 KiB цепочка -> "stack: deep recursion ok"
                 # user-stack-cap:   1.2 MiB цепочка -> killed: store page fault (stval 0x3fef_ffc8)
                 # user-stack-fork:  fork на выросшем стеке -> "stack: fork child/parent ok"
```

`user-abi-argv` запускает `userland/argv.elf` с тремя аргументами и `PWD=/`,
перехватывает вывод syscall'а `write` и сравнивает строку байт в байт, а фикстура
внутри проверяет `argc`/`argv`/NULL/`envp`/выравнивание. `user-abi-syscalls`
запускает `userland/abi.elf`: `write`, `openat` (существующий файл, создание,
`O_APPEND`, `O_TRUNC`), `read`, `fstat`, `lseek`, `getdents64`, `readlinkat`,
`brk` с записью/проверкой паттерна, `clock_gettime`, `nanosleep`, `uname`,
`getpid`, `close`, `-ENOSYS`/`-EFAULT`/`-EBADF`/`-ENOMEM`, секцию потоков
(`clone`/`gettid`/`set_tid_address`/futex/`exit`/`exit_group`), секцию сигналов
(обработчик SIGUSR1 через `abi_sigrestorer`, `kill`/`kill(0)`/`kill(-1)`/`tgkill`,
`SIG_IGN`, block→pending→unblock, SIGKILL/SIGSTOP/битые аргументы, `-EINTR` на
futex-wait, synchronous SIGSEGV в отдельном потоке, плохой кадр `rt_sigreturn`)
и расширенную сигнальную секцию: `sigaltstack` с проверкой `sp` внутри стека и
`SS_ONSTACK` из обработчика, `SA_SIGINFO` (`si_pid`/`SI_TKILL`/`si_addr`/
`si_status`/`uc_sigmask`/`mcontext`), `SA_NODEFER` (рекурсия глубины 3),
`SA_RESETHAND`, `SA_RESTART` (futex → `-ETIMEDOUT`), SIGSTOP/SIGCONT на занятом
потоке и SIGTSTP/SIGCONT на futex-ожидании; маркеры `abi: altstack ok`,
`abi: siginfo ok`, `abi: jobcontrol ok`, затем секцию процессов:
`fork`/`getppid`/`wait4` (`WNOHANG`, `ECHILD`, `EINVAL`), `abi: fork ok`,
`abi: wait ok`, наследование pipe и обработчика SIGUSR1 (ребёнок шлёт сигнал
себе и видит +1), `execve` с ошибками (`ENOENT`/`EFAULT`/`ENOEXEC`) и успешной
заменой образа (`abi: exec ok`, `abi: exec wait ok`), второй exec — сброс
`SIG_IGN` в `SIG_DFL` и сохранение blocked-маски; маркеры проверяются и
раннером по перехваченному serial-выводу, затем мутационную
половину: `mkdirat` (+`EEXIST`, игнор `mode`), `unlinkat` с `AT_REMOVEDIR`
(`EISDIR`/`ENOTDIR`/`ENOTEMPTY`), `renameat2`, `linkat` (+`EISDIR`/`EINVAL`),
`symlinkat`, `readlinkat`, `fchmodat` (+`ENOTSUP`), `ftruncate`
(+`EINVAL`/`EBADF`/`EISDIR`), уборку всех имён и `ENAMETOOLONG` на 300-байтном
пути. Сокетная секция фикстуры всегда проверяет TCP bad args (`AF_UNIX`,
protocol 6 → `-EAFNOSUPPORT`; второй `socket` → `-ENOSPC`; `connect` с плохим
указателем/длиной/family; `read`/`write`/`shutdown` до connect → `-ENOTCONN`;
`setsockopt`/`getsockopt`; `EBADF` после `close`) и UDP: `SOCK_DGRAM` +
`SOCK_NONBLOCK`, `bind` (bad pointer/len/family, занятый порт `-EADDRINUSE`,
чужой адрес `-EADDRNOTAVAIL`, повторный `-EINVAL`), `recvfrom` с
`MSG_DONTWAIT` → `-EAGAIN`, `SO_TYPE` → `SOCK_DGRAM`, `sendto` без адреса
`-EDESTADDRREQ`, oversize `-EMSGSIZE`, `close` освобождает порт. Затем
фикстура пытается подключиться к `10.0.2.2:8000`: без сервера это `SKIP` (не
FAIL), с сервером — send/recv, `shutdown`, `read` → 0 и `write` → `-EPIPE`.
Без `/mnt` фикстура проходит no-fs половину и выходит 77, что selftest считает
SKIP. Живой UDP-прогон — `tools/test_udp.py` (`libpagh::net::UdpSocket`,
host-эхо, `docs/live-udp.txt`).

Два `selftest` подряд печатают одинаковые ledger'ы — это и есть доказательство,
что не течёт ни образ, ни стек, ни стек задачи, ни heap-кадры, ни fd-узлы.
Растущий стек проверяют три фикстуры: `userland/stack.rs` (40 уровней по 4 KiB,
160 KiB — заведомо больше начальных 64 KiB) обязан вернуться и напечатать
`stack: deep recursion ok`, причём `stack::stats()` должен показать рост не
меньше 16 страниц; `userland/stack-over.rs` (300 уровней, 1.2 MiB) обязан
умереть `Exit::Fault` с `cause == store page fault` ровно после
`MAX_GROWTH_PAGES = 240` выращенных страниц и хотя бы одного отказа;
`userland/stack-fork.rs` форкается на 30-м уровне цепочки (уже ~14 выросших
страниц) и требует оба маркера `stack: fork child ok`/`stack: fork parent ok`,
движение счётчиков `forks`/`waits` и тот же ledger — это единственная проверка
`copy_grown_stack` и teardown'а выросшего стека через `wait4`. Все три
проверки сверяют PMM-ledger до/после: выросшие кадры возвращаются teardown'ом.
Таймер-in-U проверяется счётчиком `traps::user_timer_ticks()` (инкремент только
при `SPP == 0`). Скриншот — `docs/user.png`, снимается `tools/run.py` (см.
`docs/screenshot.md`).