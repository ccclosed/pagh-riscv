# `userland/` — статические RV64-программы для M6b и native ABI

Одиннадцать крошечных программ без рантайма и libc — ровно столько, чтобы проверить
точки входа ядра и контракт `pagh-api/docs/abi.md`. Все собраны в ELF заранее;
ядро встраивает их через `include_bytes!` (`src/user/mod.rs`), поэтому чистый
checkout собирается **без** кросс-компилятора.

| Файл | Что делает | Что доказывает |
|---|---|---|
| `hello.rs` / `hello.elf` | `write(1, "hello from user mode\n")`, `sched_yield`, читает `.data` + `.bss`, `exit(42)` | вход в U-mode, два PT_LOAD (rw data/bss), `ecall`-ABI, возврат из `sched_yield` в U |
| `fault.rs` / `fault.elf` | `getpid`, затем `ld` с адреса 0 | fault из U убивает задачу, а не машину; `run_image` возвращает `Exit::Fault` |
| `spin.rs` / `spin.elf` | `rdtime`-цикл на 0.2 s (~20 тиков), `getpid`, `exit(7)` | таймерное прерывание, взятое в U-mode, обрабатывается и возвращается в U |
| `argv.rs` / `argv.elf` | проверяет `a0=argc`, `a1=argv`, `sp`, `envp = argv+argc+1`, NULL-терминаторы и выравнивание; печатает `argv: argc=3 argv[0]=argv.elf argv[1]=alpha argv[2]=beta env[0]=PWD=/` | SysV-кадр входа и точную строку, которую selftest сравнивает побайтно |
| `abi.rs` / `abi.elf` | `write`, `openat` (существующий файл, создание, `O_APPEND`, `O_TRUNC`), `read`, `fstat`, `lseek`, `getdents64`, `readlinkat`, `brk` с паттерном, `clock_gettime`, `nanosleep`, `uname`, `getpid`, `close`, `-EFAULT`/`-ENOSYS`/`-EBADF`/`-ENOMEM`; **секция потоков**: `gettid`/`set_tid_address`, матрица ошибок `clone`, два потока с общим atomic-счётчиком, futex wait/wake-rendezvous, `exit` не-главного, `exit_group`; **секция сигналов**: обработчик с restorer'ом, `kill`/`tgkill`, маски/pending, `SIG_IGN`, `-EINTR`, synchronous SIGSEGV и расширенная половина — `sigaltstack`+`SA_ONSTACK` (sp внутри стека, `SS_ONSTACK`, fallback), `SA_SIGINFO` (`si_pid`/`si_code`/`si_addr`/`si_status`, `ucontext.mcontext`), `SA_NODEFER` (рекурсия), `SA_RESETHAND`, `SA_RESTART` (futex → `-ETIMEDOUT`) и SIGSTOP/SIGTSTP/SIGCONT, **секция процессов**: `fork`/`getppid`/`wait4` (включая `WNOHANG`, `ECHILD`, `EINVAL`), fork-ребёнок печатает `abi: fork ok`, родитель — `abi: wait ok`, pipe/обработчик сигналов наследуются, дочерний `execve` заменяет образ и печатает `abi: exec ok`/`abi: exec wait ok`, а второй exec проверяет сброс `SIG_IGN` в `SIG_DFL` и сохранение blocked-маски; мутационная половина: `mkdirat`/`unlinkat`/`symlinkat`/`linkat`/`renameat2`/`fchmodat`/`ftruncate` и их ошибки; сокетная секция: `socket` (bad domain/type/protocol, второй активный сокет), `connect` к `10.0.2.2:8000` (без сервера — `SKIP`, не fail), `setsockopt`/`getsockopt`, `send`/`recv`, `shutdown`, EOF и `-EPIPE`, **TCP-сервер**: `bind` TCP-порта, `listen`/повторный `listen`, `accept` с `SOCK_NONBLOCK` → `-EAGAIN` и `-EFAULT`, `bind` на listener → `-EINVAL`, `accept` на unbound stream → `-EINVAL`, `dup` listener'а → `-ENOTSUP`, освобождение порта после `close`; без `/mnt` выходит 77 после no-fs половины, оставляя живой tail-поток | весь native ABI без libpagh: файлы, каталоги, символические ссылки, мутации, heap, время, ошибки, TCP-клиент и TCP-сервер, `clone`/futex/`exit_group` |
| `exec.rs` / `exec-child.elf` | цель `execve`: проверяет, что ядро собрало `argv[1] = "exec-arg"` и `envp[0] = "PWD=/mnt"`, печатает `abi: exec ok argc=2 argv=exec-arg env=PWD=/mnt`, выходит 0 | замена образа/стека/регистров при `execve`, сохранение pid/ppid/fd, сброс обработчиков сигналов |
| `stack.rs` / `stack.elf` | рекурсивная цепочка `descend`: 40 уровней по 4 KiB (160 KiB), в каждом уровне `sd` в дно кадра; печатает `stack: deep recursion ok` и выходит 0 | деманд-рост user-стека: первые ~96 KiB ниже начального окна обязаны быть домаплены ядром, иначе первый же store за `USER_STACK_BASE` убил бы программу |
| `stack-over.rs` / `stack-over.elf` | та же цепочка на 300 уровней (1.2 MiB); если она **вернётся**, выходит 111 | потолок 1 MiB на процесс: рост обязан упереться в guard ниже `USER_STACK_FLOOR` и умереть `store page fault`, не съев PMM |
| `proc.rs` / `proc.elf` | читает синтетический `/proc` обычными syscall'ами: `openat`+`getdents64` на `/proc` (девять статических записей с `d_type`, `self` — симлинк, `kthreads/` и `net/` — каталоги, плюс свой pid) и `/proc/net` (`arp`+`dev`), `fstat` (regular file 0o644, `size()` = числу прочитанных байт), чтение кусками по 7 B с `lseek(0)` и побайтовым сравнением (`version`), содержимое `meminfo` (`MemTotal`/`MemFree`, `free <= total`), `uptime`, `tasks`, `mounts`, `net/dev` (NIC есть или честно «no interface»), read-only-отказы (`mkdirat`/`unlinkat`/`write` → `-ENOTSUP`) и `-ENOENT` на неизвестные имена; **per-pid**: `readlink("/proc/self")` = `getpid()`, `/proc/self/{status,stat,cmdline}` совпадают с `/proc/<pid>/*`, `fork`-ребёнок читается по pid, пока жив, а после `wait4` его pid — `-ENOENT`, `/proc/kthreads/{0,1}` — shell/idle; печатает `proc: … ok` и выходит 0, любая ошибка — свой ненулевой код и строка `proc: FAIL` | userland-доступ к `/proc` через native ABI; снимок на проход чтения (`size`/`read` согласованы); read-only контракт синтетической ФС; per-pid namespace и `self` |
| `statfs.rs` / `statfs.elf` | `statfs(43)`/`fstatfs(44)` через native ABI: `Statfs` — 96 B (10 little-endian `u64` + NUL-padded `type_name[16]`); проверяет поля ext2 (`magic=0xEF53`, power-of-two block size, `blocks_free<=blocks_total`, `blocks_available==blocks_free`, `files_free<=files_total`, `namelen=255`, `flags=0`, `type_name="ext2"`), живой учёт: create+write 4096 B двигает `blocks_free`/`files_free`, `unlink` возвращает; `fstatfs` на fd каталога совпадает с `statfs` пути, на `/proc` — `procfs` с нулями, на console — `-ENOTSUP`, на fd 999 — `-EBADF`; ошибки `-ENOENT`/`-ENOTSUP`/`-EINVAL`/`-EFAULT`; без диска печатает `statfs: no /mnt` и выходит 0; с диском — `statfs: … ok` и `statfs: done`, любая ошибка — свой код и `statfs: FAIL` | native `statfs`/`fstatfs`: реальные счётчики VFS, синтетическая ФС с нулевой ёмкостью, `fstatfs`-семантика fd и весь error-контракт |
| `stack-fork.rs` / `stack-fork.elf` | та же цепочка, но на 30-м уровне (когда ниже начального окна уже выросли ~14 страниц) вызывается `fork(1000)`; родитель ждёт `wait4`, ребёнок доходит до конца на своей копии и печатает `stack: fork child ok`, родитель — `stack: fork parent ok`, оба выходят 0 (при ошибке fork/wait — 9) | `fork` копирует именно `stack_low..USER_STACK_BASE`, а `wait4` возвращает выросшие кадры ребёнка в PMM; рост продолжается и в ребёнке |
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
  `statfs=43`, `fstatfs=44`, `clock_gettime=113`, `uname=160`, `brk=214`,
  `exit=93`, `exit_group=94`,
  `set_tid_address=96`, `futex=98`, `sched_yield=124`, `getpid=172`,
  `getppid=173`, `gettid=178`, `clone=220`, `fork=1000` (pagh-native),
  `execve=221`, `wait4=260`, `socket=198`, `bind=200`, `listen=201`,
  `accept=202`, `connect=203`, `sendto=206`, `recvfrom=207`, `setsockopt=208`,
  `getsockopt=209`, `shutdown=210`.
  `clone` — **нативная** подпись `clone(flags, stack, entry, arg)`, не Linux-raw:
  ребёнок стартует на `entry(arg)`, из syscall не возвращается. `fork=1000` —
  pagh-native (asm-generic номера fork нет); `clone(0,0,0,0)` принимается как его
  алиас. `fork` возвращает pid ребёнка родителю и 0 ребёнку; `wait4(pid, *status,
  WNOHANG, 0)` отдаёт `pid`, 0 (жив), `-ECHILD` или `-EINVAL`, статус — POSIX
  `code << 8` либо номер сигнала в младших 7 битах; `getppid` — 0 у программы из
  шелла. Регистры: `a7` — номер,
  `a0..a5` — аргументы, `a0` — результат
  (`-errno`); диспетчер — `src/user/syscall.rs`.
- Вход: `a0 = argc`, `a1 = argv`, `sp` — 16-выровненный SysV-кадр
  `[argc][argv…][NULL][envp…][NULL][строки]`; `argv[0]` — путь как набран,
  `envp = argv + argc + 1`. У `hello`/`fault`/`spin`/`abi` `argc = 1`; `argv.elf`
  запускается selftest'ом с тремя аргументами и `PWD=/`, `exec-child.elf` —
  с `argv = ["exec-child", "exec-arg"]` и `PWD=/mnt`.
- Никакого `std`/libc: `core::arch::global_asm!` для `_start` и голые `ecall`.
  `abi.rs`/`argv.rs` — `no_std` Rust без аллокатора; `panic_handler` паркует
  программу, поэтому проверки сделаны возвратными кодами, а не паниками.
- `rdtime` (`csrr …, time`) из U-mode работает потому, что OpenSBI оставляет
  `mcounteren.TM = 1`; это свойство фикстуры, а не обещание ABI ядра.
- Пределы: 16 fd, путь ≤ 256 B, heap ровно 1 MiB (`brk`), `read(0)` неблокирующий
  (`-EAGAIN`), `getdents64` — снимок на `openat`; TCP: один активный клиент,
  один listener (`bind`/`listen`, backlog ≤ 4), принятые соединения —
  отдельные fd (8 слотов, 4 KiB приёма), `recv` неблокирующий; плюс UDP
  (`AF_INET`/`SOCK_DGRAM`, 8 bind × 4 дейтаграммы × 1024 B), без DNS/TLS.
  Живая серверная фикстура — `pagh-api/examples/tcp_echo.rs`, прогон —
  `tools/test_tcp_server.py`. Потоков ≤ 14 на процесс, у каждого 16 KiB
  kernel-стека + guard; одновременно живых процессов ≤ 6, у каждого своё Sv39
  root/ASID. TLS/`CLONE_SETTLS`, PI/requeue-фьютексы и `join`/`waitid`
  отсутствуют; futex-таблица 32 ожидающих на 16 адресов; user-стек выделяет
  вызывающий. `fork` — глубокая копия user-страниц (не COW), `wait4` без
  `SIGCHLD` и без `WNOWAIT`; сирот убивает и хоронит раннер фонового `run`, а не
  init. `execve` сбрасывает все обработчики сигналов в `SIG_DFL`, отключает
  alt-стек и сохраняет blocked-маску; fd/pipe наследуются, сокет в ребёнке
  становится `-ENOTSUP`, offset файла копируется, а не делится.

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
SELFTEST user-abi-syscalls PASS     # с /mnt (abi: threads/signals/fork/wait/exec/mutations ok); без него SKIP
pagh:rv64> run proc
proc: version ok
proc: meminfo ok
proc: uptime ok
proc: tasks ok
proc: mounts ok
proc: net ok
proc: directory ok
proc: readonly ok
proc: missing ok
proc: self ok
proc: pid ok
proc: kthreads ok
proc: fork ok
proc: done
run: proc exited with code 0
pagh:rv64> cat /proc/version
pagh 0.24.0 (riscv64) #1
pagh:rv64> cat /proc/self/status
Name:	shell
State:	R (running)
Tid:	0
Pid:	0
PPid:	0
Threads:	1
Switches:	0
Work:	0
Stack:	boot
Satp:	0x8000000000080d1b (kernel)
pagh:rv64> stat /proc/self
  path:  /proc/self
  name:  self
  type:  symbolic link
  mode:  0777
  size:  10 bytes
  inode: 0
  links: 1
  target: kthreads/0
pagh:rv64> ls /proc/kthreads
0/
1/
pagh:rv64> run statfs
statfs: /mnt ext2 block=4096 blocks=16250/16384 files=4085/4096 namelen=255 fsid=0
statfs: ext2 ok
statfs: free-space ok
statfs: fstatfs ok
statfs: /proc procfs block=0 blocks=0/0 files=0/0 namelen=255 fsid=0
statfs: procfs ok
statfs: errors ok
statfs: done
run: statfs exited with code 0
```