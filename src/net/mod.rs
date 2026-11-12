//! The kernel's network stack (M7): a polled NIC driver seam, ARP, IPv4,
//! ICMP echo, UDP, a DHCP client, a DNS resolver and a minimal TCP client.
//!
//! # Shape of the stack
//!
//! There is no network thread and no interrupt: [`poll`] drains every frame the
//! NIC has received and dispatches it; operations that wait (`dhcp::acquire`,
//! `dns::resolve`, [`ping`]) call `poll` in a bounded loop and give up on a
//! tick deadline. That is the same synchronous model the block driver and the
//! shell use, and it keeps the whole stack `core`+`alloc` with no scheduler
//! integration.
//!
//! The layers are deliberately flat: [`wire`] knows bytes and checksums and
//! nothing else (it is what the selftest exercises), this module owns the one
//! NIC, the IP configuration and the ARP cache, and the small protocol clients
//! (`dhcp`, `dns`, `tcp`) sit on top through the crate-internal send/wait
//! primitives.
//!
//! # What it does and does not do
//!
//! * IPv4 only; IPv6 frames are ignored. Fragments are dropped, not reassembled.
//! * One interface, one address, one default gateway, one DNS server — the
//!   configuration a QEMU `user` network hands out over DHCP.
//! * ICMP echo, UDP, and a small active-open TCP client (enough for an HTTP/1.0
//!   request); no listening sockets, no congestion control beyond a fixed
//!   window, no retransmission timer beyond a bounded poll.
//! * No TLS, no apt: those are the next milestone (see `docs/roadmap.md`).

pub mod ca_bundle;
pub mod dhcp;
pub mod dns;
pub mod hostname;
pub mod http;
pub mod selftest;
pub mod tcp;
pub mod tls;
pub mod tls_auth;
pub mod tls_chain;
pub mod tls_verify;
pub mod wire;
pub mod x509;

use crate::arch::riscv64::timer;
use crate::drivers::virtio_net;
use crate::fdt::Fdt;
use crate::sync::Spinlock;
use wire::*;

/// Largest frame this stack builds or accepts (Ethernet header + 1500-byte MTU).
pub const MAX_FRAME: usize = ETH_HEADER + 1500;

/// Payload bytes kept from one received UDP datagram: enough for a DHCP message
/// (300 bytes), a DNS response (512 bytes typical maximum) or a small userland
/// datagram, with headroom. This is also the largest payload `sendto` accepts;
/// a bigger datagram is `-EMSGSIZE` rather than a silent truncation.
pub const UDP_DATAGRAM_MAX: usize = 1024;
const UDP_PAYLOAD_MAX: usize = UDP_DATAGRAM_MAX;
/// Received datagrams kept for consumers (a DHCP offer and a DNS answer can be
/// in flight at the same time).
const UDP_SLOTS: usize = 4;
/// Userland sockets that may hold a bound UDP port at once. Each slot owns a
/// [`UDP_SLOTS`]-deep queue, so the table is a fixed 32 KiB of static RAM; a
/// ninth `bind` is `-ENOSPC` rather than a heap allocation on the receive path.
const UDP_BINDINGS: usize = 8;
/// First and last port tried by `bind(0)` / the first `sendto` on an unbound
/// socket. The range is above the Linux ephemeral floor QEMU's host stack uses
/// and below the TCP client's 49152, so the two allocators cannot collide by
/// accident.
const UDP_EPHEMERAL_FIRST: u16 = 32768;
const UDP_EPHEMERAL_LAST: u16 = 60999;
/// Echo replies kept for the ping command.
const ICMP_SLOTS: usize = 4;
/// Echo data echoed back and kept in a reply (the ping probe payload).
const ICMP_DATA_MAX: usize = 32;
/// ARP cache entries. Public because `/proc/net/arp` sizes its snapshot buffer
/// from the table it reports on: the two cannot drift.
pub const ARP_ENTRIES: usize = 8;
/// How long an ARP entry stays fresh, in 10 ms ticks (60 s).
const ARP_LIFETIME_TICKS: u64 = 6000;
/// Ticks an ARP resolution may take (1 s).
const ARP_WAIT_TICKS: u64 = 100;
/// Conservative iteration cap for waits: also bounds a wait on a machine whose
/// timer never advances (`ticks()` would stay put).
const POLL_ITERATIONS: u64 = 4_000_000;
/// Identifier the `ping` command puts in its echo requests.
const PING_ID: u16 = 0x5041;
/// Source port the DNS client uses. One resolver, one transaction at a time.
const DNS_SOURCE_PORT: u16 = 53001;

/// The interface configuration handed out by DHCP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// Our IPv4 address.
    pub ip: Ipv4,
    /// Subnet mask.
    pub mask: Ipv4,
    /// Default gateway.
    pub gateway: Ipv4,
    /// DNS server.
    pub dns: Ipv4,
    /// DHCP server that leased the address.
    pub server: Ipv4,
    /// Lease length in seconds (0 when the server did not say).
    pub lease_seconds: u32,
}

/// A received UDP datagram, copied out of the frame buffer.
#[derive(Clone, Copy)]
pub struct Datagram {
    /// Sender address.
    pub src_ip: Ipv4,
    /// Sender port.
    pub src_port: u16,
    /// Destination (local) port.
    pub dst_port: u16,
    /// Payload length.
    pub len: usize,
    /// Payload bytes (`len` of them are meaningful).
    pub data: [u8; UDP_PAYLOAD_MAX],
}

/// One ICMP echo reply kept for [`ping`].
#[derive(Clone, Copy)]
struct EchoReply {
    src: Ipv4,
    id: u16,
    sequence: u16,
    data: [u8; ICMP_DATA_MAX],
}

/// A fixed-capacity datagram queue. Oldest entries are dropped when full: a slow
/// consumer loses a packet, which is exactly what UDP promises.
#[derive(Clone, Copy)]
struct DatagramQueue {
    slots: [Datagram; UDP_SLOTS],
    head: usize,
    count: usize,
}

impl DatagramQueue {
    const fn new() -> Self {
        DatagramQueue {
            slots: [Datagram {
                src_ip: [0; 4],
                src_port: 0,
                dst_port: 0,
                len: 0,
                data: [0; UDP_PAYLOAD_MAX],
            }; UDP_SLOTS],
            head: 0,
            count: 0,
        }
    }

    fn push(&mut self, datagram: Datagram) {
        if self.count == UDP_SLOTS {
            // Drop the oldest to make room for the newest.
            self.head = (self.head + 1) % UDP_SLOTS;
            self.count -= 1;
        }
        let slot = (self.head + self.count) % UDP_SLOTS;
        self.slots[slot] = datagram;
        self.count += 1;
    }

    fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Forget every queued datagram without constructing a 4 KiB queue
    /// temporary on the caller's stack (a debug-build `x = Queue::new()`
    /// spills the whole struct, which overflowed the 16 KiB kernel stack).
    fn clear(&mut self) {
        self.head = 0;
        self.count = 0;
    }

    /// Pop the first datagram for `dst_port`, copying at most `out.len()`
    /// payload bytes into `out` and returning `(source, original_len)`.
    ///
    /// This is the receive path's no-large-stack-copy shape: the queue slot is
    /// read in place, and the caller only needs a buffer as large as the bytes
    /// it is willing to accept, not a [`Datagram`] temporary.
    fn pop_into(&mut self, dst_port: u16, out: &mut [u8]) -> Option<(Ipv4, u16, usize)> {
        for index in 0..self.count {
            let slot = (self.head + index) % UDP_SLOTS;
            let candidate = &self.slots[slot];
            if candidate.dst_port != dst_port {
                continue;
            }
            let take = candidate.len.min(out.len());
            out[..take].copy_from_slice(&candidate.data[..take]);
            let source = (candidate.src_ip, candidate.src_port, candidate.len);
            for offset in index..self.count - 1 {
                let from = (self.head + offset + 1) % UDP_SLOTS;
                let to = (self.head + offset) % UDP_SLOTS;
                self.slots[to] = self.slots[from];
            }
            self.count -= 1;
            return Some(source);
        }
        None
    }

    fn pop_matching(
        &mut self,
        dst_port: u16,
        src_ip: Option<Ipv4>,
        src_port: Option<u16>,
    ) -> Option<Datagram> {
        for index in 0..self.count {
            let slot = (self.head + index) % UDP_SLOTS;
            let candidate = &self.slots[slot];
            if candidate.dst_port != dst_port {
                continue;
            }
            if let Some(ip) = src_ip {
                if candidate.src_ip != ip {
                    continue;
                }
            }
            if let Some(port) = src_port {
                if candidate.src_port != port {
                    continue;
                }
            }
            let datagram = *candidate;
            // Shift the tail down over the hole.
            for offset in index..self.count - 1 {
                let from = (self.head + offset + 1) % UDP_SLOTS;
                let to = (self.head + offset) % UDP_SLOTS;
                self.slots[to] = self.slots[from];
            }
            self.count -= 1;
            return Some(datagram);
        }
        None
    }
}

/// One userland port binding and the datagrams received for it.
///
/// The queue is per port, so a datagram for port A can never be consumed by a
/// socket bound to port B, and a full queue drops only that port's oldest
/// datagram. `handle_udp` fills it from the poll path; `udp_take` drains it.
#[derive(Clone, Copy)]
struct UdpBinding {
    used: bool,
    port: u16,
    queue: DatagramQueue,
}

impl UdpBinding {
    const fn empty() -> Self {
        UdpBinding {
            used: false,
            port: 0,
            queue: DatagramQueue::new(),
        }
    }
}

/// Why `udp_bind` refused a port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UdpBindError {
    /// Another socket already owns the port.
    AddressInUse,
    /// All [`UDP_BINDINGS`] slots are taken (or no ephemeral port is free).
    NoSlots,
}

static UDP_BOUND: Spinlock<[UdpBinding; UDP_BINDINGS]> =
    Spinlock::new([UdpBinding::empty(); UDP_BINDINGS]);

/// ARP cache entry.
#[derive(Clone, Copy)]
struct ArpEntry {
    ip: Ipv4,
    mac: Mac,
    seen_ticks: u64,
    valid: bool,
}

impl ArpEntry {
    const EMPTY: Self = ArpEntry {
        ip: [0; 4],
        mac: [0; 6],
        seen_ticks: 0,
        valid: false,
    };
}

static CONFIG: Spinlock<Option<Config>> = Spinlock::new(None);
static ARP: Spinlock<[ArpEntry; ARP_ENTRIES]> = Spinlock::new([ArpEntry::EMPTY; ARP_ENTRIES]);
static UDP_RX: Spinlock<DatagramQueue> = Spinlock::new(DatagramQueue::new());
static ICMP_RX: Spinlock<[EchoReply; ICMP_SLOTS]> = Spinlock::new(
    [EchoReply {
        src: [0; 4],
        id: 0,
        sequence: 0,
        data: [0; ICMP_DATA_MAX],
    }; ICMP_SLOTS],
);
static ICMP_COUNT: Spinlock<usize> = Spinlock::new(0);

/// The stack's own MAC address, or `None` when no NIC was found.
pub fn mac() -> Option<Mac> {
    virtio_net::mac()
}

/// The current interface configuration, if DHCP has completed.
pub fn config() -> Option<Config> {
    *CONFIG.lock()
}

/// Bring up the NIC and run DHCP.
///
/// Called once from `kmain`, after the device tree, the timer and the block
/// device. No NIC is a log line, not an error: the kernel is still a kernel on a
/// machine without one. A NIC without a lease is also a log line — the shell's
/// `dhcp` command exists to retry.
pub fn init(tree: &Fdt) {
    virtio_net::init(tree);
    if !virtio_net::present() {
        crate::kprintln!("net         no network device");
        return;
    }
    match dhcp::acquire() {
        Ok(config) => {
            // `acquire` only returns a lease; installing it is the caller's job so
            // that a failed renewal can keep the old one (see the shell's `dhcp`).
            dhcp::install(config);
            report(&config);
        }
        Err(error) => crate::kprintln!("net         DHCP failed: {error}"),
    }
}

/// Print the lease in the one-line form the smoke test greps.
fn report(config: &Config) {
    let mut ip = [0u8; 15];
    let mut mask = [0u8; 15];
    let mut gateway = [0u8; 15];
    let mut dns = [0u8; 15];
    let ip_s =
        format_ipv4(config.ip, &mut ip).map(|len| core::str::from_utf8(&ip[..len]).unwrap_or("?"));
    let mask_s = format_ipv4(config.mask, &mut mask)
        .map(|len| core::str::from_utf8(&mask[..len]).unwrap_or("?"));
    let gw_s = format_ipv4(config.gateway, &mut gateway)
        .map(|len| core::str::from_utf8(&gateway[..len]).unwrap_or("?"));
    let dns_s = format_ipv4(config.dns, &mut dns)
        .map(|len| core::str::from_utf8(&dns[..len]).unwrap_or("?"));
    match (ip_s, mask_s, gw_s, dns_s) {
        (Some(ip), Some(mask), Some(gateway), Some(dns)) => crate::kprintln!(
            "net         DHCP ip {ip}/{mask} gw {gateway} dns {dns} lease {} s",
            config.lease_seconds
        ),
        _ => crate::kprintln!("net         DHCP lease acquired (formatting failed)"),
    }
}

// ── polling ────────────────────────────────────────────────────────────────

/// Drain every received frame and dispatch it.
pub fn poll() {
    let mut frame = [0u8; MAX_FRAME];
    while let Some(len) = virtio_net::recv(&mut frame) {
        handle_frame(&frame[..len]);
    }
}

/// Dispatch one Ethernet frame.
fn handle_frame(frame: &[u8]) {
    let Some((ethernet, payload_offset)) = parse_ethernet(frame) else {
        return;
    };
    let Some(our_mac) = virtio_net::mac() else {
        return;
    };
    if ethernet.dst != our_mac && !mac_is_broadcast(ethernet.dst) && !mac_is_group(ethernet.dst) {
        return;
    }
    match ethernet.ethertype {
        ETH_ARP => handle_arp(frame, payload_offset),
        ETH_IPV4 => handle_ipv4(frame, payload_offset),
        _ => {}
    }
}

fn handle_arp(frame: &[u8], offset: usize) {
    let Some(body) = frame.get(offset..) else {
        return;
    };
    let Some(arp) = parse_arp(body) else {
        return;
    };
    arp_learn(arp.sender_ip, arp.sender_mac);
    if arp.operation != ARP_REQUEST {
        return;
    }
    let Some(config) = config() else {
        return;
    };
    if arp.target_ip != config.ip {
        return;
    }
    let Some(our_mac) = virtio_net::mac() else {
        return;
    };
    let mut out = [0u8; ETH_HEADER + 28];
    if let Some(len) = build_arp_frame(
        &mut out,
        arp.sender_mac,
        our_mac,
        ARP_REPLY,
        our_mac,
        config.ip,
        arp.sender_mac,
        arp.sender_ip,
    ) {
        let _ = virtio_net::send(&out[..len]);
    }
}

fn handle_ipv4(frame: &[u8], offset: usize) {
    let Some(packet) = parse_ipv4(frame, offset) else {
        return;
    };
    let config = config();
    // Only frames addressed to this interface (or a broadcast) belong to this
    // stack; the machine does not forward packets. Before DHCP completes there
    // is no address yet, but the DHCP replies arrive as limited broadcasts and
    // are exactly what the stack is waiting for.
    let for_us = match config {
        Some(config) => packet.dst == config.ip || is_limited_broadcast(packet.dst),
        None => is_limited_broadcast(packet.dst),
    };
    if !for_us {
        return;
    }
    let Some(payload) =
        frame.get(packet.payload_offset..packet.payload_offset + packet.payload_len)
    else {
        return;
    };
    match packet.protocol {
        IP_ICMP => {
            if let Some(config) = config {
                handle_icmp(packet.src, config.ip, payload);
            }
        }
        IP_UDP => handle_udp(packet.src, packet.dst, payload),
        IP_TCP => tcp::on_segment(packet.src, packet.dst, payload),
        _ => {}
    }
}

fn handle_icmp(src: Ipv4, dst: Ipv4, payload: &[u8]) {
    let Some(echo) = parse_icmp_echo(payload, 0) else {
        return;
    };
    match echo.kind {
        ICMP_ECHO_REQUEST => {
            let data = payload
                .get(echo.data_offset..echo.data_offset + echo.data_len)
                .unwrap_or(&[]);
            let mut message = [0u8; ICMP_ECHO_HEADER + 128];
            let len = match build_icmp_echo(
                &mut message,
                0,
                ICMP_ECHO_REPLY,
                echo.id,
                echo.sequence,
                data,
            ) {
                Some(len) => len,
                None => return,
            };
            let _ = send_ipv4_to(dst, src, IP_ICMP, len as u16, |frame| {
                frame.get_mut(..len)?.copy_from_slice(&message[..len]);
                Some(len)
            });
        }
        ICMP_ECHO_REPLY => {
            let data_len = echo.data_len.min(ICMP_DATA_MAX);
            let data = payload
                .get(echo.data_offset..echo.data_offset + data_len)
                .unwrap_or(&[]);
            let mut entry = EchoReply {
                src,
                id: echo.id,
                sequence: echo.sequence,
                data: [0; ICMP_DATA_MAX],
            };
            entry.data[..data_len].copy_from_slice(data);
            push_echo(entry);
        }
        _ => {}
    }
}

fn push_echo(reply: EchoReply) {
    let mut count = ICMP_COUNT.lock();
    let slot = *count % ICMP_SLOTS;
    ICMP_RX.lock()[slot] = reply;
    *count = count.wrapping_add(1);
}

fn handle_udp(src_ip: Ipv4, dst_ip: Ipv4, segment: &[u8]) {
    if !udp_checksum_ok(src_ip, dst_ip, segment) {
        return;
    }
    let Some(udp) = parse_udp(segment) else {
        return;
    };
    let Some(payload) = segment.get(udp.payload_offset..udp.payload_offset + udp.payload_len)
    else {
        return;
    };
    let keep = payload.len().min(UDP_PAYLOAD_MAX);
    let mut datagram = Datagram {
        src_ip,
        src_port: udp.src_port,
        dst_port: udp.dst_port,
        len: keep,
        data: [0; UDP_PAYLOAD_MAX],
    };
    datagram.data[..keep].copy_from_slice(&payload[..keep]);
    // The kernel's own clients (DHCP, DNS) keep their queue; a userspace
    // binding gets its own copy, so draining one never steals from the other.
    UDP_RX.lock().push(datagram);
    let mut bindings = UDP_BOUND.lock();
    for binding in bindings.iter_mut() {
        if binding.used && binding.port == datagram.dst_port {
            binding.queue.push(datagram);
        }
    }
}

/// Bind a userland UDP port.
///
/// `port == 0` allocates the first free port in
/// [`UDP_EPHEMERAL_FIRST`]..=[`UDP_EPHEMERAL_LAST`]; an explicit port is
/// refused with [`UdpBindError::AddressInUse`] when another binding owns it.
/// The binding starts with an empty queue, so a port reused after `close`
/// cannot deliver a datagram addressed to its previous owner.
pub fn udp_bind(port: u16) -> Result<u16, UdpBindError> {
    let mut bindings = UDP_BOUND.lock();
    let free = bindings.iter().position(|binding| !binding.used);
    let Some(slot) = free else {
        return Err(UdpBindError::NoSlots);
    };
    let port = if port != 0 {
        if bindings
            .iter()
            .any(|binding| binding.used && binding.port == port)
        {
            return Err(UdpBindError::AddressInUse);
        }
        port
    } else {
        let mut candidate = UDP_EPHEMERAL_FIRST;
        loop {
            if !bindings
                .iter()
                .any(|binding| binding.used && binding.port == candidate)
            {
                break candidate;
            }
            if candidate == UDP_EPHEMERAL_LAST {
                return Err(UdpBindError::NoSlots);
            }
            candidate = candidate.saturating_add(1);
        }
    };
    let binding = &mut bindings[slot];
    binding.used = true;
    binding.port = port;
    binding.queue.clear();
    Ok(port)
}

/// Release a userland UDP port. Unknown ports are ignored: `close` may race a
/// process teardown, and there is no state to corrupt either way.
pub fn udp_unbind(port: u16) {
    let mut bindings = UDP_BOUND.lock();
    for binding in bindings.iter_mut() {
        if binding.used && binding.port == port {
            binding.used = false;
            binding.queue.clear();
            return;
        }
    }
}

/// Pop one datagram received on a bound port, without polling the NIC.
pub fn udp_take(port: u16) -> Option<Datagram> {
    let mut bindings = UDP_BOUND.lock();
    for binding in bindings.iter_mut() {
        if binding.used && binding.port == port {
            return binding.queue.pop_matching(port, None, None);
        }
    }
    None
}

/// Pop one datagram into `out`, returning `(source address, source port,
/// original payload length)`. The copy is bounded by `out.len()`; a larger
/// datagram is truncated exactly as `recvfrom` would truncate it.
pub fn udp_take_into(port: u16, out: &mut [u8]) -> Option<(Ipv4, u16, usize)> {
    let mut bindings = UDP_BOUND.lock();
    for binding in bindings.iter_mut() {
        if binding.used && binding.port == port {
            return binding.queue.pop_into(port, out);
        }
    }
    None
}

/// Wait for a datagram and copy it into `out`; the no-large-temporary receive
/// path used by `recvfrom`. Same bounded-poll contract as the other waits.
pub fn udp_wait_into(port: u16, out: &mut [u8], budget_ticks: u64) -> Option<(Ipv4, u16, usize)> {
    let deadline = timer::ticks().saturating_add(budget_ticks);
    let mut iterations = 0u64;
    loop {
        poll();
        if let Some(datagram) = udp_take_into(port, out) {
            return Some(datagram);
        }
        if budget_ticks == 0 || timer::ticks() >= deadline || iterations >= POLL_ITERATIONS {
            return None;
        }
        iterations += 1;
        core::hint::spin_loop();
    }
}

/// Whether a bound port has a datagram queued (the `poll(POLLIN)` half).
pub fn udp_pending(port: u16) -> bool {
    let bindings = UDP_BOUND.lock();
    bindings
        .iter()
        .any(|binding| binding.used && binding.port == port && !binding.queue.is_empty())
}

/// Send a userland UDP datagram from the lease address.
///
/// `false` means "could not send": no DHCP lease, an ARP/next-hop failure, or
/// a datagram that does not fit one frame. The caller maps it to the closest
/// ABI errno; there is no partial datagram.
pub fn udp_send_from(src_port: u16, dst_ip: Ipv4, dst_port: u16, payload: &[u8]) -> bool {
    let Some(config) = config() else {
        return false;
    };
    udp_send_raw(config.ip, dst_ip, src_port, dst_port, payload)
}

/// Wait for a datagram addressed to `dst_port` (and, when given, from a specific
/// sender) for at most `budget_ticks`, polling the NIC while waiting.
pub fn wait_udp(
    dst_port: u16,
    src_ip: Option<Ipv4>,
    src_port: Option<u16>,
    budget_ticks: u64,
) -> Option<Datagram> {
    let deadline = timer::ticks().saturating_add(budget_ticks);
    let mut iterations = 0u64;
    loop {
        poll();
        if let Some(datagram) = UDP_RX.lock().pop_matching(dst_port, src_ip, src_port) {
            return Some(datagram);
        }
        if timer::ticks() >= deadline || iterations >= POLL_ITERATIONS {
            return None;
        }
        iterations += 1;
        core::hint::spin_loop();
    }
}

// ── ARP / next hop ─────────────────────────────────────────────────────────

fn arp_learn(ip: Ipv4, mac: Mac) {
    if ip == [0; 4] || mac_is_group(mac) {
        return;
    }
    let now = timer::ticks();
    let mut cache = ARP.lock();
    let mut free = 0usize;
    for (index, entry) in cache.iter_mut().enumerate() {
        if entry.valid && entry.ip == ip {
            entry.mac = mac;
            entry.seen_ticks = now;
            return;
        }
        if !entry.valid {
            free = index;
        }
    }
    cache[free] = ArpEntry {
        ip,
        mac,
        seen_ticks: now,
        valid: true,
    };
}

/// Snapshot the live ARP cache: `(ip, mac)` for every entry the cache still
/// considers fresh (inside [`ARP_LIFETIME_TICKS`]), in table order.
///
/// Writes at most `out.len()` entries and returns the number written. Stale
/// entries are skipped, not returned: `arp_lookup` ignores them, so they are not
/// mappings a reader could use, and `/proc/net/arp` must not print a mapping the
/// stack itself would not resolve.
pub fn arp_snapshot(out: &mut [(Ipv4, Mac)]) -> usize {
    let now = timer::ticks();
    let cache = ARP.lock();
    let mut written = 0usize;
    for entry in cache.iter() {
        if !entry.valid || now.saturating_sub(entry.seen_ticks) >= ARP_LIFETIME_TICKS {
            continue;
        }
        if written == out.len() {
            break;
        }
        out[written] = (entry.ip, entry.mac);
        written += 1;
    }
    written
}

fn arp_lookup(ip: Ipv4) -> Option<Mac> {
    let now = timer::ticks();
    let cache = ARP.lock();
    for entry in cache.iter() {
        if entry.valid
            && entry.ip == ip
            && now.saturating_sub(entry.seen_ticks) < ARP_LIFETIME_TICKS
        {
            return Some(entry.mac);
        }
    }
    None
}

/// Resolve `ip` to a MAC address, sending an ARP request and polling for the
/// reply when the cache does not know it.
fn arp_resolve(ip: Ipv4) -> Option<Mac> {
    if let Some(mac) = arp_lookup(ip) {
        return Some(mac);
    }
    let our_mac = virtio_net::mac()?;
    let config = config()?;
    let mut frame = [0u8; ETH_HEADER + 28];
    let len = build_arp_frame(
        &mut frame,
        [0xff; 6],
        our_mac,
        ARP_REQUEST,
        our_mac,
        config.ip,
        [0; 6],
        ip,
    )?;
    if !virtio_net::send(&frame[..len]) {
        return None;
    }
    let deadline = timer::ticks().saturating_add(ARP_WAIT_TICKS);
    let mut iterations = 0u64;
    loop {
        poll();
        if let Some(mac) = arp_lookup(ip) {
            return Some(mac);
        }
        if timer::ticks() >= deadline || iterations >= POLL_ITERATIONS {
            return None;
        }
        iterations += 1;
        core::hint::spin_loop();
    }
}

/// The MAC that should receive a frame for `dst_ip`: the destination itself on
/// the local subnet, the gateway otherwise.
fn next_hop(dst_ip: Ipv4) -> Option<Mac> {
    let config = config()?;
    if is_limited_broadcast(dst_ip) {
        return Some([0xff; 6]);
    }
    if wire::same_subnet(config.ip, config.mask, dst_ip) {
        arp_resolve(dst_ip)
    } else {
        arp_resolve(config.gateway)
    }
}

// ── sending ────────────────────────────────────────────────────────────────

/// Build and send an IPv4 packet: `payload` writes into the frame's payload area
/// and returns its length.
fn send_ipv4_to<F>(src_ip: Ipv4, dst_ip: Ipv4, protocol: u8, id: u16, payload: F) -> bool
where
    F: FnMut(&mut [u8]) -> Option<usize>,
{
    let Some(dst_mac) = next_hop(dst_ip) else {
        return false;
    };
    send_ipv4_raw(src_ip, dst_ip, dst_mac, protocol, id, payload)
}

/// As [`send_ipv4_to`], but the caller names the destination MAC (DHCP has no
/// ARP entry to resolve, and broadcasts do not need one).
fn send_ipv4_raw<F>(
    src_ip: Ipv4,
    dst_ip: Ipv4,
    dst_mac: Mac,
    protocol: u8,
    id: u16,
    mut payload: F,
) -> bool
where
    F: FnMut(&mut [u8]) -> Option<usize>,
{
    let Some(our_mac) = virtio_net::mac() else {
        return false;
    };
    let mut frame = [0u8; MAX_FRAME];
    let payload_offset = ETH_HEADER + IPV4_HEADER;
    let payload_len = match payload(&mut frame[payload_offset..]) {
        Some(len) => len,
        None => return false,
    };
    let Some(_) = build_ipv4(
        &mut frame,
        ETH_HEADER,
        src_ip,
        dst_ip,
        protocol,
        payload_len,
        id,
        64,
    ) else {
        return false;
    };
    let Some(_) = build_ethernet(&mut frame, dst_mac, our_mac, ETH_IPV4) else {
        return false;
    };
    virtio_net::send(&frame[..payload_offset + payload_len])
}

/// Send a UDP datagram with explicit source address and destination (DHCP uses
/// `0.0.0.0`→`255.255.255.255`; DNS uses the lease address).
pub fn udp_send_raw(
    src_ip: Ipv4,
    dst_ip: Ipv4,
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
) -> bool {
    let broadcast = is_limited_broadcast(dst_ip);
    let dst_mac = if broadcast {
        [0xff; 6]
    } else {
        match next_hop(dst_ip) {
            Some(mac) => mac,
            None => return false,
        }
    };
    send_ipv4_raw(src_ip, dst_ip, dst_mac, IP_UDP, 0x4d37, |frame| {
        wire::build_udp(frame, 0, src_ip, dst_ip, src_port, dst_port, payload)
    })
}

/// Send one ICMP echo request; `false` when the packet could not be built or the
/// first hop is unreachable.
fn send_echo_request(dst_ip: Ipv4, id: u16, sequence: u16, data: &[u8]) -> bool {
    let Some(config) = config() else {
        return false;
    };
    send_ipv4_to(config.ip, dst_ip, IP_ICMP, 0x4943, |frame| {
        build_icmp_echo(frame, 0, ICMP_ECHO_REQUEST, id, sequence, data)
    })
}

/// Send `count` echo requests to `dst_ip`, returning the round-trip time of each
/// reply in 10 ms ticks (`None` entries are lost probes).
pub fn ping(dst_ip: Ipv4, count: u8) -> [Option<u32>; 4] {
    let mut results = [None; 4];
    let probe = [0x50u8, 0x41, 0x47, 0x48]; // "PAGH"
    for index in 0..count.min(4) {
        let sequence = index as u16 + 1;
        let start = timer::ticks();
        if !send_echo_request(dst_ip, PING_ID, sequence, &probe) {
            continue;
        }
        let deadline = timer::ticks().saturating_add(100); // 1 s per probe
        let mut iterations = 0u64;
        loop {
            poll();
            if let Some(reply) = take_echo(PING_ID, sequence, dst_ip) {
                let elapsed = timer::ticks().saturating_sub(start);
                results[index as usize] = Some(elapsed as u32);
                let _ = reply;
                break;
            }
            if timer::ticks() >= deadline || iterations >= POLL_ITERATIONS {
                break;
            }
            iterations += 1;
            core::hint::spin_loop();
        }
    }
    results
}

/// Pop a matching echo reply from the reply ring.
fn take_echo(id: u16, sequence: u16, src_ip: Ipv4) -> Option<EchoReply> {
    let mut replies = ICMP_RX.lock();
    for index in 0..ICMP_SLOTS {
        let reply = replies[index];
        if reply.id == id && reply.sequence == sequence && reply.src == src_ip {
            replies[index].id = 0; // mark consumed
            return Some(reply);
        }
    }
    None
}

/// Resolve a host name through the leased DNS server.
pub fn resolve_name(name: &str) -> Result<Ipv4, dns::Error> {
    dns::resolve(name)
}

/// A one-line description for the shell's `net` command.
pub struct Status {
    /// Interface MAC.
    pub mac: Mac,
    /// DHCP lease.
    pub config: Config,
}

/// The current MAC + lease, if the interface is up.
pub fn status() -> Option<Status> {
    let mac = virtio_net::mac()?;
    let config = config()?;
    Some(Status { mac, config })
}
