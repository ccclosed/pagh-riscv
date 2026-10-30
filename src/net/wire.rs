//! Pure wire-format code: parse and build Ethernet, ARP, IPv4, ICMP, UDP, TCP,
//! DHCP and DNS messages, with the checksums each one requires.
//!
//! Nothing here touches the device, the clock or any global: every function takes
//! a byte slice and returns offsets/fields (or writes into a caller-provided
//! buffer). That is what makes the network stack testable without a NIC — the
//! `selftest` suite in `src/net/selftest.rs` drives these functions against fixed
//! packets and builder/parser round trips.
//!
//! The conventions that hold across the module:
//!
//! * **Network byte order on the wire.** Fields are accessed through the
//!   `be16`/`be32`/`get_be16`/`put_be16` helpers; host-order values never leak
//!   into a packet by accident.
//! * **No panics on hostile input.** Every read is bounds-checked and every
//!   length/offset arithmetic uses `checked_*`; a malformed frame is `None`,
//!   never an index panic (the kernel runs with `panic = "abort"`).
//! * **Validation is not optional.** IPv4 and transport checksums are verified
//!   when parsing, and the builders always compute them; a frame that passes the
//!   parser is a frame this stack may act on.
//! * **Fragments are dropped, not reassembled.** The stack is small and
//!   synchronous; a fragment without its neighbours is useless and reassembly is
//!   an attack surface it does not need.

/// A 48-bit Ethernet address.
pub type Mac = [u8; 6];
/// An IPv4 address in network byte order.
pub type Ipv4 = [u8; 4];

/// Bytes of an Ethernet II header.
pub const ETH_HEADER: usize = 14;
/// EtherType of ARP.
pub const ETH_ARP: u16 = 0x0806;
/// EtherType of IPv4.
pub const ETH_IPV4: u16 = 0x0800;
/// Smallest IPv4 header (no options).
pub const IPV4_HEADER: usize = 20;
/// IP protocol number of ICMP.
pub const IP_ICMP: u8 = 1;
/// IP protocol number of TCP.
pub const IP_TCP: u8 = 6;
/// IP protocol number of UDP.
pub const IP_UDP: u8 = 17;
/// Bytes of a UDP header.
pub const UDP_HEADER: usize = 8;
/// Bytes of an ICMP echo header (type, code, checksum, id, sequence).
pub const ICMP_ECHO_HEADER: usize = 8;
/// Bytes of a TCP header without options.
pub const TCP_HEADER: usize = 20;
/// Largest UDP payload this stack builds into one frame (1500 MTU minus headers).
pub const UDP_MAX_PAYLOAD: usize = 1500 - IPV4_HEADER - UDP_HEADER;

/// The DHCP magic cookie that starts the options field (RFC 2131).
pub const DHCP_MAGIC: u32 = 0x6382_5363;

// ── primitive readers/writers ──────────────────────────────────────────────

/// Read a big-endian `u16` at `offset`.
pub fn be16(buf: &[u8], offset: usize) -> Option<u16> {
    let slice = buf.get(offset..offset.checked_add(2)?)?;
    Some(u16::from_be_bytes([slice[0], slice[1]]))
}

/// Read a big-endian `u32` at `offset`.
pub fn be32(buf: &[u8], offset: usize) -> Option<u32> {
    let slice = buf.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

/// Write a big-endian `u16`.
pub fn put_be16(buf: &mut [u8], offset: usize, value: u16) -> Option<()> {
    let slot = buf.get_mut(offset..offset.checked_add(2)?)?;
    slot.copy_from_slice(&value.to_be_bytes());
    Some(())
}

/// Write a big-endian `u32`.
pub fn put_be32(buf: &mut [u8], offset: usize, value: u32) -> Option<()> {
    let slot = buf.get_mut(offset..offset.checked_add(4)?)?;
    slot.copy_from_slice(&value.to_be_bytes());
    Some(())
}

/// Copy `src` into `buf[offset..]`, failing when it does not fit.
pub fn put_bytes(buf: &mut [u8], offset: usize, src: &[u8]) -> Option<()> {
    let slot = buf.get_mut(offset..offset.checked_add(src.len())?)?;
    slot.copy_from_slice(src);
    Some(())
}

/// Copy from `buf[offset..offset+len]`, failing when out of bounds.
pub fn bytes_at(buf: &[u8], offset: usize, len: usize) -> Option<&[u8]> {
    buf.get(offset..offset.checked_add(len)?)
}

// ── checksums ──────────────────────────────────────────────────────────────

/// Fold a 32-bit accumulator into the 16-bit one's-complement sum.
fn fold(mut sum: u32) -> u16 {
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    sum as u16
}

/// Raw one's-complement sum over `data` (even length padding with a zero byte).
fn sum_words(data: &[u8]) -> u32 {
    let mut sum = 0u32;
    let mut chunks = data.chunks_exact(2);
    for pair in &mut chunks {
        sum += u16::from_be_bytes([pair[0], pair[1]]) as u32;
    }
    if let [last] = chunks.remainder() {
        sum += (*last as u32) << 8;
    }
    sum
}

/// The Internet checksum (RFC 1071) of `data`.
///
/// A structure whose checksum field is already filled in sums to zero when this
/// function is applied to the whole structure — that is how the parsers validate
/// packets.
pub fn checksum(data: &[u8]) -> u16 {
    !fold(sum_words(data))
}

/// The one's-complement sum of a transport pseudo-header (RFC 793 §3.1).
fn pseudo_sum(src: Ipv4, dst: Ipv4, protocol: u8, length: u16) -> u32 {
    let mut sum = 0u32;
    sum += u16::from_be_bytes([src[0], src[1]]) as u32;
    sum += u16::from_be_bytes([src[2], src[3]]) as u32;
    sum += u16::from_be_bytes([dst[0], dst[1]]) as u32;
    sum += u16::from_be_bytes([dst[2], dst[3]]) as u32;
    sum += protocol as u32;
    sum += length as u32;
    sum
}

/// UDP/TCP checksum over the pseudo-header plus the whole segment.
///
/// `segment` includes the transport header, whose checksum field must be zero
/// when this is called (the builders write the result into that field).
pub fn transport_checksum(src: Ipv4, dst: Ipv4, protocol: u8, segment: &[u8]) -> u16 {
    let length = segment.len() as u16;
    let sum = pseudo_sum(src, dst, protocol, length) + sum_words(segment);
    !fold(sum)
}

/// Verify a transport checksum: zero means valid.
pub fn transport_checksum_ok(src: Ipv4, dst: Ipv4, protocol: u8, segment: &[u8]) -> bool {
    let length = segment.len() as u16;
    fold(pseudo_sum(src, dst, protocol, length) + sum_words(segment)) == 0xffff
}

/// True for a MAC address in which the group (multicast/broadcast) bit is set.
pub fn mac_is_group(mac: Mac) -> bool {
    mac[0] & 0x01 != 0
}

/// True when `mac` is the Ethernet broadcast address.
pub fn mac_is_broadcast(mac: Mac) -> bool {
    mac == [0xff; 6]
}

/// True when `dst` is on the subnet described by `ip`/`mask`.
pub fn same_subnet(ip: Ipv4, mask: Ipv4, dst: Ipv4) -> bool {
    for index in 0..4 {
        if (ip[index] ^ dst[index]) & mask[index] != 0 {
            return false;
        }
    }
    true
}

/// True when `ip` is a limited-broadcast destination (255.255.255.255).
pub fn is_limited_broadcast(ip: Ipv4) -> bool {
    ip == [255; 4]
}

// ── Ethernet ───────────────────────────────────────────────────────────────

/// A parsed Ethernet II header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ethernet {
    /// Destination MAC.
    pub dst: Mac,
    /// Source MAC.
    pub src: Mac,
    /// EtherType of the payload.
    pub ethertype: u16,
}

/// Parse the Ethernet II header of `frame`; returns it and the payload offset.
pub fn parse_ethernet(frame: &[u8]) -> Option<(Ethernet, usize)> {
    let header = frame.get(..ETH_HEADER)?;
    let mut dst = [0u8; 6];
    let mut src = [0u8; 6];
    dst.copy_from_slice(&header[0..6]);
    src.copy_from_slice(&header[6..12]);
    let ethertype = be16(header, 12)?;
    Some((
        Ethernet {
            dst,
            src,
            ethertype,
        },
        ETH_HEADER,
    ))
}

/// Write an Ethernet II header; returns the payload offset.
pub fn build_ethernet(buf: &mut [u8], dst: Mac, src: Mac, ethertype: u16) -> Option<usize> {
    put_bytes(buf, 0, &dst)?;
    put_bytes(buf, 6, &src)?;
    put_be16(buf, 12, ethertype)?;
    Some(ETH_HEADER)
}

// ── ARP (RFC 826) ──────────────────────────────────────────────────────────

/// ARP operation: request.
pub const ARP_REQUEST: u16 = 1;
/// ARP operation: reply.
pub const ARP_REPLY: u16 = 2;
/// Bytes of an IPv4 ARP body (fixed header plus one address pair).
const ARP_BYTES: usize = 28;

/// A parsed Ethernet/IPv4 ARP packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Arp {
    /// `1` for a request, `2` for a reply.
    pub operation: u16,
    /// Sender hardware address.
    pub sender_mac: Mac,
    /// Sender protocol (IPv4) address.
    pub sender_ip: Ipv4,
    /// Target hardware address.
    pub target_mac: Mac,
    /// Target protocol address.
    pub target_ip: Ipv4,
}

/// Parse an ARP body (the Ethernet payload, not the frame).
pub fn parse_arp(body: &[u8]) -> Option<Arp> {
    let body = body.get(..ARP_BYTES)?;
    if be16(body, 0)? != 1 || be16(body, 2)? != ETH_IPV4 {
        return None;
    }
    if body[4] != 6 || body[5] != 4 {
        return None;
    }
    let operation = be16(body, 6)?;
    let mut sender_mac = [0u8; 6];
    let mut target_mac = [0u8; 6];
    sender_mac.copy_from_slice(&body[8..14]);
    target_mac.copy_from_slice(&body[18..24]);
    Some(Arp {
        operation,
        sender_mac,
        sender_ip: [body[14], body[15], body[16], body[17]],
        target_mac,
        target_ip: [body[24], body[25], body[26], body[27]],
    })
}

/// Build an Ethernet + ARP packet; returns the frame length.
pub fn build_arp_frame(
    frame: &mut [u8],
    eth_dst: Mac,
    eth_src: Mac,
    operation: u16,
    sender_mac: Mac,
    sender_ip: Ipv4,
    target_mac: Mac,
    target_ip: Ipv4,
) -> Option<usize> {
    let offset = build_ethernet(frame, eth_dst, eth_src, ETH_ARP)?;
    let body = &mut frame[offset..offset.checked_add(ARP_BYTES)?];
    put_be16(body, 0, 1)?; // hardware type: Ethernet
    put_be16(body, 2, ETH_IPV4)?; // protocol type
    body[4] = 6; // hardware address length
    body[5] = 4; // protocol address length
    put_be16(body, 6, operation)?;
    put_bytes(body, 8, &sender_mac)?;
    put_bytes(body, 14, &sender_ip)?;
    put_bytes(body, 18, &target_mac)?;
    put_bytes(body, 24, &target_ip)?;
    Some(offset + ARP_BYTES)
}

// ── IPv4 (RFC 791) ─────────────────────────────────────────────────────────

/// A parsed IPv4 header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ipv4Packet {
    /// Source address.
    pub src: Ipv4,
    /// Destination address.
    pub dst: Ipv4,
    /// Protocol number (ICMP/UDP/TCP).
    pub protocol: u8,
    /// Time-to-live.
    pub ttl: u8,
    /// Identification field.
    pub id: u16,
    /// Offset of the payload inside the frame.
    pub payload_offset: usize,
    /// Length of the payload.
    pub payload_len: usize,
}

/// Parse and validate an IPv4 packet whose header starts at `offset` in `frame`.
///
/// Rejects a non-v4 header, a header shorter than 20 bytes, a total length that
/// does not fit the frame, a bad checksum and every fragment (the stack does not
/// reassemble).
pub fn parse_ipv4(frame: &[u8], offset: usize) -> Option<Ipv4Packet> {
    let header = bytes_at(frame, offset, IPV4_HEADER)?;
    if header[0] >> 4 != 4 {
        return None;
    }
    let header_len = ((header[0] & 0x0f) as usize).checked_mul(4)?;
    if header_len < IPV4_HEADER {
        return None;
    }
    let total_len = be16(header, 2)? as usize;
    if total_len < header_len {
        return None;
    }
    let all = bytes_at(frame, offset, total_len)?;
    if checksum(&all[..header_len]) != 0 {
        return None;
    }
    let flags_and_fragment = be16(header, 6)?;
    if flags_and_fragment & 0x1fff != 0 || flags_and_fragment & 0x2000 != 0 {
        // Non-zero fragment offset or MF: a fragment, which this stack drops.
        return None;
    }
    Some(Ipv4Packet {
        src: [header[12], header[13], header[14], header[15]],
        dst: [header[16], header[17], header[18], header[19]],
        protocol: header[9],
        ttl: header[8],
        id: be16(header, 4)?,
        payload_offset: offset + header_len,
        payload_len: total_len - header_len,
    })
}

/// Write an IPv4 header for `payload_len` bytes that the caller will place right
/// after it; returns the payload offset. The checksum is computed here.
pub fn build_ipv4(
    frame: &mut [u8],
    offset: usize,
    src: Ipv4,
    dst: Ipv4,
    protocol: u8,
    payload_len: usize,
    id: u16,
    ttl: u8,
) -> Option<usize> {
    let total_len = IPV4_HEADER.checked_add(payload_len)?;
    if total_len > u16::MAX as usize {
        return None;
    }
    let header = frame.get_mut(offset..offset.checked_add(IPV4_HEADER)?)?;
    header[0] = 0x45; // version 4, header length 5 words
    header[1] = 0; // DSCP/ECN
    put_be16(header, 2, total_len as u16)?;
    put_be16(header, 4, id)?;
    put_be16(header, 6, 0)?; // flags/fragment offset: none
    header[8] = ttl;
    header[9] = protocol;
    put_be16(header, 10, 0)?; // checksum placeholder
    put_bytes(header, 12, &src)?;
    put_bytes(header, 16, &dst)?;
    let checksum = checksum(header);
    put_be16(header, 10, checksum)?;
    Some(offset + IPV4_HEADER)
}

// ── ICMP echo (RFC 792) ────────────────────────────────────────────────────

/// ICMP echo request type.
pub const ICMP_ECHO_REQUEST: u8 = 8;
/// ICMP echo reply type.
pub const ICMP_ECHO_REPLY: u8 = 0;

/// A parsed ICMP echo message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IcmpEcho {
    /// `8` for a request, `0` for a reply.
    pub kind: u8,
    /// Code (must be 0 for echo).
    pub code: u8,
    /// Echo identifier.
    pub id: u16,
    /// Echo sequence number.
    pub sequence: u16,
    /// Offset of the echo data.
    pub data_offset: usize,
    /// Length of the echo data.
    pub data_len: usize,
}

/// Parse an ICMP echo request/reply from an IPv4 payload.
pub fn parse_icmp_echo(body: &[u8], offset: usize) -> Option<IcmpEcho> {
    let header = bytes_at(body, offset, ICMP_ECHO_HEADER)?;
    if checksum(&body[offset..]) != 0 {
        return None;
    }
    Some(IcmpEcho {
        kind: header[0],
        code: header[1],
        id: be16(header, 4)?,
        sequence: be16(header, 6)?,
        data_offset: offset + ICMP_ECHO_HEADER,
        data_len: body.len().saturating_sub(offset + ICMP_ECHO_HEADER),
    })
}

/// Build an ICMP echo message at `offset`; returns the message length.
pub fn build_icmp_echo(
    buf: &mut [u8],
    offset: usize,
    kind: u8,
    id: u16,
    sequence: u16,
    data: &[u8],
) -> Option<usize> {
    let total = ICMP_ECHO_HEADER.checked_add(data.len())?;
    let message = buf.get_mut(offset..offset.checked_add(total)?)?;
    message[0] = kind;
    message[1] = 0;
    put_be16(message, 2, 0)?;
    put_be16(message, 4, id)?;
    put_be16(message, 6, sequence)?;
    put_bytes(message, ICMP_ECHO_HEADER, data)?;
    let checksum = checksum(message);
    put_be16(message, 2, checksum)?;
    Some(total)
}

// ── UDP (RFC 768) ──────────────────────────────────────────────────────────

/// A parsed UDP datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Udp {
    /// Source port.
    pub src_port: u16,
    /// Destination port.
    pub dst_port: u16,
    /// Offset of the payload inside the IPv4 header's frame slice.
    pub payload_offset: usize,
    /// Payload length.
    pub payload_len: usize,
}

/// Parse a UDP datagram from an IPv4 payload.
pub fn parse_udp(segment: &[u8]) -> Option<Udp> {
    let header = segment.get(..UDP_HEADER)?;
    let length = be16(header, 4)? as usize;
    if length < UDP_HEADER || length > segment.len() {
        return None;
    }
    Some(Udp {
        src_port: be16(header, 0)?,
        dst_port: be16(header, 2)?,
        payload_offset: UDP_HEADER,
        payload_len: length - UDP_HEADER,
    })
}

/// Build a UDP datagram (header + payload) at `offset`; returns the datagram
/// length. `src`/`dst` are needed for the pseudo-header checksum.
pub fn build_udp(
    buf: &mut [u8],
    offset: usize,
    src: Ipv4,
    dst: Ipv4,
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
) -> Option<usize> {
    let total = UDP_HEADER.checked_add(payload.len())?;
    if payload.len() > UDP_MAX_PAYLOAD {
        return None;
    }
    let segment = buf.get_mut(offset..offset.checked_add(total)?)?;
    put_be16(segment, 0, src_port)?;
    put_be16(segment, 2, dst_port)?;
    put_be16(segment, 4, total as u16)?;
    put_be16(segment, 6, 0)?; // checksum placeholder
    put_bytes(segment, UDP_HEADER, payload)?;
    let checksum = transport_checksum(src, dst, IP_UDP, segment);
    // RFC 768: a computed checksum of zero is transmitted as 0xffff (zero means
    // "no checksum").
    put_be16(segment, 6, if checksum == 0 { 0xffff } else { checksum })?;
    Some(total)
}

/// Verify the UDP checksum of a datagram; a zero field means "not computed".
pub fn udp_checksum_ok(src: Ipv4, dst: Ipv4, segment: &[u8]) -> bool {
    match be16(segment, 6) {
        Some(0) => true,
        Some(_) => transport_checksum_ok(src, dst, IP_UDP, segment),
        None => false,
    }
}

// ── TCP (RFC 793) ──────────────────────────────────────────────────────────

/// TCP `FIN` flag.
pub const TCP_FIN: u16 = 1 << 0;
/// TCP `SYN` flag.
pub const TCP_SYN: u16 = 1 << 1;
/// TCP `RST` flag.
pub const TCP_RST: u16 = 1 << 2;
/// TCP `PSH` flag.
pub const TCP_PSH: u16 = 1 << 3;
/// TCP `ACK` flag.
pub const TCP_ACK: u16 = 1 << 4;

/// A parsed TCP segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tcp {
    /// Source port.
    pub src_port: u16,
    /// Destination port.
    pub dst_port: u16,
    /// Sequence number.
    pub sequence: u32,
    /// Acknowledgement number.
    pub acknowledgement: u32,
    /// Flags (the low 9 bits of the data offset/flags word).
    pub flags: u16,
    /// Advertised window.
    pub window: u16,
    /// Offset of the payload inside the IPv4 header's frame slice.
    pub payload_offset: usize,
    /// Payload length.
    pub payload_len: usize,
}

/// Parse a TCP segment from an IPv4 payload.
pub fn parse_tcp(segment: &[u8]) -> Option<Tcp> {
    let header = segment.get(..TCP_HEADER)?;
    let data_offset = ((header[12] >> 4) as usize).checked_mul(4)?;
    if data_offset < TCP_HEADER || data_offset > segment.len() {
        return None;
    }
    Some(Tcp {
        src_port: be16(header, 0)?,
        dst_port: be16(header, 2)?,
        sequence: be32(header, 4)?,
        acknowledgement: be32(header, 8)?,
        flags: be16(header, 12)? & 0x01ff,
        window: be16(header, 14)?,
        payload_offset: data_offset,
        payload_len: segment.len() - data_offset,
    })
}

/// Build a TCP header (and copy `payload` after it) at `offset`; returns the
/// segment length. No options are ever emitted.
pub fn build_tcp(
    buf: &mut [u8],
    offset: usize,
    src: Ipv4,
    dst: Ipv4,
    src_port: u16,
    dst_port: u16,
    sequence: u32,
    acknowledgement: u32,
    flags: u16,
    window: u16,
    payload: &[u8],
) -> Option<usize> {
    let total = TCP_HEADER.checked_add(payload.len())?;
    let segment = buf.get_mut(offset..offset.checked_add(total)?)?;
    put_be16(segment, 0, src_port)?;
    put_be16(segment, 2, dst_port)?;
    put_be32(segment, 4, sequence)?;
    put_be32(segment, 8, acknowledgement)?;
    segment[12] = 5 << 4; // data offset: five words
    put_be16(segment, 12, (5 << 12) | flags)?;
    put_be16(segment, 14, window)?;
    put_be16(segment, 16, 0)?; // checksum placeholder
    put_be16(segment, 18, 0)?; // urgent pointer
    put_bytes(segment, TCP_HEADER, payload)?;
    let checksum = transport_checksum(src, dst, IP_TCP, segment);
    put_be16(segment, 16, checksum)?;
    Some(total)
}

/// Verify a TCP segment checksum.
pub fn tcp_checksum_ok(src: Ipv4, dst: Ipv4, segment: &[u8]) -> bool {
    transport_checksum_ok(src, dst, IP_TCP, segment)
}

// ── DHCP (RFC 2131/2132) ───────────────────────────────────────────────────

/// DHCP message type option (53) values this client uses.
pub const DHCP_DISCOVER: u8 = 1;
/// DHCP OFFER (server → client).
pub const DHCP_OFFER: u8 = 2;
/// DHCP REQUEST (client → server).
pub const DHCP_REQUEST: u8 = 3;
/// DHCP ACK (server → client).
pub const DHCP_ACK: u8 = 5;

/// Options this client cares about, after validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DhcpOptions {
    /// Option 53: message type.
    pub message_type: u8,
    /// Option 1: subnet mask.
    pub subnet_mask: Option<Ipv4>,
    /// Option 3: router.
    pub router: Option<Ipv4>,
    /// Option 6: DNS server.
    pub dns: Option<Ipv4>,
    /// Option 54: server identifier.
    pub server_id: Option<Ipv4>,
    /// Option 51: lease time in seconds.
    pub lease_seconds: Option<u32>,
    /// Option 50 (only in a request/offer): requested/offered address.
    pub address: Option<Ipv4>,
}

/// A parsed DHCP message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dhcp {
    /// BOOTP operation: 1 = request from client, 2 = reply from server.
    pub operation: u8,
    /// Transaction identifier.
    pub transaction: u32,
    /// `yiaddr`: the address the server offers/assigns.
    pub your_ip: Ipv4,
    /// `chaddr`: the client hardware address.
    pub client_mac: Mac,
    /// Parsed options.
    pub options: DhcpOptions,
}

/// Offset of the fixed BOOTP header, up to and including the magic cookie.
const DHCP_FIXED: usize = 240;

/// Walk the DHCP options field and collect the ones this client needs.
fn dhcp_parse_options(options: &[u8]) -> Option<DhcpOptions> {
    let mut parsed = DhcpOptions {
        message_type: 0,
        subnet_mask: None,
        router: None,
        dns: None,
        server_id: None,
        lease_seconds: None,
        address: None,
    };
    let mut offset = 0usize;
    while offset < options.len() {
        let code = options[offset];
        offset += 1;
        if code == 0 {
            continue; // pad
        }
        if code == 255 {
            return Some(parsed);
        }
        let len = *options.get(offset)? as usize;
        offset += 1;
        let value = options.get(offset..offset.checked_add(len)?)?;
        offset += len;
        match code {
            1 if len == 4 => parsed.subnet_mask = Some([value[0], value[1], value[2], value[3]]),
            3 if len >= 4 => parsed.router = Some([value[0], value[1], value[2], value[3]]),
            6 if len >= 4 => parsed.dns = Some([value[0], value[1], value[2], value[3]]),
            50 if len == 4 => parsed.address = Some([value[0], value[1], value[2], value[3]]),
            51 if len == 4 => parsed.lease_seconds = Some(be32(value, 0)?),
            53 if len == 1 => parsed.message_type = value[0],
            54 if len == 4 => parsed.server_id = Some([value[0], value[1], value[2], value[3]]),
            _ => {}
        }
    }
    Some(parsed)
}

/// Parse a DHCP message (the UDP payload of port 67/68).
pub fn parse_dhcp(payload: &[u8]) -> Option<Dhcp> {
    if payload.len() < DHCP_FIXED {
        return None;
    }
    if be32(payload, 236)? != DHCP_MAGIC {
        return None;
    }
    let mut client_mac = [0u8; 6];
    client_mac.copy_from_slice(payload.get(28..34)?);
    Some(Dhcp {
        operation: payload[0],
        transaction: be32(payload, 4)?,
        your_ip: [payload[16], payload[17], payload[18], payload[19]],
        client_mac,
        options: dhcp_parse_options(&payload[DHCP_FIXED..])?,
    })
}

/// Build a DHCP client message (DISCOVER or REQUEST); returns its length.
///
/// The message is padded to the BOOTP minimum of 300 bytes, which is what every
/// server (including QEMU's slirp) expects to see.
pub fn build_dhcp(
    out: &mut [u8],
    operation: u8,
    transaction: u32,
    client_mac: Mac,
    message_type: u8,
    requested_ip: Option<Ipv4>,
    server_id: Option<Ipv4>,
) -> Option<usize> {
    let message = out.get_mut(..300)?;
    for byte in message.iter_mut() {
        *byte = 0;
    }
    message[0] = operation;
    message[1] = 1; // Ethernet
    message[2] = 6; // hardware address length
    put_be32(message, 4, transaction)?;
    put_be16(message, 10, 0x8000)?; // broadcast flag
    put_bytes(message, 28, &client_mac)?;
    put_be32(message, 236, DHCP_MAGIC)?;
    let mut offset = DHCP_FIXED;
    put_bytes(message, offset, &[53, 1, message_type])?;
    offset += 3;
    if let Some(ip) = requested_ip {
        put_bytes(message, offset, &[50, 4, ip[0], ip[1], ip[2], ip[3]])?;
        offset += 6;
    }
    if let Some(server) = server_id {
        put_bytes(
            message,
            offset,
            &[54, 4, server[0], server[1], server[2], server[3]],
        )?;
        offset += 6;
    }
    // Option 55: parameter request list — subnet mask, router, DNS.
    put_bytes(message, offset, &[55, 3, 1, 3, 6])?;
    offset += 5;
    put_bytes(message, offset, &[255])?;
    Some(300)
}

// ── DNS (RFC 1035) ─────────────────────────────────────────────────────────

/// Encode a dotted name as DNS labels; returns the bytes written.
fn dns_encode_name(name: &str, out: &mut [u8]) -> Option<usize> {
    let mut written = 0usize;
    for label in name.split('.') {
        if label.is_empty() || label.len() > 63 {
            return None;
        }
        if written + 1 + label.len() > out.len() {
            return None;
        }
        out[written] = label.len() as u8;
        written += 1;
        out.get_mut(written..written + label.len())?
            .copy_from_slice(label.as_bytes());
        written += label.len();
    }
    if written >= out.len() {
        return None;
    }
    out[written] = 0; // root label
    Some(written + 1)
}

/// Build a DNS A-record query for `name`; returns the message length.
pub fn build_dns_query(out: &mut [u8], name: &str, transaction: u16) -> Option<usize> {
    let message = out.get_mut(..12)?;
    for byte in message.iter_mut() {
        *byte = 0;
    }
    put_be16(out, 0, transaction)?;
    put_be16(out, 2, 0x0100)?; // recursion desired
    put_be16(out, 4, 1)?; // one question
    let name_len = dns_encode_name(name, out.get_mut(12..)?)?;
    let mut offset = 12 + name_len;
    put_be16(out, offset, 1)?; // A
    put_be16(out, offset + 2, 1)?; // IN
    offset += 4;
    Some(offset)
}

/// Skip a (possibly compressed) DNS name; returns the offset after it.
fn dns_skip_name(message: &[u8], mut offset: usize) -> Option<usize> {
    let mut jumps = 0u8;
    loop {
        let length = *message.get(offset)?;
        if length & 0xc0 == 0xc0 {
            // Compression pointer: the two-byte pointer ends this name in place.
            message.get(offset + 1)?;
            return Some(offset + 2);
        }
        offset += 1;
        if length == 0 {
            return Some(offset);
        }
        if length & 0xc0 != 0 {
            return None;
        }
        offset = offset.checked_add(length as usize)?;
        if offset > message.len() {
            return None;
        }
        jumps = jumps.wrapping_add(1);
        if jumps == 0 {
            return None;
        }
    }
}

/// Parse a DNS response and return the first A record; validates the
/// transaction id, the QR bit and the response code.
pub fn parse_dns_a(message: &[u8], transaction: u16) -> Option<Ipv4> {
    if be16(message, 0)? != transaction {
        return None;
    }
    let flags = be16(message, 2)?;
    if flags & 0x8000 == 0 || flags & 0x000f != 0 {
        return None; // not a response / non-zero RCODE
    }
    let questions = be16(message, 4)? as usize;
    let answers = be16(message, 6)? as usize;
    let mut offset = 12usize;
    for _ in 0..questions {
        offset = dns_skip_name(message, offset)?;
        offset = offset.checked_add(4)?; // QTYPE + QCLASS
    }
    for _ in 0..answers {
        offset = dns_skip_name(message, offset)?;
        let record_type = be16(message, offset)?;
        let record_class = be16(message, offset + 2)?;
        let _ttl = be32(message, offset + 4)?;
        let length = be16(message, offset + 8)? as usize;
        let data = offset.checked_add(10)?;
        if record_type == 1 && record_class == 1 && length == 4 {
            let address = bytes_at(message, data, 4)?;
            return Some([address[0], address[1], address[2], address[3]]);
        }
        offset = data.checked_add(length)?;
    }
    None
}

// ── text helpers (shell) ───────────────────────────────────────────────────

/// Parse a dotted-quad IPv4 address.
pub fn parse_ipv4_str(text: &str) -> Option<Ipv4> {
    let mut address = [0u8; 4];
    let mut count = 0usize;
    for part in text.split('.') {
        if count == 4 || part.is_empty() || part.len() > 3 {
            return None;
        }
        let value: u16 = part.parse().ok()?;
        if value > 255 {
            return None;
        }
        address[count] = value as u8;
        count += 1;
    }
    if count == 4 {
        Some(address)
    } else {
        None
    }
}

/// Format an IPv4 address as `a.b.c.d` into a caller-provided buffer.
pub fn format_ipv4(address: Ipv4, out: &mut [u8; 15]) -> Option<usize> {
    let mut written = 0usize;
    for (index, byte) in address.iter().enumerate() {
        let mut digits = [0u8; 3];
        let mut value = *byte as u32;
        let mut count = 0usize;
        if value == 0 {
            digits[0] = b'0';
            count = 1;
        }
        while value > 0 {
            digits[count] = b'0' + (value % 10) as u8;
            value /= 10;
            count += 1;
        }
        if written + count + u8::from(index < 3) as usize > out.len() {
            return None;
        }
        for digit in digits[..count].iter().rev() {
            out[written] = *digit;
            written += 1;
        }
        if index < 3 {
            out[written] = b'.';
            written += 1;
        }
    }
    Some(written)
}

/// Format a MAC address as `aa:bb:cc:dd:ee:ff` into a caller-provided buffer.
pub fn format_mac(mac: Mac, out: &mut [u8; 17]) -> usize {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut written = 0usize;
    for (index, byte) in mac.iter().enumerate() {
        out[written] = HEX[(byte >> 4) as usize];
        out[written + 1] = HEX[(byte & 0x0f) as usize];
        written += 2;
        if index < 5 {
            out[written] = b':';
            written += 1;
        }
    }
    written
}
