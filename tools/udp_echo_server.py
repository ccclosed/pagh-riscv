#!/usr/bin/env python3
"""Host-side UDP echo server for the pagh-riscv live socket test.

QEMU's user network maps the guest's `10.0.2.2:<port>` to the host's loopback,
so binding `127.0.0.1:<port>` here is exactly what the guest reaches. The
server answers one datagram with the same payload and exits, which makes a
failed run end quickly instead of hanging on a second packet.

Usage::

    python3 tools/udp_echo_server.py 9000
    python3 tools/udp_echo_server.py 9000 --timeout 60
"""

from __future__ import annotations

import argparse
import socket
import sys


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("port", type=int, help="host UDP port (also the guest's 10.0.2.2:PORT)")
    parser.add_argument("--timeout", type=float, default=60.0, help="seconds to wait for the datagram")
    parser.add_argument("--reply", default=None, help="reply text (default: echo the request bytes)")
    args = parser.parse_args()
    if not 0 < args.port < 65536:
        print("udp_echo_server: port outside 1..65535", file=sys.stderr)
        return 2

    server = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    server.settimeout(args.timeout)
    server.bind(("127.0.0.1", args.port))
    print(f"udp_echo_server: listening on 127.0.0.1:{args.port}", flush=True)
    try:
        data, peer = server.recvfrom(4096)
    except socket.timeout:
        print("udp_echo_server: timeout waiting for a datagram", file=sys.stderr)
        server.close()
        return 1
    print(f"udp_echo_server: {len(data)} bytes from {peer[0]}:{peer[1]}: {data!r}", flush=True)
    reply = args.reply.encode() if args.reply is not None else data
    server.sendto(reply, peer)
    print(f"udp_echo_server: echoed {len(reply)} bytes", flush=True)
    server.close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
