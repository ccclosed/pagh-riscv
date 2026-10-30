//! A DNS stub resolver (RFC 1035): one A-record query over UDP to the leased
//! server.
//!
//! The resolver is synchronous: it builds the query, sends it from
//! [`DNS_SOURCE_PORT`](super::DNS_SOURCE_PORT), polls for the answer and parses
//! the first A record whose transaction id matches. There is no cache (every
//! `dns` command asks again) and no TCP fallback for truncated answers — both
//! are stated as gaps, not implemented halfway.
//!
//! Name compression is handled in the answer section (a server may point back
//! into the question), which is the one non-obvious part; [`wire::parse_dns_a`]
//! owns it.

use super::wire::{self, Ipv4};
use super::{config, udp_send_raw, wait_udp, DNS_SOURCE_PORT};
use crate::arch::riscv64::timer;

/// DNS server port.
const DNS_PORT: u16 = 53;
/// Ticks one query may take (3 s).
const WAIT_TICKS: u64 = 300;
/// Whole-query attempts before giving up.
const ATTEMPTS: u8 = 2;

/// Why a name could not be resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// No lease: there is no DNS server to ask.
    NoLease,
    /// The lease has no DNS server address.
    NoServer,
    /// The query could not be built (name too long or empty label).
    BadName,
    /// No usable answer arrived.
    NoAnswer,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::NoLease => f.write_str("no DHCP lease"),
            Error::NoServer => f.write_str("the lease names no DNS server"),
            Error::BadName => f.write_str("not a valid host name"),
            Error::NoAnswer => f.write_str("no DNS answer"),
        }
    }
}

/// A transaction id that differs between queries (the resolver is single-flight,
/// so a counter plus the tick clock is enough).
fn transaction_id(attempt: u8) -> u16 {
    (timer::ticks() as u16) ^ 0x53a7 ^ ((attempt as u16) << 12)
}

/// Resolve `name` to its first A record.
pub fn resolve(name: &str) -> Result<Ipv4, Error> {
    let config = config().ok_or(Error::NoLease)?;
    if config.dns == [0; 4] {
        return Err(Error::NoServer);
    }
    if name.is_empty() || name.len() > 253 {
        return Err(Error::BadName);
    }
    let mut query = [0u8; 512];
    for attempt in 0..ATTEMPTS {
        let transaction = transaction_id(attempt);
        let length = wire::build_dns_query(&mut query, name, transaction).ok_or(Error::BadName)?;
        if !udp_send_raw(
            config.ip,
            config.dns,
            DNS_SOURCE_PORT,
            DNS_PORT,
            &query[..length],
        ) {
            continue;
        }
        let deadline = timer::ticks().saturating_add(WAIT_TICKS);
        let mut iterations = 0u64;
        loop {
            let Some(datagram) = wait_udp(DNS_SOURCE_PORT, Some(config.dns), Some(DNS_PORT), 10)
            else {
                if timer::ticks() >= deadline || iterations >= 4_000_000 {
                    break;
                }
                iterations += 1;
                continue;
            };
            if let Some(address) = wire::parse_dns_a(&datagram.data[..datagram.len], transaction) {
                return Ok(address);
            }
            if timer::ticks() >= deadline {
                break;
            }
        }
    }
    Err(Error::NoAnswer)
}
