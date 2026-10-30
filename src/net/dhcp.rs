//! DHCP client (RFC 2131): DISCOVER/OFFER/REQUEST/ACK on UDP 68→67.
//!
//! The client is synchronous and polled: [`acquire`] sends a DISCOVER, waits for
//! an OFFER (source port 67), sends the REQUEST and waits for the ACK, retrying
//! the whole exchange a few times. The lease is turned into [`super::Config`]
//! and installed by the caller; option 1/3/6 are what the rest of the stack
//! consumes.
//!
//! What it does not do: renew or rebind in the background (there is no timer
//! event loop; the shell's `dhcp` command re-runs the whole exchange), RARP/BOOTP
//! fallback, multiple addresses, option overload, or a hostname. Those are
//! stated as gaps in `docs/roadmap.md` rather than half-implemented.

use core::fmt;

use super::wire::{self, Ipv4, DHCP_ACK, DHCP_DISCOVER, DHCP_OFFER, DHCP_REQUEST};
use super::{udp_send_raw, wait_udp, Config};
use crate::arch::riscv64::timer;
use crate::drivers::virtio_net;

/// Limited-broadcast IPv4 address.
const BROADCAST_IP: Ipv4 = [255, 255, 255, 255];
/// Unspecified address a client uses before it has a lease.
const UNSPECIFIED_IP: Ipv4 = [0, 0, 0, 0];
/// Server port.
const SERVER_PORT: u16 = 67;
/// Client port.
const CLIENT_PORT: u16 = 68;
/// Ticks to wait for an OFFER or ACK (2 s; QEMU's slirp answers in milliseconds).
const WAIT_TICKS: u64 = 200;
/// Whole-exchange attempts before giving up.
const ATTEMPTS: u8 = 3;

/// Why a lease could not be obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// No NIC was found.
    NoDevice,
    /// A message could not be built (should not happen; a programming error).
    Malformed,
    /// The exchange timed out after every attempt.
    Timeout,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NoDevice => f.write_str("no network device"),
            Error::Malformed => f.write_str("the client could not build its message"),
            Error::Timeout => f.write_str("no DHCP reply"),
        }
    }
}

/// A per-exchange transaction id. Ticks make it differ between boots and between
/// retries; the MAC makes two machines on one network unlikely to collide.
fn transaction_id(attempt: u8) -> u32 {
    let mut id = (timer::ticks() as u32) ^ 0x4d37_0000 ^ ((attempt as u32) << 16);
    if let Some(mac) = virtio_net::mac() {
        id ^= u32::from_be_bytes([0x52, mac[3], mac[4], mac[5]]);
    }
    id
}

/// Run the DISCOVER/OFFER/REQUEST/ACK exchange and return the lease.
pub fn acquire() -> Result<Config, Error> {
    let Some(mac) = virtio_net::mac() else {
        return Err(Error::NoDevice);
    };
    // A retry must re-run the whole exchange: the previous transaction id is
    // dead once its OFFER was not answered.
    for attempt in 0..ATTEMPTS {
        let transaction = transaction_id(attempt);
        let mut message = [0u8; 300];
        let length = wire::build_dhcp(&mut message, 1, transaction, mac, DHCP_DISCOVER, None, None)
            .ok_or(Error::Malformed)?;
        if !udp_send_raw(
            UNSPECIFIED_IP,
            BROADCAST_IP,
            CLIENT_PORT,
            SERVER_PORT,
            &message[..length],
        ) {
            continue;
        }
        let Some(offer) = wait_udp(CLIENT_PORT, None, Some(SERVER_PORT), WAIT_TICKS) else {
            continue;
        };
        let Some(offer) = wire::parse_dhcp(&offer.data[..offer.len]) else {
            continue;
        };
        if offer.operation != 2
            || offer.transaction != transaction
            || offer.options.message_type != DHCP_OFFER
        {
            continue;
        }
        let requested = offer.options.address.unwrap_or(offer.your_ip);
        let server = offer.options.server_id;
        let length = wire::build_dhcp(
            &mut message,
            1,
            transaction,
            mac,
            DHCP_REQUEST,
            Some(requested),
            server,
        )
        .ok_or(Error::Malformed)?;
        if !udp_send_raw(
            UNSPECIFIED_IP,
            BROADCAST_IP,
            CLIENT_PORT,
            SERVER_PORT,
            &message[..length],
        ) {
            continue;
        }
        let Some(ack) = wait_udp(CLIENT_PORT, None, Some(SERVER_PORT), WAIT_TICKS) else {
            continue;
        };
        let Some(ack) = wire::parse_dhcp(&ack.data[..ack.len]) else {
            continue;
        };
        if ack.operation != 2
            || ack.transaction != transaction
            || ack.options.message_type != DHCP_ACK
        {
            continue;
        }
        let ip = ack.options.address.unwrap_or(ack.your_ip);
        if ip == [0; 4] {
            continue;
        }
        let mask = ack.options.subnet_mask.unwrap_or([255, 255, 255, 0]);
        // A lease without a router is a link-local setup; use the server as the
        // gateway so the stack still has a next hop, and say so by leaving DNS
        // equal to it when no DNS option arrived.
        let gateway = ack
            .options
            .router
            .or(ack.options.server_id)
            .unwrap_or(UNSPECIFIED_IP);
        let dns = ack.options.dns.unwrap_or(gateway);
        return Ok(Config {
            ip,
            mask,
            gateway,
            dns,
            server: ack.options.server_id.unwrap_or([0; 4]),
            lease_seconds: ack.options.lease_seconds.unwrap_or(0),
        });
    }
    Err(Error::Timeout)
}

/// Install a lease into the stack's configuration. Used by `net::init` after
/// [`acquire`] and by the shell's `dhcp` command.
pub fn install(config: Config) {
    *super::CONFIG.lock() = Some(config);
}

/// Re-run the exchange and install the result; returns the old lease on failure.
pub fn renew() -> Result<Config, Error> {
    let lease = acquire()?;
    install(lease);
    Ok(lease)
}
