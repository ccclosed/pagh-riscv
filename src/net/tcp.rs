//! A small TCP implementation: one active-open client plus a passive-open
//! listener with a bounded accept queue (RFC 793 subset).
//!
//! This is deliberately not a general TCP stack. It has a fixed table of
//! connections, it assumes in-order delivery with no loss on the path (QEMU's
//! `user` network is a local host stack; a real link would need retransmission
//! timers and out-of-order reassembly), and its advertised window is exactly
//! the free space in each 4 KiB receive buffer. What it does implement
//! correctly is the part a client or a small server needs: the three-way
//! handshake, sequence/acknowledgement tracking, payload delivery to the
//! caller, a clean FIN from the peer, a bounded wait for every step, and a
//! `listen`/`accept` pair whose handshaken connections wait in a queue.
//!
//! The window matters for a package download. A receiver that always advertises
//! the full buffer lets the peer send bytes it cannot store; those bytes are
//! dropped, the sender waits for a retransmission timer, and a 13 KiB download
//! stalls after the first 4 KiB. Every ACK here carries the *actual* free
//! space, and [`recv_nonblocking`] re-opens the window after the caller drains
//! the buffer.
//!
//! # Shape
//!
//! One static [`TcpState`] guarded by a spinlock owns the connection table (one
//! active-open client plus passive connections), the single listener and its
//! accept queue. [`on_segment`] is called from the poll loop (no lock held by
//! the caller); it updates the state and then sends any acknowledgement *after*
//! releasing the lock, so the send path may itself poll and re-enter
//! [`on_segment`] without deadlocking. For the same reason `bind`/`listen`/
//! `accept`/`send`/`close` never call the NIC under the state lock.
//!
//! The old one-connection API ([`connect`], [`send`], [`recv`],
//! [`recv_nonblocking`], [`peer`], [`close`], [`reset`]) still names the
//! active-open client and is what the shell's `http` command and the package
//! installer use. The `_id` variants address any connection in the table and
//! are what the socket syscalls use once an accepted descriptor exists.
//!
//! # Limits (exact)
//!
//! * [`CONNECTIONS`] slots total. The active client takes at most one; passive
//!   connections (queued or accepted) take the rest.
//! * one listener, one bind reservation at a time;
//! * an accept queue of [`ACCEPT_QUEUE`] entries, further capped by the
//!   `listen` backlog (`1..=ACCEPT_QUEUE`);
//! * 4 KiB of receive buffer per connection;
//! * no out-of-order reassembly, congestion control, keepalive, TIME_WAIT,
//!   half-close or retransmission timer beyond the bounded wait in each call.

use super::wire::{self, Ipv4, TCP_ACK, TCP_FIN, TCP_PSH, TCP_RST, TCP_SYN};
use super::{config, poll};
use crate::arch::riscv64::timer;
use crate::sync::Spinlock;
use core::sync::atomic::{AtomicU16, Ordering};

/// First and last local port the stack hands out (for an active connection or
/// `bind(0)`). A *fresh* port per connection (not one fixed port) is what keeps
/// a second download after a close from landing on a 4-tuple the host stack
/// still holds in TIME_WAIT: with the old fixed port, QEMU's user-network layer
/// silently dropped the new SYN and the package install timed out before the
/// server saw a request.
const LOCAL_PORT_FIRST: u16 = 49152;
const LOCAL_PORT_LAST: u16 = 60999;
static NEXT_LOCAL_PORT: AtomicU16 = AtomicU16::new(LOCAL_PORT_FIRST);

/// Connection slots. One is enough for the active-open client, so a server can
/// hold seven accepted/queued connections; the fixture only needs one, and the
/// accept queue is smaller than this by design.
pub const CONNECTIONS: usize = 8;
/// Entries the accept queue can hold (also the hard cap on `listen`'s backlog).
pub const ACCEPT_QUEUE: usize = 4;

/// Receive window when the buffer is empty (also the SYN window).
const WINDOW: u16 = 4096;
/// Receive buffer for one connection.
const RX_CAPACITY: usize = 4096;
/// Payload bytes per segment we send; small enough for one Ethernet frame.
const MSS: usize = 1000;
/// Ticks the active handshake may take per SYN attempt (3 s).
const CONNECT_TICKS: u64 = 300;
/// Ticks a data step may take (10 s). A debug kernel writes the received bytes
/// to ext2 between polls, so the NIC's small RX ring can drop frames; the
/// sender then recovers on its retransmission timers, and the first few of
/// those (200 ms, 400 ms, …) must fit inside one bounded step.
const DATA_TICKS: u64 = 1000;
/// Ticks a blocking `accept` may wait for a handshaken connection (30 s). The
/// syscall maps the expiry to `-EAGAIN`, the same contract a non-blocking
/// listener gets immediately.
pub const ACCEPT_TICKS: u64 = 3000;
/// Poll iterations allowed **while the tick clock is not advancing**. The cap
/// exists only so a machine whose timer never fires cannot spin forever; when
/// ticks do advance, the tick deadline is the bound. Counting every iteration
/// instead made the data timeout shorter than the 10 s it claims: the release
/// kernel polled 8 M times in under 3 s and abandoned a download whose retrans-
/// mission was still in flight.
const STEP_ITERATIONS: u64 = 2_000_000;

/// Connection state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Slot is allocated but the connection is shut down (tombstone) or the
    /// slot is free; [`Connection::in_use`] distinguishes the two.
    Closed,
    /// Active open: SYN sent, waiting for SYN+ACK.
    SynSent,
    /// Passive open: SYN+ACK sent, waiting for the handshake's third ACK.
    SynReceived,
    /// Handshake done, data may flow.
    Established,
    /// The peer sent FIN; the local side may still read buffered data.
    Closing,
    /// The connection is gone (RST or a protocol error).
    Reset,
}

/// One TCP connection. The receive buffer dominates the size; a slot is never
/// moved by value after startup (the table is static).
struct Connection {
    in_use: bool,
    state: State,
    /// Opened passively (allocated by a SYN to the listener, not `connect`).
    passive: bool,
    /// Waiting in the listener's accept queue.
    queued: bool,
    /// Popped by `accept`; the listener's teardown must leave it alone.
    accepted: bool,
    local_port: u16,
    remote: Ipv4,
    remote_port: u16,
    /// Next sequence number we will send.
    sequence: u32,
    /// Next sequence number we expect from the peer.
    acknowledgement: u32,
    /// Highest sequence number the peer has acknowledged for our data.
    acked: u32,
    /// Peer's advertised window (we never exceed it by more than one segment).
    peer_window: u16,
    rx: [u8; RX_CAPACITY],
    rx_len: usize,
}

impl Connection {
    const EMPTY: Self = Connection {
        in_use: false,
        state: State::Closed,
        passive: false,
        queued: false,
        accepted: false,
        local_port: 0,
        remote: [0; 4],
        remote_port: 0,
        sequence: 0,
        acknowledgement: 0,
        acked: 0,
        peer_window: 0,
        rx: [0; RX_CAPACITY],
        rx_len: 0,
    };

    /// Release the slot without copying the 4 KiB receive buffer around (a
    /// debug-build struct assignment would put a 4 KiB temporary on the 16 KiB
    /// kernel stack).
    fn clear(&mut self) {
        self.in_use = false;
        self.state = State::Closed;
        self.passive = false;
        self.queued = false;
        self.accepted = false;
        self.local_port = 0;
        self.remote = [0; 4];
        self.remote_port = 0;
        self.sequence = 0;
        self.acknowledgement = 0;
        self.acked = 0;
        self.peer_window = 0;
        self.rx_len = 0;
    }
}

/// The one listener. `bound` is set by `bind` (and reserves the port) before
/// `listen` flips `active`; the queue holds connection indices, not copies.
struct Listener {
    bound: Option<u16>,
    active: bool,
    backlog: usize,
    queue: [u8; ACCEPT_QUEUE],
    queue_len: usize,
}

impl Listener {
    const EMPTY: Self = Listener {
        bound: None,
        active: false,
        backlog: 0,
        queue: [0; ACCEPT_QUEUE],
        queue_len: 0,
    };

    fn enqueue(&mut self, id: usize) -> bool {
        if !self.active || self.queue_len >= self.backlog || self.queue_len >= ACCEPT_QUEUE {
            return false;
        }
        self.queue[self.queue_len] = id as u8;
        self.queue_len += 1;
        true
    }

    fn remove(&mut self, id: usize) {
        let mut found = None;
        for index in 0..self.queue_len {
            if self.queue[index] as usize == id {
                found = Some(index);
                break;
            }
        }
        let Some(index) = found else { return };
        for slot in index..self.queue_len.saturating_sub(1) {
            self.queue[slot] = self.queue[slot + 1];
        }
        self.queue_len = self.queue_len.saturating_sub(1);
    }
}

/// All TCP state under one lock: the connection table, the active client's
/// slot and the listener. One lock keeps a transition (handshake, accept,
/// teardown) atomic; the send path runs only after it is released.
struct TcpState {
    connections: [Connection; CONNECTIONS],
    active: Option<u8>,
    listener: Listener,
}

impl TcpState {
    const EMPTY: Self = TcpState {
        connections: [const { Connection::EMPTY }; CONNECTIONS],
        active: None,
        listener: Listener::EMPTY,
    };

    fn find(&self, remote: Ipv4, local_port: u16, remote_port: u16) -> Option<usize> {
        self.connections.iter().position(|connection| {
            connection.in_use
                && connection.local_port == local_port
                && connection.remote == remote
                && connection.remote_port == remote_port
        })
    }

    fn free_slot(&self) -> Option<usize> {
        self.connections
            .iter()
            .position(|connection| !connection.in_use)
    }

    /// Whether `port` is already named by any live connection or reserved by
    /// the listener.
    fn port_in_use(&self, port: u16) -> bool {
        if self.listener.bound == Some(port) {
            return true;
        }
        self.connections
            .iter()
            .any(|connection| connection.in_use && connection.local_port == port)
    }
}

static STATE: Spinlock<TcpState> = Spinlock::new(TcpState::EMPTY);

/// Why a connection step failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// No DHCP lease, so no source address.
    NoLease,
    /// Another connection is already open in the one active-client slot.
    Busy,
    /// The handshake or a data exchange timed out.
    Timeout,
    /// The peer reset the connection.
    Reset,
    /// The peer closed before the operation finished.
    Closed,
    /// A segment could not be built or sent.
    Send,
    /// A non-blocking receive/accept found nothing yet.
    WouldBlock,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::NoLease => f.write_str("no DHCP lease"),
            Error::Busy => f.write_str("no free TCP connection slot"),
            Error::Timeout => f.write_str("TCP timeout"),
            Error::Reset => f.write_str("connection reset"),
            Error::Closed => f.write_str("connection closed"),
            Error::Send => f.write_str("the segment could not be sent"),
            Error::WouldBlock => f.write_str("no connection queued yet"),
        }
    }
}

/// Why `bind_port` refused a port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindError {
    /// The port is already named by the listener or a live connection.
    AddressInUse,
    /// No ephemeral port is free.
    NoPorts,
    /// `listen` was called for a port the socket did not bind.
    NotBound,
}

/// The window to advertise for a buffer holding `used` bytes.
fn window_for(used: usize) -> u16 {
    RX_CAPACITY.saturating_sub(used).min(u16::MAX as usize) as u16
}

/// A monotonically changing initial sequence number.
fn initial_sequence() -> u32 {
    (timer::ticks() as u32).wrapping_mul(2654435761) ^ 0x5041_4748
}

/// A segment the caller must send after releasing the state lock.
#[derive(Clone, Copy)]
struct PendingSegment {
    local_port: u16,
    remote: Ipv4,
    remote_port: u16,
    sequence: u32,
    acknowledgement: u32,
    flags: u16,
    window: u16,
}

impl PendingSegment {
    /// Send the segment. Must be called without [`STATE`] held: `next_hop` may
    /// poll the NIC, and the poll path re-enters [`on_segment`].
    fn emit(self, payload: &[u8]) -> bool {
        let Some(config) = config() else {
            return false;
        };
        let Some(mac) = super::next_hop(self.remote) else {
            return false;
        };
        super::send_ipv4_raw(config.ip, self.remote, mac, wire::IP_TCP, 0x5443, |frame| {
            wire::build_tcp(
                frame,
                0,
                config.ip,
                self.remote,
                self.local_port,
                self.remote_port,
                self.sequence,
                self.acknowledgement,
                self.flags,
                self.window,
                payload,
            )
        })
    }
}

/// The acknowledgement (or reset) for `connection` right now, with its current
/// window.
fn segment_for(connection: &Connection, flags: u16) -> PendingSegment {
    PendingSegment {
        local_port: connection.local_port,
        remote: connection.remote,
        remote_port: connection.remote_port,
        sequence: connection.sequence,
        acknowledgement: connection.acknowledgement,
        flags,
        window: window_for(connection.rx_len),
    }
}

/// Copy the connection's endpoints out of the table (short lock, no nesting).
fn connection_endpoints(id: u8) -> Option<(u16, Ipv4, u16)> {
    let state = STATE.lock();
    let connection = state.connections.get(id as usize)?;
    if !connection.in_use {
        return None;
    }
    Some((
        connection.local_port,
        connection.remote,
        connection.remote_port,
    ))
}

/// Handle an incoming TCP segment. Called from the poll loop with no lock held.
pub fn on_segment(src: Ipv4, dst: Ipv4, segment: &[u8]) {
    if !wire::tcp_checksum_ok(src, dst, segment) {
        return;
    }
    let Some(tcp) = wire::parse_tcp(segment) else {
        return;
    };
    let payload = segment
        .get(tcp.payload_offset..tcp.payload_offset + tcp.payload_len)
        .unwrap_or(&[]);
    let mut pending: Option<PendingSegment> = None;
    {
        let mut state = STATE.lock();
        if let Some(index) = state.find(src, tcp.dst_port, tcp.src_port) {
            if tcp.flags & TCP_RST != 0 {
                // A reset queued for `accept` must not be handed to the
                // application: drop the entry and free the slot. A reset on an
                // accepted/active socket stays visible through `recv`.
                let queued = state.connections[index].queued;
                if queued {
                    state.listener.remove(index);
                    state.connections[index].queued = false;
                }
                let connection = &mut state.connections[index];
                if connection.passive && !connection.accepted {
                    connection.clear();
                } else {
                    connection.state = State::Reset;
                    connection.rx_len = 0;
                }
                return;
            }

            let mut queue_it = false;
            let mut reject = false;
            {
                let connection = &mut state.connections[index];
                connection.peer_window = tcp.window;

                if connection.state == State::SynSent {
                    // The peer acknowledges the SYN with `our_seq + 1` (the SYN
                    // consumes one sequence number); comparing against the SYN's
                    // own sequence is a classic off-by-one that keeps the
                    // connection in SynSent while the peer retransmits.
                    if tcp.flags & (TCP_SYN | TCP_ACK) == (TCP_SYN | TCP_ACK)
                        && tcp.acknowledgement == connection.sequence.wrapping_add(1)
                    {
                        connection.acknowledgement = tcp.sequence.wrapping_add(1);
                        connection.sequence = connection.sequence.wrapping_add(1);
                        connection.acked = connection.sequence;
                        connection.state = State::Established;
                        pending = Some(segment_for(connection, TCP_ACK));
                    }
                }

                if connection.state == State::SynReceived {
                    if tcp.flags & TCP_SYN != 0 && tcp.flags & TCP_ACK == 0 {
                        // A retransmitted SYN means our SYN+ACK was lost.
                        pending = Some(segment_for(connection, TCP_SYN | TCP_ACK));
                    } else if tcp.flags & TCP_ACK != 0
                        && tcp.acknowledgement == connection.sequence.wrapping_add(1)
                    {
                        connection.sequence = connection.sequence.wrapping_add(1);
                        connection.acked = connection.sequence;
                        connection.state = State::Established;
                        queue_it = true;
                    }
                }

                if matches!(connection.state, State::Established | State::Closing) {
                    if tcp.flags & TCP_ACK != 0
                        && (tcp.acknowledgement.wrapping_sub(connection.acked) as i32) > 0
                    {
                        connection.acked = tcp.acknowledgement;
                    }
                    if !payload.is_empty() {
                        if tcp.sequence == connection.acknowledgement {
                            let start = connection.rx_len;
                            let take = payload.len().min(RX_CAPACITY - start);
                            if take > 0 {
                                connection.rx[start..start + take]
                                    .copy_from_slice(&payload[..take]);
                                connection.rx_len = start + take;
                                connection.acknowledgement =
                                    connection.acknowledgement.wrapping_add(take as u32);
                            }
                            pending = Some(segment_for(connection, TCP_ACK));
                        } else {
                            // A gap (or a duplicate): repeat the ACK. Three of
                            // these are the sender's fast-retransmit trigger;
                            // staying silent makes every lost segment cost a
                            // full RTO.
                            pending = Some(segment_for(connection, TCP_ACK));
                        }
                    }
                    if tcp.flags & TCP_FIN != 0
                        && tcp.sequence.wrapping_add(payload.len() as u32)
                            == connection.acknowledgement
                    {
                        connection.acknowledgement = connection.acknowledgement.wrapping_add(1);
                        if connection.state == State::Established {
                            connection.state = State::Closing;
                        } else {
                            connection.state = State::Closed;
                        }
                        pending = Some(segment_for(connection, TCP_ACK));
                    }
                }
            }

            if queue_it {
                let queued = state.listener.enqueue(index);
                if queued {
                    state.connections[index].queued = true;
                } else {
                    reject = true;
                }
            }
            if reject {
                // The accept queue (backlog or the fixed table) is full; refuse
                // the handshaken connection instead of keeping state no one can
                // accept. The peer sees a reset and retries later.
                let connection = &mut state.connections[index];
                pending = Some(segment_for(connection, TCP_RST | TCP_ACK));
                connection.clear();
            }
        } else if state.listener.active
            && state.listener.bound == Some(tcp.dst_port)
            && tcp.flags & TCP_SYN != 0
            && tcp.flags & TCP_ACK == 0
        {
            if let Some(index) = state.free_slot() {
                let connection = &mut state.connections[index];
                connection.clear();
                connection.in_use = true;
                connection.state = State::SynReceived;
                connection.passive = true;
                connection.local_port = tcp.dst_port;
                connection.remote = src;
                connection.remote_port = tcp.src_port;
                connection.sequence = initial_sequence();
                connection.acknowledgement = tcp.sequence.wrapping_add(1);
                connection.acked = connection.sequence;
                connection.peer_window = tcp.window;
                pending = Some(segment_for(connection, TCP_SYN | TCP_ACK));
            }
        }
    }
    if let Some(segment) = pending {
        let _ = segment.emit(&[]);
    }
}

/// Wait for connection `id` to satisfy `done`, polling the NIC.
fn wait_for_id(id: u8, done: impl Fn(&Connection) -> bool, ticks: u64) -> bool {
    let start = timer::ticks();
    let deadline = start.saturating_add(ticks);
    let mut stalled = 0u64;
    loop {
        poll();
        {
            let state = STATE.lock();
            let Some(connection) = state.connections.get(id as usize) else {
                return false;
            };
            if !connection.in_use {
                return false;
            }
            if done(connection) {
                return true;
            }
            if connection.state == State::Reset || connection.state == State::Closed {
                return false;
            }
        }
        let now = timer::ticks();
        if now >= deadline {
            return false;
        }
        if now == start {
            stalled += 1;
            if stalled >= STEP_ITERATIONS {
                return false;
            }
        }
        core::hint::spin_loop();
    }
}

/// Choose an unused local port, skipping every named by a live connection or
/// reserved by the listener.
fn allocate_port(state: &TcpState) -> Option<u16> {
    let mut candidate = NEXT_LOCAL_PORT.fetch_add(1, Ordering::Relaxed);
    for _ in 0..=(LOCAL_PORT_LAST - LOCAL_PORT_FIRST) {
        if candidate < LOCAL_PORT_FIRST || candidate > LOCAL_PORT_LAST {
            candidate = LOCAL_PORT_FIRST;
        }
        if !state.port_in_use(candidate) {
            return Some(candidate);
        }
        candidate = candidate.saturating_add(1);
    }
    None
}

/// Open an active connection to `remote:port`, completing the three-way
/// handshake. `local` is a previously bound port (a client that bound before
/// connecting), or `None` to let the stack pick an ephemeral one.
pub fn connect_from(remote: Ipv4, port: u16, local: Option<u16>) -> Result<(), Error> {
    if config().is_none() {
        return Err(Error::NoLease);
    }
    let id = {
        let mut state = STATE.lock();
        if let Some(active) = state.active {
            if state
                .connections
                .get(active as usize)
                .map(|connection| connection.in_use)
                .unwrap_or(false)
            {
                return Err(Error::Busy);
            }
            state.active = None;
        }
        let Some(id) = state.free_slot() else {
            return Err(Error::Busy);
        };
        let local_port = match local {
            Some(port) => {
                // `bind` reserved the port in the listener slot; connecting
                // from it transfers the reservation to this connection.
                if state.listener.bound == Some(port) && !state.listener.active {
                    state.listener.bound = None;
                } else if state.port_in_use(port) {
                    return Err(Error::Busy);
                }
                port
            }
            None => match allocate_port(&state) {
                Some(port) => port,
                None => return Err(Error::Busy),
            },
        };
        let Some(connection) = state.connections.get_mut(id as usize) else {
            return Err(Error::Busy);
        };
        connection.clear();
        connection.in_use = true;
        connection.state = State::SynSent;
        connection.local_port = local_port;
        connection.remote = remote;
        connection.remote_port = port;
        connection.sequence = initial_sequence();
        connection.acknowledgement = 0;
        connection.acked = connection.sequence;
        connection.peer_window = WINDOW;
        state.active = Some(id as u8);
        id as u8
    };
    // Two SYN attempts: a lost SYN is the one retransmission a local host stack
    // can actually need.
    for _attempt in 0..2 {
        let sequence = {
            let state = STATE.lock();
            match state.connections.get(id as usize) {
                Some(connection) if connection.in_use && connection.state == State::SynSent => {
                    connection.sequence
                }
                _ => return Err(Error::Timeout),
            }
        };
        let Some((local_port, remote, remote_port)) = connection_endpoints(id) else {
            reset_id(id);
            return Err(Error::Send);
        };
        let syn = PendingSegment {
            local_port,
            remote,
            remote_port,
            sequence,
            acknowledgement: 0,
            flags: TCP_SYN,
            window: WINDOW,
        };
        if !syn.emit(&[]) {
            reset_id(id);
            return Err(Error::Send);
        }
        if wait_for_id(
            id,
            |connection| connection.state == State::Established,
            CONNECT_TICKS,
        ) {
            return Ok(());
        }
        let state = STATE.lock();
        let connection = state.connections.get(id as usize);
        let current = connection.map(|connection| connection.state);
        if current != Some(State::SynSent) {
            drop(state);
            let reset = current == Some(State::Reset);
            reset_id(id);
            return if reset {
                Err(Error::Reset)
            } else {
                Err(Error::Timeout)
            };
        }
    }
    reset_id(id);
    Err(Error::Timeout)
}

/// Open an active connection, letting the stack pick the local port.
pub fn connect(remote: Ipv4, port: u16) -> Result<(), Error> {
    connect_from(remote, port, None)
}

/// Send `data` on connection `id` and wait until the peer acknowledges it.
pub fn send_id(id: u8, data: &[u8]) -> Result<usize, Error> {
    let mut sent = 0usize;
    while sent < data.len() {
        let chunk_len = (data.len() - sent).min(MSS);
        let (local_port, remote, remote_port, sequence, acknowledgement, window) = {
            let state = STATE.lock();
            let Some(connection) = state.connections.get(id as usize) else {
                return Err(Error::Closed);
            };
            if !connection.in_use {
                return Err(Error::Closed);
            }
            // `Closing` means the peer sent FIN: its sending side is closed,
            // but ours is not, which is exactly the echo-server case (the host
            // shuts down its write half and then waits for the reply).
            if !matches!(connection.state, State::Established | State::Closing) {
                return Err(if connection.state == State::Reset {
                    Error::Reset
                } else {
                    Error::Closed
                });
            }
            (
                connection.local_port,
                connection.remote,
                connection.remote_port,
                connection.sequence,
                connection.acknowledgement,
                window_for(connection.rx_len),
            )
        };
        let chunk = &data[sent..sent + chunk_len];
        let segment = PendingSegment {
            local_port,
            remote,
            remote_port,
            sequence,
            acknowledgement,
            flags: TCP_PSH | TCP_ACK,
            window,
        };
        if !segment.emit(chunk) {
            return Err(Error::Send);
        }
        {
            let mut state = STATE.lock();
            let Some(connection) = state.connections.get_mut(id as usize) else {
                return Err(Error::Closed);
            };
            if !connection.in_use {
                return Err(Error::Closed);
            }
            connection.sequence = connection.sequence.wrapping_add(chunk_len as u32);
        }
        let target = sequence.wrapping_add(chunk_len as u32);
        if !wait_for_id(
            id,
            |connection| (connection.acked.wrapping_sub(target) as i32) >= 0,
            DATA_TICKS,
        ) {
            // The peer never acknowledged; report what left and drop the
            // connection rather than pretend the bytes were delivered.
            reset_id(id);
            return Err(Error::Timeout);
        }
        sent += chunk_len;
    }
    Ok(sent)
}

/// Send `data` on the active client.
pub fn send(data: &[u8]) -> Result<usize, Error> {
    let Some(id) = active_id() else {
        return Err(Error::Closed);
    };
    send_id(id, data)
}

/// Copy buffered payload into `out` and shift the remainder down. Returns how
/// many bytes were copied; the caller holds the state lock.
fn drain_rx(connection: &mut Connection, out: &mut [u8]) -> usize {
    let buffered = connection.rx_len;
    if buffered == 0 {
        return 0;
    }
    let take = buffered.min(out.len());
    out[..take].copy_from_slice(&connection.rx[..take]);
    // The connection is small enough that a copy is cheaper than a ring
    // buffer's bookkeeping.
    connection.rx.copy_within(take..buffered, 0);
    connection.rx_len = buffered - take;
    take
}

/// The window-update segment that must follow [`drain_rx`], or `None` when the
/// connection can no longer accept data. The caller sends it after releasing
/// the state lock: the send path may poll the NIC and re-enter [`on_segment`].
fn window_update(connection: &Connection) -> Option<PendingSegment> {
    match connection.state {
        State::Established | State::Closing => Some(segment_for(connection, TCP_ACK)),
        _ => None,
    }
}

/// Read whatever payload is available on `id`, waiting up to a bounded time for
/// the first byte. Returns `Ok(0)` when the peer has closed and the buffer is
/// empty.
pub fn recv_id(id: u8, out: &mut [u8]) -> Result<usize, Error> {
    if out.is_empty() {
        return Ok(0);
    }
    // A FIN is data delivery for this wait: without it the call sleeps out the
    // whole 10 s data deadline after the last byte and delays `close` by the
    // same amount (visible in a pcap as a very late client FIN).
    let readable = |connection: &Connection| {
        connection.rx_len > 0
            || matches!(
                connection.state,
                State::Closing | State::Closed | State::Reset
            )
    };
    wait_for_id(id, readable, DATA_TICKS);
    let mut state = STATE.lock();
    let Some(connection) = state.connections.get_mut(id as usize) else {
        return Ok(0);
    };
    if !connection.in_use {
        return Ok(0);
    }
    if connection.rx_len == 0 {
        return match connection.state {
            State::Closing | State::Closed => Ok(0),
            State::Reset => Err(Error::Reset),
            _ => Err(Error::Timeout),
        };
    }
    let take = drain_rx(connection, out);
    // The bytes just handed to the caller must be reflected in the advertised
    // window, or a sender that filled the old window has no way to learn that
    // space opened up (it would wait for a retransmission timer). Send the
    // window update *after* releasing the lock.
    let update = window_update(connection);
    drop(state);
    if let Some(segment) = update {
        let _ = segment.emit(&[]);
    }
    Ok(take)
}

/// One non-blocking receive on `id`: poll the NIC once, drain what is buffered,
/// and return [`Error::WouldBlock`] when there is nothing yet.
pub fn recv_nonblocking_id(id: u8, out: &mut [u8]) -> Result<usize, Error> {
    if out.is_empty() {
        return Ok(0);
    }
    poll();
    let mut state = STATE.lock();
    let Some(connection) = state.connections.get_mut(id as usize) else {
        return Ok(0);
    };
    if !connection.in_use {
        return Ok(0);
    }
    if connection.rx_len == 0 {
        return match connection.state {
            State::Closing | State::Closed => Ok(0),
            State::Reset => Err(Error::Reset),
            _ => Err(Error::WouldBlock),
        };
    }
    let take = drain_rx(connection, out);
    let update = window_update(connection);
    drop(state);
    if let Some(segment) = update {
        let _ = segment.emit(&[]);
    }
    Ok(take)
}

/// Read whatever payload is available on the active client, waiting up to a
/// bounded time for the first byte.
pub fn recv(out: &mut [u8]) -> Result<usize, Error> {
    match active_id() {
        Some(id) => recv_id(id, out),
        None => Err(Error::Closed),
    }
}

/// One non-blocking receive on the active client. Kept alongside [`recv`] as
/// the client-facing wrapper; the socket layer addresses accepted connections
/// through [`recv_nonblocking_id`].
#[allow(dead_code)]
pub fn recv_nonblocking(out: &mut [u8]) -> Result<usize, Error> {
    match active_id() {
        Some(id) => recv_nonblocking_id(id, out),
        None => Err(Error::Closed),
    }
}

/// The active client id, if its slot is live.
pub fn active_id() -> Option<u8> {
    let state = STATE.lock();
    let id = state.active?;
    if state
        .connections
        .get(id as usize)
        .map(|connection| connection.in_use)
        .unwrap_or(false)
    {
        Some(id)
    } else {
        None
    }
}

/// The current peer address and port of connection `id`, if it exists.
pub fn peer_id(id: u8) -> Option<(Ipv4, u16)> {
    let state = STATE.lock();
    let connection = state.connections.get(id as usize)?;
    if !connection.in_use || connection.state == State::Closed {
        None
    } else {
        Some((connection.remote, connection.remote_port))
    }
}

/// The active client's peer address and port. Kept alongside [`peer_id`] for
/// the one-connection API; the socket layer uses the id form.
#[allow(dead_code)]
pub fn peer() -> Option<(Ipv4, u16)> {
    active_id().and_then(peer_id)
}

/// Whether connection `id` has payload buffered or has reached end of stream
/// (the `poll(POLLIN)`/`POLLHUP` half for a stream socket).
pub fn readable_id(id: u8) -> bool {
    let state = STATE.lock();
    match state.connections.get(id as usize) {
        Some(connection) if connection.in_use => {
            connection.rx_len > 0
                || matches!(
                    connection.state,
                    State::Closing | State::Closed | State::Reset
                )
        }
        _ => false,
    }
}

/// Whether connection `id` is in the reset state (the `POLLERR` half).
pub fn reset_id_state(id: u8) -> bool {
    let state = STATE.lock();
    state
        .connections
        .get(id as usize)
        .map(|connection| connection.in_use && connection.state == State::Reset)
        .unwrap_or(false)
}

// ── passive open (server) ──────────────────────────────────────────────────

/// Reserve `port` for a listening socket. `0` picks an ephemeral port.
///
/// The reservation lives in the single listener slot, so there is at most one
/// listener (and one pre-listen bound socket) at a time. The port is released
/// by [`release_binding`] (close before `listen`) or [`close_listener`].
pub fn bind_port(port: u16) -> Result<u16, BindError> {
    let mut state = STATE.lock();
    if state.listener.bound.is_some() || state.listener.active {
        return Err(BindError::AddressInUse);
    }
    let chosen = if port != 0 {
        if state.port_in_use(port) {
            return Err(BindError::AddressInUse);
        }
        port
    } else {
        match allocate_port(&state) {
            Some(port) => port,
            None => return Err(BindError::NoPorts),
        }
    };
    state.listener.bound = Some(chosen);
    state.listener.active = false;
    state.listener.backlog = 1;
    state.listener.queue_len = 0;
    Ok(chosen)
}

/// Turn the reserved port into an active listener. `backlog` is clamped to
/// `1..=ACCEPT_QUEUE`.
pub fn listen(port: u16, backlog: usize) -> Result<(), BindError> {
    let mut state = STATE.lock();
    if state.listener.bound != Some(port) {
        return Err(BindError::NotBound);
    }
    if state.listener.active {
        state.listener.backlog = backlog.clamp(1, ACCEPT_QUEUE);
        return Ok(());
    }
    if state
        .connections
        .iter()
        .any(|connection| connection.in_use && connection.local_port == port)
    {
        return Err(BindError::AddressInUse);
    }
    state.listener.active = true;
    state.listener.backlog = backlog.clamp(1, ACCEPT_QUEUE);
    state.listener.queue_len = 0;
    Ok(())
}

/// Drop a pre-listen bind reservation. A listening socket is torn down by
/// [`close_listener`] instead.
pub fn release_binding(port: u16) {
    let mut state = STATE.lock();
    if state.listener.bound == Some(port) && !state.listener.active {
        state.listener = Listener::EMPTY;
    }
}

/// Tear down the listener: stop accepting, reset every passive connection that
/// has not been accepted yet, and leave accepted sockets alone.
pub fn close_listener() {
    let port = {
        let mut state = STATE.lock();
        let Some(port) = state.listener.bound else {
            return;
        };
        state.listener = Listener::EMPTY;
        port
    };
    loop {
        let pending = {
            let mut state = STATE.lock();
            let index = state.connections.iter().position(|connection| {
                connection.in_use
                    && connection.passive
                    && !connection.accepted
                    && connection.local_port == port
            });
            match index {
                Some(index) => {
                    let connection = &mut state.connections[index];
                    let segment = segment_for(connection, TCP_RST | TCP_ACK);
                    connection.clear();
                    Some(segment)
                }
                None => None,
            }
        };
        match pending {
            Some(segment) => {
                let _ = segment.emit(&[]);
            }
            None => break,
        }
    }
}

/// Pop the oldest handshaken connection without polling or waiting.
fn accept_take() -> Option<(u8, Ipv4, u16)> {
    let mut state = STATE.lock();
    while state.listener.queue_len > 0 {
        let id = state.listener.queue[0];
        state.listener.remove(id as usize);
        let Some(connection) = state.connections.get_mut(id as usize) else {
            continue;
        };
        connection.queued = false;
        if !connection.in_use || connection.state == State::Reset {
            continue;
        }
        connection.accepted = true;
        return Some((id, connection.remote, connection.remote_port));
    }
    None
}

/// Pop one queued connection after a single NIC poll; `WouldBlock` when the
/// queue is empty.
pub fn accept_nonblocking() -> Result<(u8, Ipv4, u16), Error> {
    poll();
    accept_take().ok_or(Error::WouldBlock)
}

/// Wait up to `ticks` for a queued connection, polling the NIC. Returns
/// `WouldBlock` on timeout (the ABI's `-EAGAIN`).
pub fn accept(ticks: u64) -> Result<(u8, Ipv4, u16), Error> {
    let start = timer::ticks();
    let deadline = start.saturating_add(ticks);
    let mut stalled = 0u64;
    loop {
        poll();
        if let Some(accepted) = accept_take() {
            return Ok(accepted);
        }
        let now = timer::ticks();
        if now >= deadline {
            return Err(Error::WouldBlock);
        }
        if now == start {
            stalled += 1;
            if stalled >= STEP_ITERATIONS {
                return Err(Error::WouldBlock);
            }
        }
        core::hint::spin_loop();
    }
}

/// Whether a handshaken connection is waiting (the listener's `POLLIN`).
pub fn accept_pending() -> bool {
    STATE.lock().listener.queue_len > 0
}

// ── teardown ───────────────────────────────────────────────────────────────

/// Send FIN (established) or RST (half-open passive) for connection `id` and
/// release its slot.
pub fn close_id(id: u8) {
    let pending = {
        let mut state = STATE.lock();
        let Some(connection) = state.connections.get_mut(id as usize) else {
            return;
        };
        if !connection.in_use {
            return;
        }
        let flags = match connection.state {
            State::Established | State::Closing => TCP_FIN | TCP_ACK,
            State::SynReceived => TCP_RST | TCP_ACK,
            _ => 0,
        };
        let segment = if flags != 0 {
            Some(segment_for(connection, flags))
        } else {
            None
        };
        connection.clear();
        if state.active == Some(id) {
            state.active = None;
        }
        segment
    };
    if let Some(segment) = pending {
        let _ = segment.emit(&[]);
    }
}

/// Close both directions of `id` but keep its slot as an end-of-stream
/// tombstone: a later `recv` returns 0, a later `send` is `-EPIPE`, and the
/// descriptor's `close` frees the slot.
pub fn shutdown_id(id: u8) {
    let pending = {
        let mut state = STATE.lock();
        let Some(connection) = state.connections.get_mut(id as usize) else {
            return;
        };
        if !connection.in_use {
            return;
        }
        let segment = match connection.state {
            State::Established | State::Closing => {
                let segment = Some(segment_for(connection, TCP_FIN | TCP_ACK));
                connection.state = State::Closed;
                connection.rx_len = 0;
                segment
            }
            State::SynReceived | State::SynSent => {
                let segment = Some(segment_for(connection, TCP_RST | TCP_ACK));
                connection.state = State::Closed;
                connection.rx_len = 0;
                segment
            }
            _ => None,
        };
        segment
    };
    if let Some(segment) = pending {
        let _ = segment.emit(&[]);
    }
}

/// Abandon connection `id` without a handshake or FIN.
pub fn reset_id(id: u8) {
    let mut state = STATE.lock();
    if let Some(connection) = state.connections.get_mut(id as usize) {
        connection.clear();
    }
    if state.active == Some(id) {
        state.active = None;
    }
}

/// Close the active-open client (FIN when established) and forget it.
pub fn close() {
    if let Some(id) = active_id() {
        close_id(id);
    }
    let mut state = STATE.lock();
    state.active = None;
}

/// Abandon the active-open client without a handshake.
pub fn reset() {
    let mut state = STATE.lock();
    if let Some(id) = state.active {
        if let Some(connection) = state.connections.get_mut(id as usize) {
            connection.clear();
        }
    }
    state.active = None;
}
