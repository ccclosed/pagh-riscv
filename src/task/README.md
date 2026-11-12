# `src/task/` — потоки ядра и планировщик (M6a + M6b)

M6a — это потоки *внутри* S-mode: контекстный switch, round-robin очередь и
вытеснение по таймеру. M6b (user mode, syscall'ы, ELF) построен сверху:
user-программа — это такая же задача, у которой подготовлен кадр возврата в
U-mode (см. «M6b: U-mode как состояние задачи»). С native-proc у задачи появился
ещё и `satp`: поток процесса исполняется в root/ASID своего процесса, а switch
устанавливает его перед `task_switch`.

## Карта

| Файл | Роль |
|---|---|
| `mod.rs` | фасад: `init`, `spawn`, `tick`, `stats`, `snapshot`, `spawn_demo`; user-потоки идут через `scheduler` напрямую |
| `scheduler.rs` | таблица задач, ready-queue, квант, стеки из PMM, switch, idle, `block_current`/`wake_task`/`kill_group`, `spawn_user_thread`/`spawn_user_frame`/`spawn_in_group_satp`, `set_current_satp`, `install_satp`, `stop_current`/`continue_task`, счётчики |
| `selftest.rs` | пять проверок для `selftest` (плюс `user/selftest.rs` для M6b) |

Регистровый контракт switch — `src/arch/riscv64/switch.rs`; там же `TaskContext`
и сборка, которая берёт смещения из layout'а структуры.

## Модель

* **Слоты.** `MAX_TASKS = 16`. Слот 0 — поток, который вызвал `init`, то есть
  шелл на загрузочном стеке (`stack_base = 0` — маркер «стек не наш»). Слот 1 —
  `idle`, его стек выделен из PMM один раз и живёт весь boot. Слоты 2.. — потоки
  `spawn`/`spawn_in_group_satp`/`spawn_user_thread`/`spawn_user_frame`, поэтому
  `spawn` демо-когорты принимает не больше 14 за раз, а один процесс — не больше
  14 живых потоков (главный + 13 `clone`); `shell` и `idle` занимают два слота,
  а процессов в таблице `MAX_PROCESSES = 6`.
* **Стек.** 24 KiB usable (96 KiB в debug) + 4 KiB guard (7/25 кадров
  `alloc_frames_contiguous`); размеры измерены по самому глубокому пути —
  рукопожатию TLS 1.3 в syscall 1100 (см. `docs/tls.md`).
  Guard внизу, канарейка в начале стека; `reap` проверяет её и сообщает
  `[task] STACK OVERFLOW`, после чего всё равно возвращает кадры. Выход потока
  обязан вернуть стек: `SELFTEST task-exit-returns-stack` проверяет
  «N → N−5 (поток жив) → N (reap)».
* **Состояния.** `Free -> Ready -> Running -> Exited -> Free`, плюс
  `Blocked` для futex/wait4: `block_current(timeout)` снимает поток с очереди и
  ставит `wake_at` (абсолютный тик); `wake_task(id)` возвращает его в `Ready`, а
  тиковый скан `wake_expired` делает то же по дедлайну. `Stopped` — job-control
  остановка (`stop_current`/`continue_task`), не в ready-очереди, но считается
  живым для `kill`, `kill_group` и ожидания раннера. `schedule(reason)`
  выбирает следующий `Ready` FIFO-порядком, квант — `QUANTUM_TICKS = 8` тиков;
  `Reason` (`Quantum`, `Yield`, `Exit`, `Block`, `ProbeIdle`, …) существует для
  отладки и счётчиков, а не для политики. Блокировка — настоящий switch: лок
  очереди не держится, другие потоки (включая шелл и idle) работают.
* **idle.** Отдельный поток держит hart в `wfi`, пока нет готовых; его пробуждает
  прерывание. `run_idle_once` — проба для selftest: доказать, что switch в
  свежий контекст и обратно работает.
* **Вытеснение.** Одна строка в `kmain`:
  `timer::set_tick_handler(Some(task::scheduler::tick_from_timer))`. Таймер (M3)
  вызывает хук из trap-контекста; `tick_from_timer` помечает аппаратные часы
  привязанными и вызывает `schedule`. Переключение из обработчика корректно,
  потому что кадр прерывания лежит на стеке вытесняемого потока: когда поток
  снова получит CPU, его обработчик просто доедет до `sret`.
  Без таймера (машина без timebase) работают *программные часы*:
  `scheduler::pump()` вызывают шелл и тело демо-потока, а после привязки таймера
  `pump` — no-op.
* **Lock-дисциплина.** Таблица под спинлоком с замаскированными прерываниями.
  `schedule()` берёт лок, выбирает слот, отпускает лок и только потом делает
  `task_switch`: switch никогда не выполняется с удержанным локом. В тике и
  switch нет аллокаций и печати — печать из trap-контекста запрещена (AGENTS,
  инвариант 13), поэтому счётчики читает обычный контекст (`tasks`, `selftest`).
  **Порядок для user-потоков:** futex/process → scheduler (`wake_task` зовётся
  без signal- и process-лока; обратного пути нет — планировщик/тик не берут
  futex/process/сигнальный лок). `kill_group` снимает `Ready`/`Blocked`/`Stopped`
  и чистит ready-ring (`compact_ready`), не трогая текущий слот; стеки снимает
  следующий `reap`. `install_satp`/`set_current_satp` — единственные писатели
  `satp`; первый зовётся под scheduler-локом непосредственно перед switch.

## Как это проверяется

```sh
# в гостя:
spawn 3        # три демо-потока, каждый крутит свой счётчик
tasks          # id/имя/состояние/переключения/работа/стек — снимок под локом
yield 64       # 64 тика -> ~32 переключения, счётчики растут
sleep 1        # с привязанным таймером счётчики растут БЕЗ yield — это вытеснение
selftest       # SELFTEST task-* 5/5 + ledger'ы PMM в двух прогонах совпадают
```

`SELFTEST task-*`: interleave (130 switch-out за 2×64 итерации), exit-returns-stack,
stack-layout/guard, canary (переполнение ловится на reap), idle-probe.

## M6b: U-mode как состояние задачи

Планировщик не знает, что такое процесс, но знает два своих поля: `group`
(pid процесса) и `satp` (root, в котором поток должен исполняться).

1. `run_source` (`src/user/mod.rs`) создаёт root/ASID, грузит в него ELF, мапит
   user-стек и делает `spawn_in_group_satp(user_task, pid, space.satp)`; тело
   `user_task` читает запрос запуска из `Process` и вызывает `enter_user`;
2. `enter_user` кладёт в `sscratch` указатель на стек ядра этой задачи и уходит
   в U-mode через `sret` (`sstatus.SPP = 0`, `SPIE = 1`, `SUM = 0`, `sepc = entry`);
3. первый же trap из U-mode строится уже на **стеке этой задачи** (обмен
   `sp`/`sscratch` в `trap_entry`), поэтому переключение из обработчика ловушки —
   обычное: кадр живёт на стеке вытесняемого потока. Kernel-карта скопирована в
   root процесса, поэтому код и стек ловушки доступны без смены `satp`;
4. `ecall` обслуживается диспетчером в trap-контексте; таймерное прерывание в
   U-mode вытесняет задачу так же, как kernel-поток (это доказано счётчиком
   `traps::user_timer_ticks()` и фикстурой `run spin`);
5. `clone`-поток и `fork`-ребёнок стартуют не через `enter_user`, а через
   контекст `{ ra = traps::trap_return, sp = верх kernel-стека − sizeof(TrapFrame) }`:
   на стеке лежит обычный `TrapFrame` (`sepc=entry` для clone, сохранённый
   родительский кадр с `a0=0`/`sepc+4` для fork, `sstatus` как у `enter_user`),
   и эпилог трапа сам делает `sret`. Второго frame layout'а нет — см.
   `src/user/README.md`;
6. `exit` завершает поток (`threads -= 1`), `exit_group`/fault снимают группу
   `kill_group`; `run_source` ждёт `Process::threads == 0` циклом `yield_now`,
   затем `reap` возвращает по 5 кадров каждого kernel-стека, а раннер/`wait4`
   снимает user-маппинги, fd и root;
7. `scheduler::install_satp(to)` вызывается под scheduler-локом **перед**
   `task_switch`: `satp` пишется и делается полный `sfence.vma`, потому что
   ASID — метка TLB, а не гарантия, что кадр таблицы не переиспользован. При
   возврате к ядру (ASID 0) и при переключении между потоками одного процесса
   установка не меняется (сравнение значения).

Что планировщик при этом **не** делает: не выделяет и не освобождает user-страницы
(это `src/user/`), не хранит отдельного «user-контекста» — сохранённый
`TaskContext` указывает в trap-обработчик, а кадр U-mode лежит на kernel-стеке
задачи. `set_current_satp` обновляет `Task.satp` после `execve`, чтобы
следующий switch не вернул старый root.

## Чего нет

Приоритетов, sleep/wakeup-очередей (есть только futex/`wait4` `Blocked` с
тиковым дедлайном), блокирующего I/O/SMP. Адресное пространство теперь
per-process (`src/memory/paging.rs`, `src/user/process.rs`), но:
ASID не используется для пропуска flush'ей, процессов ≤ 6, `fork` — deep copy
без COW, а планировщик по-прежнему один hart и round-robin. `panic = "abort"`:
паника в потоке убивает машину, как и везде.
