# `src/vfs/` — виртуальная файловая система

Слой между шеллом (и будущим syscall-слоем) и конкретной ФС. Один трейт, одна
таблица монтирований, два примитива разрешения путей. `core` + `alloc`, ничего
не печатает.

## Карта

| Что | Где | Зачем |
|---|---|---|
| `VfsNode` | `mod.rs:110` | трейт узла: `name`, `is_directory`, `read/write/truncate`, `readdir/lookup`, `size`, `fs_stat`, `is_symlink`, `read_link`; изменяющие операции по умолчанию `NotSupported` |
| `VfsError` | `mod.rs:50` | POSIX-подобные имена, но **не** errno: маппинг в Linux — одна match-ветка в будущем syscall-слое |
| синтетический корень | `init`, `root_node` | каталог, которым владеет ядро; под ним живёт `/mnt` |
| таблица монтирований | `mount_at`/`unmount_at` | вставить ФС под имя; `..` из монтирования не убегает |
| `resolve(cwd, path)` | `mod.rs:453` | лексическое склеивание относительного пути с cwd |
| `lookup_path` | `mod.rs:509` | узел по пути **без** перехода по последней симлинке (`lstat`/`rm`) |
| `walk` | `mod.rs:520` | переход по путям **с** симлинками и бюджетом шагов (`cat`/`cd`/`write`) |
| `replace_contents` | `mod.rs:584` | атомарная замена содержимого файла (truncate+write) |
| `rename` | `mod.rs:599` | переименование через `VfsNode::rename` |
| `proc` | `proc.rs` | синтетический read-only `/proc`: живые файлы состояния, per-pid (`<pid>/{status,stat,cmdline}`), `/proc/kthreads` для kernel-потоков и `/proc/self`-симлинк; рендер при чтении; монтируется в `init` |
| `FsStat`/`fs_stat`/`fs_type` | `mod.rs` | то, из чего `statfs(43)`/`fstatfs(44)` собирают `Statfs`: magic, `block_size`, блоки/inode'ы, UUID-префикс (`fsid`), имя типа; синтетический узел отвечает `None`/`procfs` |

## `/proc`: синтетический срез живого состояния

`vfs::init` монтирует `/proc` (`proc::root()` в `proc.rs`) **до** диска, потому
что диагностический filesystem нужен именно тогда, когда `/mnt` нет. Дерево —
`cpuinfo`, `meminfo`, `mounts`, `tasks`, `uptime`, `version`, каталог `net/` с
`dev` (интерфейс + DHCP-lease) и `arp` (живой ARP-кэш), плюс два динамических
пространства имён: `<pid>/{status,stat,cmdline}` (pid — это `getpid()`, источник —
`Process`, запись живёт, пока у процесса есть живой поток) и
`kthreads/<tid>/{status,stat}` для kernel-потоков без pid (шелл, idle,
демо-потоки). `/proc/self` — симлинк, цель которого вычисляется в контексте
читателя (`<pid>` либо `kthreads/<tid>`) и разворачивается обычным `walk`, так
что `cat /proc/self/status` не требует ни одной особой ветки в шелле или
syscall-слое. Мёртвый/переиспользованный id — `-ENOENT`; зомби не выставляются
(их забирает `wait4`); прав нет — читаются все живые записи. Файл —
это рендерер, а не буфер: `lookup`/`readdir` создают новый узел, `read` с
offset 0 перерисовывает (живое состояние), следующие offset'ы продолжают тот же
рендер (один проход чтения не склеивает два разных uptime), `size()` равен
длине рендера, чтобы `stat`/`fstat` и `lseek(SEEK_END)` видели ту же длину, что
вернёт чтение (EOF здесь — ответ `read` = 0, а не `size()`). `write`/`truncate`/`create_*`/`remove` остаются дефолтами
`NotSupported` → `-ENOTSUP`; неизвестное имя — `-ENOENT`. Рендерер работает без
блокировки узла (порядок «сначала состояние, потом узел») и берёт не больше
одной подсистемной блокировки за раз. Для `mounts` у узла появился
`fs_type()` (`ext2`, `procfs`, `synthetic`), а `mount_snapshot()` отдаёт таблицу
монтирований снимком — так `/proc/mounts` не держит её лок, пока форматирует
строку. Форматы файлов описаны в `proc.rs`.

`<pid>/status` — key/value-строки Linux-подобных имён; значения реальные:
`State` — самое «проснувшееся» состояние любого живого потока процесса,
`Threads` — их число, `Switches`/`Work` — суммы, `Tid` — id главного потока,
`Stack` — его kernel-стек (`boot` для потока загрузки), `Satp`/`ASID` — корень
процесса. `<pid>/stat` — документированный нативный набор полей
`id (comm) state ppid threads switches work stack_kib satp`: колонок utime/stime
нет, потому что учёта CPU-времени в ядре нет, а выдумывать нули нечем.
`<pid>/cmdline` — снимок argv, скопированный при `run`/`execve` в
`Process.cmdline` (NUL-разделители и завершающий NUL, потолок 128 байт,
`fork` копирует ребёнку); это не живая память программы. Проверки —
`SELFTEST user-proc-fs` (self/свой pid/живой ребёнок/reaped `-ENOENT`,
kthreads) и serial/net/disk smoke-режимы.

## `statfs(2)`/`fstatfs(2)`: откуда берутся цифры

Syscall `statfs(43)`/`fstatfs(44)` (`src/user/syscall.rs`) не считает ничего
сам: он берёт снимок [`FsStat`](mod.rs) у узла, к которому привёл путь (или fd),
и добавляет [`fs_type`](VfsNode::fs_type). Для ext2 это `magic = 0xEF53`,
`block_size`, реальные `s_blocks_count`/`s_free_blocks_count` и
`s_inodes_count`/`s_free_inodes_count`, и `fsid` — первые 8 байт `s_uuid`
(little-endian; у образа без UUID он `0`, и это честный ноль, а не «неизвестно»).
`blocks_available` равен `blocks_free`: форматтер пишет `s_r_blocks_count = 0`,
резерва для root нет. `flags` — `0`, потому что read-only монтирований в ядре
нет. `name_len` — 255: это контракт VFS на имя компонента, а не свойство драйвера.

У синтетического filesystem (`/proc`) ёмкости нет, но тип есть: он отвечает
`procfs` и нулями во всех счётчиках — ноль здесь правда. Синтетический **корень**
(`/`) не является чужой ФС: `-ENOTSUP`. Относительный путь — `-EINVAL` (в ABI нет
cwd), отсутствующий — `-ENOENT`, `NULL`-запись — `-EFAULT`. `fstatfs` работает
только для `File`-дескрипторов: console/pipe/socket/TLS — существующие fd без
файловой системы за ними, поэтому `-ENOTSUP`; несуществующий номер — `-EBADF`.
Проверки — `userland/statfs.rs` (`run statfs`) и `SELFTEST user-statfs`;
последняя сравнивает счётчики до/после реального create+write+unlink, так что
`statfs`, возвращающий константы, её не пройдёт.

## Почему два примитива разрешения путей

`lookup_path` и `walk` разделены намеренно: `rm link` обязан удалять ссылку, а не
её цель, поэтому удаление/`stat`/`readlink` идут через `lookup_path`; `cat`/`cd`/
`write` идут через `walk`, который символические ссылки разворачивает и считает
шаги (защита от циклов). Слить их в один — ровно та ошибка, из-за которой `rm`
начинает удалять чужие данные.

## Что важно знать

* **Пути — UTF-8 `String`.** Ядро живёт в текстовом шелле и не имеет `OsStr`;
  имя на диске, не являющееся UTF-8, ext2 отображает lossy, а не отказывается
  монтировать.
* **`/` — синтетический корень** (пустой каталог во владении ядра). Реальная ФС
  монтируется в `/mnt` при загрузке (`main.rs::init_fs`); без блочного устройства
  `/mnt` не существует, и шелл печатает одну строку об этом.
* **`..` не покидает монтирование**: `/mnt/..` — это корень ФС, а не `/`. Это
  документированное упрощение v1 (в x86-версии так же); код очистки не должен
  полагаться на `..` как на выход из монтирования.
* **Владение и время.** `VfsNode` отдаёт `fs_stat`, `fs_ino`, `mode`, `nlink` —
  ровно то, что нужно `stat`-подобным командам и будущему `stat(2)`; uid/gid и
  времена в M5 не используются.
* **Потокобезопасность.** Узлы `Send + Sync`; сортированные списки детей под
  спинлоком; ядро однохартовое, но контракт записан явно.

## Чего нет

`/dev`, `ramfs`, `/proc/<pid>/task/<tid>` (per-thread view), `/proc/sys`,
`/proc/stat` и запись в `/proc`; hard links и права; page cache и отложенная
запись; `open`/`close`-дескрипторы (шелл адресует файлы путями). `/proc`
существует как read-only срез с per-pid на уровне процесса (см. выше);
остальное — за пределами M5 и названо в `docs/roadmap.md` и `README.md`.

## Как проверить

```sh
# в гостя, с диском (см. src/fs/README.md про mkfs):
#   ls /mnt && mkdir /mnt/d && write /mnt/d/a.txt hi && cat /mnt/d/a.txt
#   cp /mnt/d/a.txt /mnt/d/b.txt && mv /mnt/d/b.txt /mnt/d/c.txt && rm /mnt/d/c.txt
# без диска: одна строка "no block device", шелл работает
# /proc: ls /proc; cat /proc/version; cat /proc/meminfo; cat /proc/tasks;
#        cat /proc/mounts; stat /proc/meminfo; run proc
# per-pid: ls /proc/kthreads; cat /proc/self/status; cat /proc/self/stat;
#          stat /proc/self; cat /proc/0/status  (не процесс -> not found)
# statfs: run statfs   (с диском: /mnt ext2 ...; без диска: statfs: no /mnt)
# selftest: SELFTEST ext2-* (11/11), SELFTEST user-proc-fs, PMM ledger одинаков
```