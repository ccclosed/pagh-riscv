//! The network stack's in-QEMU self-tests: the pure wire layer, driven with
//! fixed vectors and builder/parser round trips.
//!
//! Same contract as the other suites: non-destructive, deterministic, no state
//! outside a few stack buffers. The tests deliberately do not need a NIC — a
//! machine without one still runs them, which is what lets CI cover the network
//! code on every boot (the smoke test's `--net` mode covers the device and DHCP
//! end to end).
//!
//! Every check prints `SELFTEST net-<name> PASS/FAIL`; the summary and the frame
//! ledger match the shape of the other suites.

use super::wire::{self, Ipv4, Mac};

/// The RFC 1071 worked example, verbatim: these bytes sum to `0xddf2`, so the
/// checksum is `0x220d`.
const RFC1071: [u8; 8] = [0x00, 0x01, 0xf2, 0x03, 0xf4, 0xf5, 0xf6, 0xf7];

fn checksum_vector() -> bool {
    wire::checksum(&RFC1071) == 0x220d
}

fn ethernet_roundtrip() -> bool {
    let dst: Mac = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
    let src: Mac = [0x52, 0x54, 0x00, 0x65, 0x43, 0x21];
    let mut frame = [0u8; 64];
    let Some(offset) = wire::build_ethernet(&mut frame, dst, src, wire::ETH_IPV4) else {
        return false;
    };
    if offset != wire::ETH_HEADER {
        return false;
    }
    let roundtrip = match wire::parse_ethernet(&frame[..offset]) {
        Some((header, payload)) => {
            header.dst == dst
                && header.src == src
                && header.ethertype == wire::ETH_IPV4
                && payload == 14
        }
        None => false,
    };
    roundtrip && wire::parse_ethernet(&frame[..10]).is_none()
}

fn ipv4_roundtrip() -> bool {
    let src: Ipv4 = [10, 0, 2, 15];
    let dst: Ipv4 = [10, 0, 2, 2];
    let mut frame = [0u8; 64];
    let payload_off = match wire::build_ipv4(&mut frame, 0, src, dst, wire::IP_UDP, 8, 0x1234, 64) {
        Some(off) => off,
        None => return false,
    };
    if payload_off != wire::IPV4_HEADER {
        return false;
    }
    let valid = match wire::parse_ipv4(&frame, 0) {
        Some(packet) => {
            packet.src == src
                && packet.dst == dst
                && packet.protocol == wire::IP_UDP
                && packet.payload_offset == payload_off
                && packet.payload_len == 8
        }
        None => false,
    };
    // Corrupt one header byte: the checksum must reject it.
    let mut corrupt = frame;
    corrupt[8] = 63;
    let rejected = wire::parse_ipv4(&corrupt, 0).is_none();
    // A fragment (MF set) is dropped, not reassembled.
    let mut fragment = frame;
    fragment[6] = 0x20;
    let fragment_rejected = wire::parse_ipv4(&fragment, 0).is_none();
    valid && rejected && fragment_rejected
}

fn udp_roundtrip() -> bool {
    let src: Ipv4 = [10, 0, 2, 15];
    let dst: Ipv4 = [10, 0, 2, 3];
    let payload = b"pagh";
    let mut frame = [0u8; 64];
    let length = match wire::build_udp(&mut frame, 0, src, dst, 53001, 53, payload) {
        Some(len) => len,
        None => return false,
    };
    let parsed = match wire::parse_udp(&frame[..length]) {
        Some(udp) => {
            udp.src_port == 53001 && udp.dst_port == 53 && udp.payload_len == payload.len()
        }
        None => false,
    };
    let checksum_ok = wire::udp_checksum_ok(src, dst, &frame[..length]);
    let mut corrupt = [0u8; 64];
    corrupt[..length].copy_from_slice(&frame[..length]);
    corrupt[4] ^= 0xff; // length field
    let corrupted_rejected = !wire::udp_checksum_ok(src, dst, &corrupt[..length])
        || wire::parse_udp(&corrupt[..length]).is_none();
    parsed && checksum_ok && corrupted_rejected
}

fn icmp_roundtrip() -> bool {
    let data = [0x50u8, 0x41, 0x47, 0x48];
    let mut message = [0u8; 64];
    let length = match wire::build_icmp_echo(&mut message, 0, wire::ICMP_ECHO_REQUEST, 7, 9, &data)
    {
        Some(len) => len,
        None => return false,
    };
    let mut parsed = match wire::parse_icmp_echo(&message[..length], 0) {
        Some(echo) => echo,
        None => return false,
    };
    let good = parsed.kind == wire::ICMP_ECHO_REQUEST
        && parsed.id == 7
        && parsed.sequence == 9
        && &message[parsed.data_offset..parsed.data_offset + parsed.data_len] == data;
    // The reply builder flips the type and the checksum follows.
    let reply_len = match wire::build_icmp_echo(&mut message, 0, wire::ICMP_ECHO_REPLY, 7, 9, &data)
    {
        Some(len) => len,
        None => return false,
    };
    parsed = match wire::parse_icmp_echo(&message[..reply_len], 0) {
        Some(echo) => echo,
        None => return false,
    };
    good && parsed.kind == wire::ICMP_ECHO_REPLY
}

fn arp_roundtrip() -> bool {
    let sender: Mac = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
    let mut frame = [0u8; 64];
    let length = match wire::build_arp_frame(
        &mut frame,
        [0xff; 6],
        sender,
        wire::ARP_REQUEST,
        sender,
        [10, 0, 2, 15],
        [0; 6],
        [10, 0, 2, 2],
    ) {
        Some(len) => len,
        None => return false,
    };
    let Some((header, offset)) = wire::parse_ethernet(&frame[..length]) else {
        return false;
    };
    if header.ethertype != wire::ETH_ARP {
        return false;
    }
    let Some(arp) = wire::parse_arp(&frame[offset..length]) else {
        return false;
    };
    arp.operation == wire::ARP_REQUEST
        && arp.sender_mac == sender
        && arp.sender_ip == [10, 0, 2, 15]
        && arp.target_ip == [10, 0, 2, 2]
}

fn dhcp_build_and_parse() -> bool {
    let mac: Mac = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
    let mut message = [0u8; 300];
    let length = match wire::build_dhcp(
        &mut message,
        1,
        0x1122_3344,
        mac,
        wire::DHCP_DISCOVER,
        None,
        None,
    ) {
        Some(len) => len,
        None => return false,
    };
    if length != 300 {
        return false;
    }
    let Some(parsed) = wire::parse_dhcp(&message[..length]) else {
        return false;
    };
    if parsed.operation != 1
        || parsed.transaction != 0x1122_3344
        || parsed.client_mac != mac
        || parsed.options.message_type != wire::DHCP_DISCOVER
    {
        return false;
    }
    // A server-style message with the options this client consumes: build the
    // fixed header by hand so the parser is not tested against its own builder.
    let mut ack = [0u8; 300];
    ack[0] = 2; // reply
    ack[1] = 1;
    ack[2] = 6;
    let _ = wire::put_be32(&mut ack, 4, 0x1122_3344);
    ack[16..20].copy_from_slice(&[10, 0, 2, 15]); // yiaddr
    ack[28..34].copy_from_slice(&mac);
    let _ = wire::put_be32(&mut ack, 236, wire::DHCP_MAGIC);
    let options: &[u8] = &[
        53, 1, 5, // message type: ACK
        1, 4, 255, 255, 255, 0, // subnet mask
        3, 4, 10, 0, 2, 2, // router
        6, 4, 10, 0, 2, 3, // DNS
        51, 4, 0, 0, 0x0e, 0x10, // lease 3600 s
        54, 4, 10, 0, 2, 2, // server id
        255,
    ];
    ack[240..240 + options.len()].copy_from_slice(options);
    let Some(parsed) = wire::parse_dhcp(&ack[..300]) else {
        return false;
    };
    parsed.operation == 2
        && parsed.your_ip == [10, 0, 2, 15]
        && parsed.options.message_type == wire::DHCP_ACK
        && parsed.options.subnet_mask == Some([255, 255, 255, 0])
        && parsed.options.router == Some([10, 0, 2, 2])
        && parsed.options.dns == Some([10, 0, 2, 3])
        && parsed.options.lease_seconds == Some(3600)
        && parsed.options.server_id == Some([10, 0, 2, 2])
}

fn dns_roundtrip() -> bool {
    let mut query = [0u8; 128];
    let length = match wire::build_dns_query(&mut query, "example.com", 0x4242) {
        Some(len) => len,
        None => return false,
    };
    // Header (12) + labels: 7 "example" + 1 + 3 "com" + 1 + root + 4 QTYPE/QCLASS.
    if length != 12 + 1 + 7 + 1 + 3 + 1 + 4 {
        return false;
    }
    // A canned response: question echoed with a compression pointer answer.
    let mut response = [0u8; 64];
    let _ = wire::put_be16(&mut response, 0, 0x4242);
    let _ = wire::put_be16(&mut response, 2, 0x8180); // response, RD, RA
    let _ = wire::put_be16(&mut response, 4, 1); // one question
    let _ = wire::put_be16(&mut response, 6, 1); // one answer
    let mut at = 12;
    response[at] = 7;
    response[at + 1..at + 8].copy_from_slice(b"example");
    at += 8;
    response[at] = 3;
    response[at + 1..at + 4].copy_from_slice(b"com");
    at += 4;
    response[at] = 0;
    at += 1;
    let _ = wire::put_be16(&mut response, at, 1); // A
    let _ = wire::put_be16(&mut response, at + 2, 1); // IN
    at += 4;
    response[at] = 0xc0; // pointer to the question name
    response[at + 1] = 0x0c;
    at += 2;
    let _ = wire::put_be16(&mut response, at, 1); // A
    let _ = wire::put_be16(&mut response, at + 2, 1); // IN
    let _ = wire::put_be32(&mut response, at + 4, 60); // TTL
    let _ = wire::put_be16(&mut response, at + 8, 4); // RDLENGTH
    response[at + 10..at + 14].copy_from_slice(&[93, 184, 216, 34]);
    at += 14;
    let resolved = wire::parse_dns_a(&response[..at], 0x4242) == Some([93, 184, 216, 34]);
    let wrong_transaction = wire::parse_dns_a(&response[..at], 0x4243).is_none();
    resolved && wrong_transaction
}

fn tcp_roundtrip() -> bool {
    let src: Ipv4 = [10, 0, 2, 15];
    let dst: Ipv4 = [93, 184, 216, 34];
    let mut frame = [0u8; 64];
    let length = match wire::build_tcp(
        &mut frame,
        0,
        src,
        dst,
        49152,
        80,
        0x1122_3344,
        0x5566_7788,
        wire::TCP_SYN,
        4096,
        &[],
    ) {
        Some(len) => len,
        None => return false,
    };
    let Some(parsed) = wire::parse_tcp(&frame[..length]) else {
        return false;
    };
    let good = parsed.src_port == 49152
        && parsed.dst_port == 80
        && parsed.sequence == 0x1122_3344
        && parsed.acknowledgement == 0x5566_7788
        && parsed.flags & wire::TCP_SYN != 0
        && parsed.payload_len == 0;
    let checksum = wire::tcp_checksum_ok(src, dst, &frame[..length]);
    let mut corrupt = frame;
    corrupt[4] ^= 0xff; // sequence number
    let rejected = !wire::tcp_checksum_ok(src, dst, &corrupt[..length]);
    good && checksum && rejected
}

fn text_roundtrip() -> bool {
    let ip = match wire::parse_ipv4_str("192.168.100.7") {
        Some(ip) => ip,
        None => return false,
    };
    if ip != [192, 168, 100, 7] || wire::parse_ipv4_str("192.168.100.256").is_some() {
        return false;
    }
    let mut text = [0u8; 15];
    let written = match wire::format_ipv4(ip, &mut text) {
        Some(len) => len,
        None => return false,
    };
    if &text[..written] != b"192.168.100.7" {
        return false;
    }
    let mut mac_text = [0u8; 17];
    let written = wire::format_mac([0x52, 0x54, 0x00, 0xab, 0xcd, 0xef], &mut mac_text);
    &mac_text[..written] == b"52:54:00:ab:cd:ef"
}

fn subnet_math() -> bool {
    let ip: Ipv4 = [10, 0, 2, 15];
    let mask: Ipv4 = [255, 255, 255, 0];
    wire::same_subnet(ip, mask, [10, 0, 2, 2])
        && !wire::same_subnet(ip, mask, [10, 0, 3, 2])
        && wire::is_limited_broadcast([255; 4])
        && wire::mac_is_broadcast([0xff; 6])
        && !wire::mac_is_broadcast([0x52, 0x54, 0x00, 0x12, 0x34, 0x56])
}

/// Run every network check, print `SELFTEST net-<name> PASS|FAIL`, and return how
/// many passed.
pub fn run() -> usize {
    let checks: [(&str, fn() -> bool); 11] = [
        ("net-checksum", checksum_vector),
        ("net-ethernet", ethernet_roundtrip),
        ("net-ipv4", ipv4_roundtrip),
        ("net-udp", udp_roundtrip),
        ("net-icmp", icmp_roundtrip),
        ("net-arp", arp_roundtrip),
        ("net-dhcp", dhcp_build_and_parse),
        ("net-dns", dns_roundtrip),
        ("net-tcp", tcp_roundtrip),
        ("net-text", text_roundtrip),
        ("net-subnet", subnet_math),
    ];
    let mut passed = 0usize;
    for (name, check) in checks {
        let ok = check();
        if ok {
            passed += 1;
        }
        crate::kprintln!("SELFTEST {} {}", name, if ok { "PASS" } else { "FAIL" });
    }
    crate::kprintln!("SELFTEST SUMMARY (net): {}/{} passed", passed, checks.len());
    passed
}
