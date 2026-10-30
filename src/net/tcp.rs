//! A minimal active-open TCP client (RFC 793): enough for one HTTP/1.0 request.
//!
//! This is deliberately not a general TCP implementation. It has one socket, it
//! only initiates connections, it assumes in-order delivery with no loss on the
//! path (QEMU's `user` network is a local host stack; a real link would need
//! retransmission timers and out-of-order reassembly), and it advertises a small
//! fixed window. What it does implement correctly is the part an HTTP client
//! needs: the three-way handshake, sequence/acknowledgement tracking, payload
//! delivery to the caller, a clean FIN from the peer and a bounded wait for
//! every step.
//!
//! The state lives in one static [`Socket`] guarded by a spinlock. [`on_segment`]
//! is called from the poll loop (no lock held by the caller); it updates the
//! state and then sends any acknowledgement *after* releasing the lock, so the
//! send path may itself poll and re-enter [`on_segment`] without deadlocking.

use super::wire::{self, Ipv4, TCP_ACK, TCP_FIN, TCP_PSH, TCP_RST, TCP_SYN};
use super::{config, poll};
use crate::arch::riscv64::timer;
use crate::sync::Spinlock;

/// Local port the single client socket uses.
const LOCAL_PORT: u16 = 49152;
/// Advertised receive window.
const WINDOW: u16 = 4096;
/// Receive buffer for one connection.
const RX_CAPACITY: usize = 4096;
/// Payload bytes per segment we send; small enough for one Ethernet frame.
const MSS: usize = 1000;
/// Ticks one step may take (3 s).
const STEP_TICKS: u64 = 300;
/// Poll iterations before a step is declared stuck (bounds a machine whose clock
/// does not advance).
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
        }
    }
}

/// A monotonically changing initial sequence number.
fn initial_sequence() -> u32 {
    (timer::ticks() as u32).wrapping_mul(2654435761) ^ 0x5041_4748
}

/// Send one segment on the socket's 4-tuple; returns whether the frame left.
fn send_segment(sequence: u32, acknowledgement: u32, flags: u16, payload: &[u8]) -> bool {
    let Some(config) = config() else {
        return false;
    };
    let Some((remote, remote_port)) = socket_endpoints() else {
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
                LOCAL_PORT,
                remote_port,
                sequence,
                acknowledgement,
                flags,
                WINDOW,
                payload,
            )
        },
    )
}

/// Copy the remote endpoint out of the socket (short lock, no nesting).
fn socket_endpoints() -> Option<(Ipv4, u16)> {
    let socket = SOCKET.lock();
    if socket.state == State::Closed {
        return None;
    }
    Some((socket.remote, socket.remote_port))
}

/// Handle an incoming TCP segment. Called from the poll loop with no lock held.
pub fn on_segment(src: Ipv4, dst: Ipv4, segment: &[u8]) {
    if !wire::tcp_checksum_ok(src, dst, segment) {
        return;
    }
    let Some(tcp) = wire::parse_tcp(segment) else {
        return;
    };
    if tcp.dst_port != LOCAL_PORT {
        return;
    }
    let payload = segment
        .get(tcp.payload_offset..tcp.payload_offset + tcp.payload_len)
        .unwrap_or(&[]);
    let mut ack = None;
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
                    ack = Some((socket.sequence, socket.acknowledgement));
                }
            }
            State::Established | State::Closing => {
                if tcp.flags & TCP_ACK != 0 {
                    if (tcp.acknowledgement.wrapping_sub(socket.acked) as i32) > 0 {
                        socket.acked = tcp.acknowledgement;
                    }
                }
                if !payload.is_empty() && tcp.sequence == socket.acknowledgement {
                    let start = socket.rx_len;
                    let take = payload.len().min(RX_CAPACITY - start);
                    if take > 0 {
                        socket.rx[start..start + take].copy_from_slice(&payload[..take]);
                        socket.rx_len = start + take;
                        socket.acknowledgement = socket.acknowledgement.wrapping_add(take as u32);
                        ack = Some((socket.sequence, socket.acknowledgement));
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
                    ack = Some((socket.sequence, socket.acknowledgement));
                }
            }
            _ => {}
        }
    }
    if let Some((sequence, acknowledgement)) = ack {
        let _ = send_segment(sequence, acknowledgement, TCP_ACK, &[]);
    }
}

/// Wait for the socket to reach a state satisfying `done`, polling the NIC.
fn wait_for(done: impl Fn(&Socket) -> bool) -> bool {
    let deadline = timer::ticks().saturating_add(STEP_TICKS);
    let mut iterations = 0u64;
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
        if timer::ticks() >= deadline || iterations >= STEP_ITERATIONS {
            return false;
        }
        iterations += 1;
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
        if !send_segment(sequence, 0, TCP_SYN, &[]) {
            reset();
            return Err(Error::Send);
        }
        if wait_for(|socket| socket.state == State::Established) {
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
        let (sequence, acknowledgement) = {
            let socket = SOCKET.lock();
            if socket.state != State::Established {
                return Err(if socket.state == State::Reset {
                    Error::Reset
                } else {
                    Error::Closed
                });
            }
            (socket.sequence, socket.acknowledgement)
        };
        let chunk = &data[sent..sent + chunk_len];
        if !send_segment(sequence, acknowledgement, TCP_PSH | TCP_ACK, chunk) {
            return Err(Error::Send);
        }
        {
            let mut socket = SOCKET.lock();
            socket.sequence = socket.sequence.wrapping_add(chunk_len as u32);
        }
        let target = sequence.wrapping_add(chunk_len as u32);
        if !wait_for(|socket| (socket.acked.wrapping_sub(target) as i32) >= 0) {
            // The peer never acknowledged; report what left and drop the
            // connection rather than pretend the bytes were delivered.
            reset();
            return Err(Error::Timeout);
        }
        sent += chunk_len;
    }
    Ok(sent)
}

/// Read whatever payload is available, waiting up to a bounded time for the
/// first byte. Returns `Ok(0)` when the peer has closed and the buffer is empty.
pub fn recv(out: &mut [u8]) -> Result<usize, Error> {
    if out.is_empty() {
        return Ok(0);
    }
    let has_data = |socket: &Socket| socket.rx_len > 0;
    wait_for(has_data);
    let mut socket = SOCKET.lock();
    if socket.rx_len == 0 {
        return match socket.state {
            State::Closing | State::Closed => Ok(0),
            State::Reset => Err(Error::Reset),
            _ => Err(Error::Timeout),
        };
    }
    let buffered = socket.rx_len;
    let take = buffered.min(out.len());
    out[..take].copy_from_slice(&socket.rx[..take]);
    // Shift the remainder down; the connection is small enough that a copy is
    // cheaper than a ring-buffer's bookkeeping.
    socket.rx.copy_within(take..buffered, 0);
    socket.rx_len = buffered - take;
    Ok(take)
}

/// Close the connection (FIN|ACK) and forget the socket.
pub fn close() {
    let (sequence, acknowledgement, state) = {
        let socket = SOCKET.lock();
        (socket.sequence, socket.acknowledgement, socket.state)
    };
    if state == State::Established || state == State::Closing {
        let _ = send_segment(sequence, acknowledgement, TCP_FIN | TCP_ACK, &[]);
    }
    reset();
}

/// Abandon the socket without a handshake.
pub fn reset() {
    *SOCKET.lock() = Socket::EMPTY;
}
