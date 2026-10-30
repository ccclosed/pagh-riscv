# Скриншоты экрана pagh-riscv

Как получить PNG того, что ядро нарисовало, и как доказать, что нарисован именно
закоммиченный шрифт. Инструмент один — `tools/run.py`; проверка — `tools/font_check.py`
(перенесён из x86-версии вместе с ассетом шрифта).

## Короткий путь

```sh
cargo build
python3 tools/run.py --shot /tmp/riscv-shell.png
```

Скрипт:

1. поднимает QEMU (`qemu-system-riscv64 -machine virt -bios default -device ramfb`)
   с сокетом серийного порта `/tmp/pagh-drv.sock` и сокетом монитора
   `/tmp/pagh-drv-mon.sock`, без дисплея (`-display none`);
2. подключается к серийному сокету **до** старта гостя (`wait=on`), поэтому
   загрузочный лог не теряется, и ждёт маркер `pagh:rv64>`;
3. при `--keys` печатает текст в тот же сокет (на RISC-V `virt` серийный порт —
   это и есть клавиатура: PS/2 в машине нет);
4. снимает экран командой монитора `screendump <tmp> -f png` и переименовывает
   файл в целевой (существующий PNG заменяется только удачной съёмкой);
5. сохраняет серийный лог (`/tmp/pagh-drv-serial.log`) и гасит QEMU.

Часть шагов отдельно:

```sh
python3 tools/run.py --keys "help\n" --shot /tmp/riscv-help.png
python3 tools/run.py --keys "clear\n" --shot /tmp/riscv-clear.png
python3 tools/run.py --keys "run demo\nrun fault\n" --shot /tmp/riscv-user.png --check-font
python3 tools/run.py --net --keys "net\nping 10.0.2.2\n" --shot /tmp/riscv-net.png --check-font
python3 tools/run.py --attach --keys "mem\n" --shot /tmp/riscv-mem.png   # к уже запущенному QEMU
python3 tools/run.py --help
```

## Проверка, что на экране — закоммиченный шрифт

`font_check.py` пересчитывает ожидаемые пиксели из `assets/font8x18.bin` **по той
же формуле**, что и ядро (`blend_px`: покрытие 0..255, округление `+127`), и
сравнивает с PNG без допусков:

```sh
python3 tools/font_check.py --screenshot /tmp/riscv-shell.png --find --expect "Type 'help'"
# font_check: SCREEN OK row=11 col=0 diff=0

python3 tools/font_check.py --screenshot /tmp/riscv-shell.png --scan-all
# scan      no cell on the console holds pixels outside the asset  OK
```

* `--find` ищет позицию, где строка совпала **пиксель в пиксель**, — то есть
  строку/колонку не нужно угадывать;
* `--scan-all` проверяет каждую клетку выше статус-бара: пустая или ровно один
  глиф ассета (маска чернил не зависит от цвета, поэтому зелёное приглашение и
  красные ошибки тоже покрыты);
* `--diff-out /tmp/diff.png` пишет три панели (ожидание / экран / отличия
  красным), когда что-то не сошлось;
* без `--screenshot` тот же файл проверяет сам ассет (95 глифов × 18 × 8 =
  13680 байт, ровно один пустой глиф — пробел, есть частичное покрытие) и
  сверяет `CHAR_WIDTH`/`CHAR_HEIGHT` в `src/drivers/framebuffer.rs` с ассетом.

Одной командой и съёмка, и проверка:

```sh
python3 tools/run.py --keys "help\n" --shot /tmp/riscv-help.png --check-font --scan-all
```

`--check-font` возвращает ненулевой код, если проверка не прошла, поэтому это
пригодно для CI. По умолчанию проверяется `Type 'help'` (приветствие шелла есть
на каждом экране загрузки); для снимка после `clear` ожидание другое:

```sh
python3 tools/run.py --keys "clear\n" --shot /tmp/riscv-clear.png \
    --check-font --expect "pagh:rv64>" --scan-all
```

## Почему это вообще возможно headless

Дисплей у машины — устройство `ramfb`: ядро через fw_cfg (`src/drivers/fw_cfg.rs`)
отдаёт ему адрес поверхности в своей RAM (`src/drivers/ramfb.rs`), а QEMU
показывает её как кадровый буфер. Поэтому `-display none` не мешает съёмке:
`screendump` читает ту же поверхность, что увидел бы человек. Префикс
`framebuffer  ramfb 1280x800 stride 5120 cells 100x37` в серийном логе — признак,
что устройство приняло конфигурацию (ядро читает её обратно и сверяет байты).

Без `-device ramfb` ядро остаётся работоспособным, но снимать нечего: в логе
будет `framebuffer  ramfb: no such fw_cfg item — serial console only`, а
`screendump` вернёт пустую поверхность. `run.py` всегда добавляет `-device ramfb`,
и `run.sh` тоже (там же есть `--window`, который показывает ту же поверхность в
окне QEMU).

## Что смотреть глазами

* `docs/shell.png` — консоль с `help` (снимается тем же `run.py`, см. выше);
* `docs/fs.png` — `DISK`-образ: `ls /mnt`, `disk`, `cat`, `stat`;
* `docs/tasks.png` — `spawn 3`, `tasks`, `yield 48`: таблица потоков и счётчики;
* `docs/user.png` — M6b: `run demo` (hello + exit 42), `run fault` (killed, машина
  жива), `run spin` (0.2 s в U-mode, exit 7); снимается
  `python3 tools/run.py --keys "run demo\nrun fault\nrun spin\n" --shot docs/user.png
  --check-font --expect "run: demo exited with code 42" --scan-all`;
* `docs/net.png` — M7: `net` (MAC + DHCP lease), `ping 10.0.2.2` (свой
  ARP/ICMP-путь) и `dns`; снимается `python3 tools/run.py --net --keys
  "net\nping 10.0.2.2\n" --shot docs/net.png --check-font --expect
  "ping: reply from 10.0.2.2" --scan-all`;
* статус-бар внизу: слева `pagh OS  RISC-V (RV64)`, справа `ramfb 1280x800`;
* текст занимает 100×37 клеток, ниже — чёрная полоса, затем бар (раскладка
  x86-версии, см. `src/drivers/README.md`);
* не-ASCII символы (например длинное тире в баннере) на экран не попадают:
  в шрифте нет глифов вне `0x20..0x7E`, и `write_char` их отбрасывает.

## Если что-то не так

| Симптом | Причина | Что делать |
|---|---|---|
| `run: cannot connect to the serial socket` | QEMU не стартовал (нет ядра, занят сокет) | `cargo build`; `ls -l /tmp/pagh-drv*.sock`; запустить с `--serial /tmp/другой.sock` |
| `run: the marker 'pagh:rv64>' did not appear` | ядро упало или изменился текст приглашения | смотреть `/tmp/pagh-drv-serial.log`; маркер переопределяется `--wait` |
| PNG чёрный | гость ещё не дошёл до `shell::run`, либо собран другой ELF | `--settle`, `--kernel target/.../release/pagh` |
| `screendump produced no file` | нет `-device ramfb` (см. выше) | запускать через `run.py` |
| `font_check: SCREEN FAIL` | шрифт/константы разошлись или снят не тот кадр | `python3 tools/font_check.py --screenshot … --diff-out /tmp/diff.png` |
| ассет не совпадает с пайплайном | шрифт перегенерировали другим размером/кеглем | `python3 tools/gen_font.py --out assets/font8x18.bin --check` |