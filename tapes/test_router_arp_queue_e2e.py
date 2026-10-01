#!/usr/bin/env python3
"""End-to-end test: the first packet through the router survives an ARP miss.

A host on segment A pings a host on segment B through the router tape. The
router has never heard from the destination, so it must ARP for it first. It
used to drop the packet that triggered the ARP request (relying on the sender
to try again), so the first ping of every conversation was lost or answered a
second later. Now it holds that one packet and sends it as soon as the ARP
reply arrives.

This script plays both hosts and both wires (see test_nettest_e2e.py: a TCP
point-to-point netdev per segment). Run it with `make test-tape-router-arp`.
"""

import struct
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_nettest_e2e import (  # noqa: E402
    CHAT_ETHERTYPE, Guest as _Guest, Segment, TERM_DATA, TERM_HELLO,
)
from test_router_e2e import arp_request, checksum, icmp_echo, ipv4_frame  # noqa: E402
import test_nettest_e2e as base  # noqa: E402

ROUTER_MAC_A = base.NIC_A
ROUTER_MAC_B = base.NIC_B
HOST_A_MAC = bytes.fromhex("525400ce0a01")
HOST_B_MAC = bytes.fromhex("525400ce0b01")
HOST_A_IP = bytes((10, 0, 1, 2))
HOST_B_IP = bytes((10, 0, 2, 2))
ROUTER_A_IP = bytes((10, 0, 1, 1))
ROUTER_B_IP = bytes((10, 0, 2, 1))


def arp_reply(sender_mac, sender_ip, target_mac, target_ip):
    payload = struct.pack("!HHBBH6s4s6s4s", 1, 0x0800, 6, 4, 2, sender_mac, sender_ip, target_mac, target_ip)
    frame = bytearray(target_mac + sender_mac + b"\x08\x06" + payload)
    frame.extend(b"\0" * max(0, 60 - len(frame)))
    return bytes(frame)


def wait_for(segment, predicate, timeout):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        for when, frame in segment.frames(0.05):
            if predicate(frame):
                return when, frame
    return None


def main():
    seg_a, seg_b = Segment(base.SEGMENT_A_PORT), Segment(base.SEGMENT_B_PORT)
    guest = _Guest(256)
    seg_a.accept()
    seg_b.accept()
    try:
        guest.wait_for(b"Waiting for a remote terminal client", 0, 40)
        seg_a.send(base.term_frame(TERM_HELLO))
        guest.wait_for(b"fos> ", 0, 20)
        mark = len(guest.output)
        seg_a.send(base.term_frame(TERM_DATA, b"tape load disk router.bin\n"))
        guest.wait_for(b"Loaded ", mark, 15)
        guest.wait_for(b"fos> ", mark, 10)
        mark = len(guest.output)
        seg_a.send(base.term_frame(TERM_DATA, b"tape run router no-nat 1 10.0.1.1/24 2 10.0.2.1/24\n"))
        guest.wait_for(b"fos> " if False else b"Running ", mark, 15)
        guest.pump(2.0)
        list(seg_a.frames())
        list(seg_b.frames())

        # Host A learns the router's MAC the ordinary way.
        seg_a.send(arp_request(HOST_A_MAC, HOST_A_IP, ROUTER_A_IP))
        got = wait_for(seg_a, lambda f: f[12:14] == b"\x08\x06" and f[20:22] == b"\x00\x02", 3)
        assert got, "router did not answer ARP\n" + guest.text()

        # Ping across the router. Nobody on B has spoken yet.
        echo = icmp_echo(8, 0x4321, 1, b"first ping")
        seg_a.send(ipv4_frame(ROUTER_MAC_A, HOST_A_MAC, HOST_A_IP, HOST_B_IP, echo, protocol=1))
        asked = wait_for(
            seg_b, lambda f: f[12:14] == b"\x08\x06" and f[20:22] == b"\x00\x01" and f[38:42] == HOST_B_IP, 3)
        assert asked, "router did not ARP for the destination\n" + guest.text()
        print("PASS: the router ARPs for an unknown destination.")

        # The destination answers; the held echo request must now be forwarded
        # without anyone resending it.
        replied_at = time.monotonic()
        seg_b.send(arp_reply(HOST_B_MAC, HOST_B_IP, ROUTER_MAC_B, ROUTER_B_IP))
        forwarded = wait_for(
            seg_b,
            lambda f: f[12:14] == b"\x08\x00" and f[23] == 1 and f[34] == 8 and f[0:6] == HOST_B_MAC,
            2,
        )
        assert forwarded, "the first echo request was dropped instead of held during ARP\n" + guest.text()
        latency = forwarded[0] - replied_at
        assert latency < 0.5, f"held packet took {latency:.2f}s after the ARP reply"
        ttl = forwarded[1][22]
        assert ttl == 63, f"TTL not decremented exactly once: {ttl}"
        assert forwarded[1][38:42] == icmp_echo(8, 0x4321, 1, b"first ping")[4:8], "echo id/seq changed"
        print(f"PASS: the first echo request was forwarded {latency * 1000:.0f} ms after the ARP reply (TTL {ttl}).")

        # A second, different destination works the same way.
        other_ip = bytes((10, 0, 2, 3))
        other_mac = bytes.fromhex("525400ce0b02")
        seg_a.send(ipv4_frame(ROUTER_MAC_A, HOST_A_MAC, HOST_A_IP, other_ip,
                              icmp_echo(8, 0x4321, 2, b"second host"), protocol=1))
        asked = wait_for(
            seg_b, lambda f: f[12:14] == b"\x08\x06" and f[20:22] == b"\x00\x01" and f[38:42] == other_ip, 3)
        assert asked, "router did not ARP for the second destination"
        seg_b.send(arp_reply(other_mac, other_ip, ROUTER_MAC_B, ROUTER_B_IP))
        forwarded = wait_for(seg_b, lambda f: f[12:14] == b"\x08\x00" and f[0:6] == other_mac, 2)
        assert forwarded, "the packet held for the second destination was dropped"
        print("PASS: a packet held for a second destination is forwarded too.")

        # Once resolved, later packets go straight through.
        seg_a.send(ipv4_frame(ROUTER_MAC_A, HOST_A_MAC, HOST_A_IP, HOST_B_IP,
                              icmp_echo(8, 0x4321, 3, b"cached"), protocol=1))
        direct = wait_for(seg_b, lambda f: f[12:14] == b"\x08\x00" and f[0:6] == HOST_B_MAC, 1)
        assert direct, "a packet to an already-resolved host was not forwarded"
        print("PASS: later packets to a resolved host are forwarded immediately.")
    finally:
        guest.stop()
        seg_a.close()
        seg_b.close()
    print("PASS: the router holds the first packet across an ARP miss.")


if __name__ == "__main__":
    main()
