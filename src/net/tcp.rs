//! A minimal active-open TCP client (RFC 793): enough for one HTTP/1.0 request.
//!
//! This is deliberately not a general TCP implementation. It has one socket, it
//! only initiates connections, it assumes in-order delivery with no loss on the
//! path (QEMU's `user` network is a local host stack; a real link would need
//! retransmission timers and out-of-order reassembly), and its advertised
//! window is exactly the free space in the 4 KiB receive buffer. What it does
//! implement correctly is the part an HTTP client needs: the three-way
//! handshake, sequence/acknowledgement tracking, payload delivery to the
//! caller, a clean FIN from the peer and a bounded wait for every step.
//!
//! The window matters for a package download. A receiver that always advertises
//! the full buffer lets the peer send bytes it cannot store; those bytes are
//! dropped, the sender waits for a retransmission timer, and a 13 KiB download
//! stalls after the first 4 KiB. Every ACK here carries the *actual* free
//! space, and [`recv`] re-opens the window after the caller drains the buffer.
//!
//! The state lives in one static [`Socket`] guarded by a spinlock. [`on_segment`]
//! is called from the poll loop (no lock held by the caller); it updates the
//! state and then sends any acknowledgement *after* releasing the lock, so the
//! send path may itself poll and re-enter [`on_segment`] without deadlocking.

use super::wire::{self, Ipv4, TCP_ACK, TCP_FIN, TCP_PSH, TCP_RST, TCP_SYN};
use super::{config, poll};
use crate::arch::riscv64::timer;
use crate::sync::Spinlock;
use core::sync::atomic::{AtomicU16, Ordering};

/// First local port handed to a connection. A *fresh* port per connection (not
/// one fixed port) is what keeps a second download after a close from landing on
/// a 4-tuple the host stack still holds in TIME_WAIT: with the old fixed port,
/// QEMU's user-network layer silently dropped the new SYN and the package
/// install timed out before the server saw a request.
const LOCAL_PORT_FIRST: u16 = 49152;
static NEXT_LOCAL_PORT: AtomicU16 = AtomicU16::new(LOCAL_PORT_FIRST);

/// Allocate the next local port; wraps back to 49152 past 65535.
fn allocate_local_port() -> u16 {
    loop {
        let port = NEXT_LOCAL_PORT.fetch_add(1, Ordering::Relaxed);
        if port >= LOCAL_PORT_FIRST {
            return port;
        }
        NEXT_LOCAL_PORT.store(LOCAL_PORT_FIRST, Ordering::Relaxed);
    }
}
/// Receive window when the buffer is empty (also the SYN window).
const WINDOW: u16 = 4096;
/// Receive buffer for one connection.
const RX_CAPACITY: usize = 4096;
/// Payload bytes per segment we send; small enough for one Ethernet frame.
const MSS: usize = 1000;
/// Ticks the handshake may take per SYN attempt (3 s).
const CONNECT_TICKS: u64 = 300;
/// Ticks a data step may take (10 s). A debug kernel writes the received bytes
/// to ext2 between polls, so the NIC's small RX ring can drop frames; the
/// sender then recovers on its retransmission timers, and the first few of
/// those (200 ms, 400 ms, …) must fit inside one bounded step. Three seconds
/// was enough for a single segment exchange and not for a 13 KiB package.
const DATA_TICKS: u64 = 1000;
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
    /// No connection.
    Closed,
    /// SYN sent, waiting for SYN+ACK.
    SynSent,
    /// Handshake done, data may flow.
    Established,
    /// The peer sent FIN; the local side may still read buffered data.
    Closing,
    /// The connection is gone (RST or a protocol error).
    Reset,
}

/// The one socket.
#[derive(Clone, Copy)]
struct Socket {
    state: State,
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

impl Socket {
    const EMPTY: Self = Socket {
        state: State::Closed,
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
}

static SOCKET: Spinlock<Socket> = Spinlock::new(Socket::EMPTY);

/// Why a connection step failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// No DHCP lease, so no source address.
    NoLease,
    /// Another connection is already open.
    Busy,
    /// The handshake or a data exchange timed out.
    Timeout,
    /// The peer reset the connection.
    Reset,
    /// The peer closed before the operation finished.
    Closed,
    /// A segment could not be built or sent.
    Send,
    /// A non-blocking receive found no buffered payload and the peer has not
    /// closed; the caller should poll again (the ABI's `-EAGAIN`).
    WouldBlock,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::NoLease => f.write_str("no DHCP lease"),
            Error::Busy => f.write_str("a TCP connection is already open"),
            Error::Timeout => f.write_str("TCP timeout"),
            Error::Reset => f.write_str("connection reset"),
            Error::Closed => f.write_str("connection closed"),
            Error::Send => f.write_str("the segment could not be sent"),
            Error::WouldBlock => f.write_str("no data buffered yet"),
        }
    }
}

/// The window to advertise for a buffer holding `used` bytes.
///
/// Advertising capacity the buffer does not have is what turns this client's
/// 4 KiB receive buffer into silent packet loss on anything larger: the peer
/// fills the window, the excess cannot be stored, and recovery waits on the
/// sender's retransmission timer instead of on an ACK.
fn window_for(used: usize) -> u16 {
    RX_CAPACITY.saturating_sub(used).min(u16::MAX as usize) as u16
}

/// A monotonically changing initial sequence number.
fn initial_sequence() -> u32 {
    (timer::ticks() as u32).wrapping_mul(2654435761) ^ 0x5041_4748
}

/// Send one segment on the socket's 4-tuple; returns whether the frame left.
fn send_segment(
    sequence: u32,
    acknowledgement: u32,
    flags: u16,
    payload: &[u8],
    window: u16,
) -> bool {
    let Some(config) = config() else {
        return false;
    };
    let Some((local_port, remote, remote_port)) = socket_endpoints() else {
        return false;
    };
    super::send_ipv4_raw(
        config.ip,
        remote,
        if let Some(mac) = super::next_hop(remote) {
            mac
        } else {
            return false;
        },
        wire::IP_TCP,
        0x5443,
        |frame| {
            wire::build_tcp(
                frame,
                0,
                config.ip,
                remote,
                local_port,
                remote_port,
                sequence,
                acknowledgement,
                flags,
                window,
                payload,
            )
        },
    )
}

/// Copy the connection's endpoints out of the socket (short lock, no nesting).
fn socket_endpoints() -> Option<(u16, Ipv4, u16)> {
    let socket = SOCKET.lock();
    if socket.state == State::Closed {
        return None;
    }
    Some((socket.local_port, socket.remote, socket.remote_port))
}

/// The local port of the current connection, read under its own short lock.
/// `on_segment` runs before the state lock below; the port can only change in
/// `connect`, which is serialised with this check by the shell's request loop.
fn socket_local_port() -> u16 {
    SOCKET.lock().local_port
}

/// Handle an incoming TCP segment. Called from the poll loop with no lock held.
pub fn on_segment(src: Ipv4, dst: Ipv4, segment: &[u8]) {
    if !wire::tcp_checksum_ok(src, dst, segment) {
        return;
    }
    let Some(tcp) = wire::parse_tcp(segment) else {
        return;
    };
    if tcp.dst_port != socket_local_port() {
        return;
    }
    let payload = segment
        .get(tcp.payload_offset..tcp.payload_offset + tcp.payload_len)
        .unwrap_or(&[]);
    let mut ack: Option<(u32, u32, u16)> = None;
    {
        let mut socket = SOCKET.lock();
        if socket.state == State::Closed
            || src != socket.remote
            || tcp.src_port != socket.remote_port
        {
            return;
        }
        if tcp.flags & TCP_RST != 0 {
            socket.state = State::Reset;
            return;
        }
        socket.peer_window = tcp.window;
        match socket.state {
            State::SynSent => {
                // The peer acknowledges the SYN with `our_seq + 1` (the SYN
                // consumes one sequence number); comparing against the SYN's own
                // sequence is a classic off-by-one that keeps the connection in
                // SynSent while the peer retransmits its SYN+ACK.
                if tcp.flags & (TCP_SYN | TCP_ACK) == (TCP_SYN | TCP_ACK)
                    && tcp.acknowledgement == socket.sequence.wrapping_add(1)
                {
                    socket.acknowledgement = tcp.sequence.wrapping_add(1);
                    socket.sequence = socket.sequence.wrapping_add(1);
                    socket.acked = socket.sequence;
                    socket.state = State::Established;
                    ack = Some((
                        socket.sequence,
                        socket.acknowledgement,
                        window_for(socket.rx_len),
                    ));
                }
            }
            State::Established | State::Closing => {
                if tcp.flags & TCP_ACK != 0 {
                    if (tcp.acknowledgement.wrapping_sub(socket.acked) as i32) > 0 {
                        socket.acked = tcp.acknowledgement;
                    }
                }
                if !payload.is_empty() {
                    if tcp.sequence == socket.acknowledgement {
                        let start = socket.rx_len;
                        let take = payload.len().min(RX_CAPACITY - start);
                        if take > 0 {
                            socket.rx[start..start + take].copy_from_slice(&payload[..take]);
                            socket.rx_len = start + take;
                            socket.acknowledgement =
                                socket.acknowledgement.wrapping_add(take as u32);
                            ack = Some((
                                socket.sequence,
                                socket.acknowledgement,
                                window_for(socket.rx_len),
                            ));
                        }
                    } else {
                        // A gap (or a duplicate): repeat the ACK. Three of these
                        // are the sender's fast-retransmit trigger; staying
                        // silent makes every lost segment cost a full RTO.
                        ack = Some((
                            socket.sequence,
                            socket.acknowledgement,
                            window_for(socket.rx_len),
                        ));
                    }
                }
                if tcp.flags & TCP_FIN != 0
                    && tcp.sequence.wrapping_add(payload.len() as u32) == socket.acknowledgement
                {
                    socket.acknowledgement = socket.acknowledgement.wrapping_add(1);
                    if socket.state == State::Established {
                        socket.state = State::Closing;
                    } else {
                        socket.state = State::Closed;
                    }
                    ack = Some((
                        socket.sequence,
                        socket.acknowledgement,
                        window_for(socket.rx_len),
                    ));
                }
            }
            _ => {}
        }
    }
    if let Some((sequence, acknowledgement, window)) = ack {
        let _ = send_segment(sequence, acknowledgement, TCP_ACK, &[], window);
    }
}

/// Wait for the socket to reach a state satisfying `done`, polling the NIC.
/// `ticks` is the bound in 10 ms timer ticks; there is no blocking sleep.
fn wait_for(done: impl Fn(&Socket) -> bool, ticks: u64) -> bool {
    let start = timer::ticks();
    let deadline = start.saturating_add(ticks);
    let mut stalled = 0u64;
    loop {
        poll();
        {
            let socket = SOCKET.lock();
            if done(&socket) {
                return true;
            }
            if socket.state == State::Reset || socket.state == State::Closed {
                return false;
            }
        }
        let now = timer::ticks();
        if now >= deadline {
            return false;
        }
        if now == start {
            // The clock is not moving: bound the spin, not the nominal wait.
            stalled += 1;
            if stalled >= STEP_ITERATIONS {
                return false;
            }
        }
        core::hint::spin_loop();
    }
}

/// Open a connection to `remote:port`, completing the three-way handshake.
pub fn connect(remote: Ipv4, port: u16) -> Result<(), Error> {
    if config().is_none() {
        return Err(Error::NoLease);
    }
    {
        let mut socket = SOCKET.lock();
        if socket.state != State::Closed && socket.state != State::Reset {
            return Err(Error::Busy);
        }
        socket.state = State::SynSent;
        socket.local_port = allocate_local_port();
        socket.remote = remote;
        socket.remote_port = port;
        socket.sequence = initial_sequence();
        socket.acknowledgement = 0;
        socket.acked = socket.sequence;
        socket.peer_window = WINDOW;
        socket.rx_len = 0;
    }
    // Two SYN attempts: a lost SYN is the one retransmission a local host stack
    // can actually need.
    for _attempt in 0..2 {
        // Read the sequence number out from under the lock *before* sending:
        // `send_segment` may poll the NIC, and the poll path re-enters this
        // module, so the lock must be free by then.
        let sequence = SOCKET.lock().sequence;
        if !send_segment(sequence, 0, TCP_SYN, &[], WINDOW) {
            reset();
            return Err(Error::Send);
        }
        if wait_for(|socket| socket.state == State::Established, CONNECT_TICKS) {
            return Ok(());
        }
        let state = SOCKET.lock().state;
        if state != State::SynSent {
            reset();
            return if state == State::Reset {
                Err(Error::Reset)
            } else {
                Err(Error::Timeout)
            };
        }
    }
    reset();
    Err(Error::Timeout)
}

/// Send `data` and wait until the peer acknowledges all of it.
pub fn send(data: &[u8]) -> Result<usize, Error> {
    let mut sent = 0usize;
    while sent < data.len() {
        let chunk_len = (data.len() - sent).min(MSS);
        let (sequence, acknowledgement, window) = {
            let socket = SOCKET.lock();
            if socket.state != State::Established {
                return Err(if socket.state == State::Reset {
                    Error::Reset
                } else {
                    Error::Closed
                });
            }
            (
                socket.sequence,
                socket.acknowledgement,
                window_for(socket.rx_len),
            )
        };
        let chunk = &data[sent..sent + chunk_len];
        if !send_segment(sequence, acknowledgement, TCP_PSH | TCP_ACK, chunk, window) {
            return Err(Error::Send);
        }
        {
            let mut socket = SOCKET.lock();
            socket.sequence = socket.sequence.wrapping_add(chunk_len as u32);
        }
        let target = sequence.wrapping_add(chunk_len as u32);
        if !wait_for(
            |socket| (socket.acked.wrapping_sub(target) as i32) >= 0,
            DATA_TICKS,
        ) {
            // The peer never acknowledged; report what left and drop the
            // connection rather than pretend the bytes were delivered.
            reset();
            return Err(Error::Timeout);
        }
        sent += chunk_len;
    }
    Ok(sent)
}

/// Copy buffered payload into `out` and shift the remainder down. Returns how
/// many bytes were copied; the caller holds the socket lock.
fn drain_rx(socket: &mut Socket, out: &mut [u8]) -> usize {
    let buffered = socket.rx_len;
    if buffered == 0 {
        return 0;
    }
    let take = buffered.min(out.len());
    out[..take].copy_from_slice(&socket.rx[..take]);
    // The connection is small enough that a copy is cheaper than a ring
    // buffer's bookkeeping.
    socket.rx.copy_within(take..buffered, 0);
    socket.rx_len = buffered - take;
    take
}

/// The window-update segment that must follow [`drain_rx`], or `None` when the
/// connection can no longer accept data. The caller sends it after releasing
/// the socket lock: `send_segment` may poll the NIC and re-enter `on_segment`.
fn window_update(socket: &Socket) -> Option<(u32, u32, u16)> {
    match socket.state {
        State::Established | State::Closing => Some((
            socket.sequence,
            socket.acknowledgement,
            window_for(socket.rx_len),
        )),
        _ => None,
    }
}

/// Read whatever payload is available, waiting up to a bounded time for the
/// first byte. Returns `Ok(0)` when the peer has closed and the buffer is empty.
pub fn recv(out: &mut [u8]) -> Result<usize, Error> {
    if out.is_empty() {
        return Ok(0);
    }
    // A FIN is data delivery for this wait: without it the call sleeps out the
    // whole 10 s data deadline after the last byte and delays `close` by the
    // same amount (visible in a pcap as a very late client FIN).
    let readable = |socket: &Socket| {
        socket.rx_len > 0 || matches!(socket.state, State::Closing | State::Closed)
    };
    wait_for(readable, DATA_TICKS);
    let mut socket = SOCKET.lock();
    if socket.rx_len == 0 {
        return match socket.state {
            State::Closing | State::Closed => Ok(0),
            State::Reset => Err(Error::Reset),
            _ => Err(Error::Timeout),
        };
    }
    let take = drain_rx(&mut socket, out);
    // The bytes just handed to the caller must be reflected in the advertised
    // window, or a sender that filled the old window has no way to learn that
    // space opened up (it would wait for a retransmission timer). Send the
    // window update *after* releasing the lock: `send_segment` can poll the NIC
    // and re-enter `on_segment`, which takes this lock.
    let update = window_update(&socket);
    drop(socket);
    if let Some((sequence, acknowledgement, window)) = update {
        let _ = send_segment(sequence, acknowledgement, TCP_ACK, &[], window);
    }
    Ok(take)
}

/// One non-blocking receive: poll the NIC once, drain what is buffered, and
/// return [`Error::WouldBlock`] when there is nothing yet. The caller (a
/// socket `recv` syscall) turns that into `-EAGAIN`; the program polls.
///
/// A peer FIN is not an error: once the buffer is empty the result is `Ok(0)`,
/// the ABI's end of stream.
pub fn recv_nonblocking(out: &mut [u8]) -> Result<usize, Error> {
    if out.is_empty() {
        return Ok(0);
    }
    poll();
    let mut socket = SOCKET.lock();
    if socket.rx_len == 0 {
        return match socket.state {
            State::Closing | State::Closed => Ok(0),
            State::Reset => Err(Error::Reset),
            _ => Err(Error::WouldBlock),
        };
    }
    let take = drain_rx(&mut socket, out);
    let update = window_update(&socket);
    drop(socket);
    if let Some((sequence, acknowledgement, window)) = update {
        let _ = send_segment(sequence, acknowledgement, TCP_ACK, &[], window);
    }
    Ok(take)
}

/// The current peer address and port, if a connection was started.
pub fn peer() -> Option<(Ipv4, u16)> {
    let socket = SOCKET.lock();
    if socket.state == State::Closed {
        None
    } else {
        Some((socket.remote, socket.remote_port))
    }
}

/// Close the connection (FIN|ACK) and forget the socket.
pub fn close() {
    let (sequence, acknowledgement, state, window) = {
        let socket = SOCKET.lock();
        (
            socket.sequence,
            socket.acknowledgement,
            socket.state,
            window_for(socket.rx_len),
        )
    };
    if state == State::Established || state == State::Closing {
        let _ = send_segment(sequence, acknowledgement, TCP_FIN | TCP_ACK, &[], window);
    }
    reset();
}

/// Abandon the socket without a handshake.
pub fn reset() {
    *SOCKET.lock() = Socket::EMPTY;
}
