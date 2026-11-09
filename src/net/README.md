# `src/net/` — сеть: свой стек и virtio-net (M7)

Сеть в pagh-riscv — это **свой** стек поверх своего TCP/IP, без smoltcp и без
внешних крейтов, и драйвер virtio-net, написанный рядом с virtio-blk. Всё
`core`+`alloc`, всё опрашивается: сетевого потока и прерываний у NIC нет.

## Карта

| Файл | Роль |
|---|---|
| `mod.rs` | состояние стека: MAC, конфигурация DHCP, ARP-кэш, `poll()`, диспетчер кадров (ARP/IPv4/ICMP/UDP/TCP), `ping`, `wait_udp` для DHCP/DNS и per-port очередь userland-UDP (`udp_bind`/`udp_take_into`/`udp_wait_into`), сборка кадров |
| `wire.rs` | чистые форматы: Ethernet, ARP, IPv4, ICMP, UDP, TCP, DHCP, DNS; контрольные суммы (RFC 1071 + pseudo-header) и текстовые помощники. Не знает ни устройства, ни часов — поэтому тестируется без QEMU |
| `dhcp.rs` | DHCP-клиент: DISCOVER/OFFER/REQUEST/ACK, опции 1/3/6/50/51/53/54 |
| `dns.rs` | stub-резолвер: A-запрос по UDP, разбор ответа с компрессией имён |
| `tcp.rs` | TCP: один active-open клиент для HTTP/1.0, пассивный open (`bind`/`listen`/`accept`) с очередью на 4, таблица из 8 соединений (seq/ack, 4 KiB приёма, FIN); `recv_nonblocking[_id]` — один `poll()` и `WouldBlock`, `peer[_id]` — адрес для `recvfrom`/`accept` |
| `tls.rs` | TLS 1.3 клиент: fail-closed проверка сервера, `KernelRng`/`KernelVerifier`/`KernelProvider`, транспорт `TlsTransport` поверх `tcp.rs`, `block_on`, one-shot `https_get[_at]` **и** userland-сессии (`user_open`/`user_write`/`user_read`/`user_ready`/`user_shutdown`/`user_close`) для syscall'ов 1100–1103 |
| `tls_chain.rs`, `tls_auth.rs`, `tls_verify.rs`, `x509.rs`, `hostname.rs`, `ca_bundle.rs` | решение «доверять ли серверу»: сборка цепочки, подписи, SAN по RFC 6125, закоммиченные корни |
| `http.rs` | чистые `build_get_request` / `parse_http_head` + инкрементальный декодер `chunked` |
| `selftest.rs` | 12 проверок для `selftest`: контрольные суммы, round-trip всех заголовков, разбор DHCP/DNS из фиксированных байт, TCP-сегмент, fail-closed поверхность таблицы TLS-сессий |

Драйвер — `src/drivers/virtio_net.rs`; его контракт с этим модулем: `mac()`,
`send(frame)`, `recv(out)` и одна строка лога при загрузке.

## Модель

**Опрос, не прерывания.** У virtio-mmio транспорта нет включённого прерывания
(PLIC разведён только на UART), поэтому доступный ring помечен
`NO_INTERRUPT`, а ожидание — ограниченный опрос. `net::poll()` вычитывает все
завершённые RX-буферы и разбирает кадры; операции, которым надо ждать
(`dhcp::acquire`, `dns::resolve`, `ping`, `tcp`-вызовы), крутят `poll()` до
дедлайна в тиках (`timer::ticks()`, 10 мс/тик) и возвращаются с ошибкой, а не
висят.

**Один интерфейс, один адрес.** DHCP выдаёт `ip/mask/gateway/dns`; второй
адрес, второй шлюз, IPv6 и фрагментация не поддерживаются (фрагмент
отбрасывается, а не собирается: сборка — это отдельная поверхность атак, которая
стеку не нужна).

**`wire.rs` — единственное место, где живут байты и порядок сети.** Сборка и
разбор каждого заголовка — пара функций; парсеры проверяют контрольные суммы и
границы и возвращают `None` на любом мусоре (в ядре `panic = "abort"`, поэтому
«индекс за границей» — это мёртвая машина). Контрольные суммы:
IPv4 — по заголовку; UDP/TCP — с pseudo-header; UDP с нулевой суммой означает
«не вычислена» (RFC 768).

**ARP.** Кэш на 8 записей со сроком 60 с; при промахе — запрос broadcast и
ограниченный опрос. Отвечаем на request'ы, адресованные нам; чужие не
пересылаем (машина не роутер).

**DHCP до появления адреса.** Пока lease нет, `handle_ipv4` принимает только
limited-broadcast (`255.255.255.255`) кадры — именно так приходят OFFER/ACK.
Это не дыра: всё остальное до получения адреса отбрасывается, а UDP-очередь
проверяет порты 68→67 и transaction id.

**ICMP.** Отвечаем на echo request к нашему адресу, принимаем echo reply для
`ping`; `ping` шлёт до 4 проб с идентификатором `0x5041` и печатает RTT в мс
(тик = 10 мс).

**TCP.** Один active-open клиент и один listener. `SYN → SYN-ACK → ACK`,
данные с `PSH|ACK`, приём в буфер 4 KiB на соединение, `FIN` от peer'а
переводит в `Closing` (отправка после peer FIN разрешена — это echo-сервер).
Окно в каждом ACK — **свободное место в буфере**, а после вычитывания данных
`recv` присылает window update: иначе 13 КиБ пакета застревали после первых
4 КиБ (sender заливал окно, лишнее терялось, и восстановление ждало RTO).
Пропуск в последовательности получает duplicate ACK — это fast-retransmit у
отправителя, а не полный тайм-аут. Активный локальный порт — свежий на каждое
соединение (`49152+`), иначе повторный `pkg install` после закрытия попадал в
4-tuple, который слой user-network ещё держал, и SYN терялся. Пассивное
состояние: `bind_port` резервирует **единственный** listener-порт, `listen`
включает приём и зажимает backlog в `1..=4`, SYN создаёт соединение в
`SynReceived`, третий ACK переводит в `Established` и кладёт id в очередь;
`accept` забирает старейшее. Очередь переполнена — RST. Таблица — 8 слотов
(клиент + принятые/ждущие), поэтому принятые соединения живут одновременно;
`close_listener` сбрасывает только непринятые, `close_id` шлёт FIN (RST для
половинного passive). Нет retransmission-таймера (кроме двух попыток SYN), нет
out-of-order reassembly, congestion control, keepalive, TIME_WAIT и half-close
— это HTTP/1.0-клиент и маленький сервер, а не замена Linux TCP. Ошибки
типизированы (`tcp::Error`), а не «молча пусто».

**Сокетный ABI для userland.** `src/user/syscall.rs` даёт минимальный BSD-набор
поверх этого клиента, listener'а и UDP: `socket` (`AF_INET`/`SOCK_STREAM` —
один активный клиент, второй активный — `-ENOSPC`; listener — `FdEntry::Listener`,
принятые — `FdEntry::TcpConn`; `AF_INET`/`SOCK_DGRAM` — UDP-сокет, можно много;
`SOCK_NONBLOCK` — бит типа для UDP `recvfrom` и stream `accept`), `bind(200)`
(16-байтный `sockaddr_in`, `0.0.0.0` или lease, port 0 — эфемерный; UDP —
занятый порт `-EADDRINUSE`, TCP — резерв под `listen`), `listen(201)`
(backlog `1..=4`, один listener), `accept(202)` (peer `sockaddr_in`, новый fd,
блокирующий опрос до 30 s или `-EAGAIN` с `SOCK_NONBLOCK`), `connect`
(16-байтный `sockaddr_in`, IPv4-литерал, ошибки
`-ENETUNREACH`/`-EHOSTUNREACH`/`-ECONNREFUSED`/`-ETIMEDOUT`; UDP — `-ENOTSUP`),
`sendto`/`recvfrom` (их же форма — `send`/`recv`; `read`/`write` на socket fd
маршрутизируются сюда), `setsockopt`/`getsockopt` (типовые опции принимаются и
игнорируются) и `shutdown` (оба направления, fd остаётся). TCP `recv`
неблокирующий: `-EAGAIN` — «опять нет данных», 0 — peer закрылся. UDP
`recvfrom` без `MSG_DONTWAIT` блокируется: он крутит `net::poll()` до
дейтаграммы или тикового бюджета (`UDP_RECV_TICKS = 1000`, ~10 s), затем
`-EAGAIN`; `MSG_DONTWAIT`/`SOCK_NONBLOCK` — один poll. Дейтаграмма
копируется в per-port очередь из `handle_udp`: 8 портов по 4 слота, payload
≤ 1024 B, при переполнении вытесняется самая старая (UDP semantics). Очередь
защищена отдельным `Spinlock`, лок не держится через `copy_to_user`/`poll`.
`FdEntry::Socket`/`Listener`/`TcpConn`/`UdpSocket` живут в per-process таблице и
закрываются при teardown (UDP `close` освобождает порт, listener — очередь).
Пока user-программа работает, шелл не в idle-цикле, поэтому
`connect`/`send`/`shutdown`/`close`/`accept` и блокирующий `recvfrom` включают
прерывания на время ожидания и сами вызывают `net::poll()` на каждом шаге —
иначе тики бы стояли, а NIC остался бы без опроса.

## Команды шелла

| Команда | Что делает |
|---|---|
| `net` | MAC, `ip/mask`, gateway, DNS, срок lease |
| `dhcp` | повторить DHCP-обмен и напечатать новый lease |
| `ping <ip> [n]` | до 4 ICMP echo; `seq`, `time=… ms`, итог |
| `dns <name>` | A-запись через DNS из lease |
| `http <ip> <port> <path>` | HTTP/1.0 GET через TCP-клиент, ответ печатается |

## Как проверяется

- `selftest` → `SELFTEST net-* 12/12`: контрольные суммы (вектор RFC 1071),
  Ethernet/IPv4/UDP/ICMP/ARP/TCP round-trip + отказ на испорченном байте,
  DHCP-парсер на «серверном» сообщении, DNS-ответ с compression pointer,
  текст/подсети. `net-udp` дополнительно биндит порт, заливает в
  `handle_udp` на одну дейтаграмму больше глубины очереди и проверяет FIFO с
  вытеснением старшей. `net-tls-session-gate` берёт свободный id таблицы
  userland-TLS-сессий и требует отказа от всех её точек входа
  (`user_read`/`user_write`/`user_shutdown`/`user_ready`/`user_close`) — без
  сервера и без единой сессии. Без NIC — набор работает на любой машине.
- Живой прогон: `run.sh` (сеть включена по умолчанию) → в логе
  `virtio-net: virtio-net0 … MAC …` и `net         DHCP ip 10.0.2.15/24 …`;
  `ping 10.0.2.2` → 4/4 reply; `dns example.com`; `http` к серверу на хосте
  (`10.0.2.2` — это loopback хоста в QEMU user network).
- Живой сокетный прогон: `python3 tools/test_sockets.py` (сервер на
  `127.0.0.1:8000`, `http_get` на диске, `selftest` с реальным connect/recv/
  shutdown и `run /mnt/http_get`); транскрипт — `docs/live-sockets.txt`.
- Живой UDP-прогон: `python3 tools/test_udp.py` — `tools/run.py` форматирует
  ext2, `debugfs` кладёт libpagh `udp_echo`, host-эхо на `127.0.0.1:PORT`
  отвечает гостю (`10.0.2.2:PORT`); фикстура печатает bind/sent/recv и
  выходит 0. Транскрипт — `docs/live-udp.txt`.
- Живой TCP-сервер: `python3 tools/test_tcp_server.py` собирает ext2-образ
  `mke2fs -d` с libpagh `tcp_echo` (без boot-форматирования), поднимает QEMU
  `user` с `hostfwd=tcp:127.0.0.1:HOST-:45679`, host-клиент шлёт 1040 B и
  получает эхо; фикстура печатает bind/listen/accept/recv/echo и выходит 0.
  Транскрипт — `docs/live-tcp-server.txt`.
- CI: `python3 .github/smoke.py <elf> --net` — загрузка, DHCP, `net`, ping
  шлюза, `halt` (QEMU's slirp, без зависимости от интернета раннера).
- Скриншот `docs/net.png` (`tools/run.py --net --keys "net\nping 10.0.2.2\n"`).

## Чего нет (честно)

- Нет TLS и apt: и то и другое — следующий майлстоун, требует крипто-слоя
  (vendored `embedded-tls` + OpenPGP-проверка, как на x86). Сейчас `http`, а не
  `https`.
- IPv6, фрагментация, ICMP redirect/unreachable, UDP-`connect`/`sendmsg`/
  `recvmsg`, несколько TCP-listener'ов, keepalive, Nagle/окна больше
  фиксированного, out-of-order reassembly, congestion control, TIME_WAIT и
  half-close. TCP-сервер ограничен одним listener'ом, backlog ≤ 4, восемью
  слотами соединений и 4 KiB приёма. UDP ограничен 8 bind, 4 дейтаграммами на
  порт и payload ≤ 1024 B; блокирующий `recvfrom` не прерывается сигналом и
  отвечает `-EAGAIN` по ~10 s бюджету. `setsockopt`/`getsockopt` принимают
  типовые опции, но ничего не настраивают; DNS из userland вызывается только
  shell-командой `dns`, не сокетным syscall'ом; TLS нет.
- Прерывания NIC: всё polled, `wfi`-idle шелла просыпается на тик и сам
  опрашивает сеть.
- DHCP renewal по таймеру: `dhcp` — ручная команда; lease не обновляется сам.
- Один интерфейс, один адрес, один DNS; MTU 1500; нет VLAN/MAC-фильтров.
