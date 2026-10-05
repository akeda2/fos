#!/usr/bin/env python3
"""The v2 shell's `nic` subcommands: `nic sniff` (frames from the network, decoded), `nic discover` and `nic peers`
(another fos machine answers with its name), `nic map` (a guest with two NICs cabled through a QEMU hub hears itself)
and `nic use`."""

import socket
import struct
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_audio_net import GROUP, PORT, Guest, checksum  # noqa: E402
from test_boot import check  # noqa: E402


def inject(frame):
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_TTL, 1)
    sock.sendto(frame, (GROUP, PORT))
    sock.close()


def arp_request(sender_mac, sender_ip, target_ip):
    return bytes.fromhex("ffffffffffff") + sender_mac + b"\x08\x06" + struct.pack(">HHBBH", 1, 0x0800, 6, 4, 1) + sender_mac + sender_ip + bytes(6) + target_ip


def udp_frame(sender_mac, source, destination, source_port, destination_port):
    udp = struct.pack(">HHHH", source_port, destination_port, 8 + 4, 0) + b"abcd"
    ip = bytes([0x45, 0, 0, 20 + len(udp), 0, 0, 0, 0, 64, 17, 0, 0]) + source + destination
    ip = ip[:10] + struct.pack(">H", checksum(ip)) + ip[12:]
    return bytes.fromhex("ffffffffffff") + sender_mac + b"\x08\x00" + ip + udp


def main():
    with tempfile.TemporaryDirectory() as tmp:
        a = Guest(tmp, "nic-a", "52:54:00:12:34:a1")
        b = Guest(tmp, "nic-b", "52:54:00:12:34:a2", files={"hostname": "bravo"})
        c = Guest(tmp, "nic-c", "52:54:00:12:34:a3", ["-netdev", "hubport,id=hp1,hubid=1", "-device", "e1000,netdev=hp1,mac=52:54:00:12:34:c1",
                                                        "-netdev", "hubport,id=hp2,hubid=1", "-device", "e1000,netdev=hp2,mac=52:54:00:12:34:c2"])
        try:
            for guest in (a, b, c):
                guest.wait_for("M3 complete")
                guest.wait_for("fos> ")

            mark = a.send("nic sniff")
            a.wait_for("Sniffing NIC 1", 30, mark)
            a.pump(0.5)
            peer = bytes.fromhex("525400aabbcc")
            inject(arp_request(peer, bytes([10, 1, 2, 4]), bytes([10, 1, 2, 3])))
            inject(udp_frame(peer, bytes([10, 1, 2, 4]), bytes([239, 255, 255, 255]), 9875, 9875))
            inject(udp_frame(peer, bytes([10, 1, 2, 4]), bytes([10, 1, 2, 9]), 40000, 53))
            a.pump(1.5)
            a.process.stdin.write(b"\x1b")
            a.process.stdin.flush()
            a.wait_for("Stopped:", 15, mark)
            text = a.text()[mark:]
            check("ARP who has 10.1.2.3? tell 10.1.2.4" in text, "nic sniff decodes an ARP request", text[-800:])
            check("UDP 9875 > 9875 (SAP)" in text and "UDP 40000 > 53 (DNS)" in text, "and UDP datagrams with their well-known ports", text[-800:])
            check("Stopped:" in text and "frame(s) shown" in text, "Escape stops it and it reports how many frames it showed", text[-300:])
            a.wait_for("fos> ", 10, mark)

            mark = a.send("nic discover")
            a.wait_for("fos> ", 30, mark)
            text = a.text()[mark:]
            check("52:54:00:12:34:a2" in text and "bravo" in text, "nic discover lists the other machine with the name from its hostname file", text[-500:])
            mark = a.send("nic peers")
            a.pump(1.0)
            check("bravo" in a.text()[mark:], "nic peers shows the table again", a.text()[mark:])

            mark = c.send("nic map")
            c.wait_for("fos> ", 40, mark)
            text = c.text()[mark:]
            check("NIC 2" in text and "heard by NIC 3" in text and "heard by NIC 2" in text, "nic map shows two interfaces cabled together hearing each other", text[-700:])
            check("NIC 1" in text and "heard by nobody" in text, "and an interface on another network heard by nobody", text[-700:])

            mark = c.send("nic use 2")
            c.pump(0.5)
            check("Using NIC 2" in c.text()[mark:], "nic use selects an interface", c.text()[mark:])
            mark = c.send("nic use 9")
            c.pump(0.5)
            check("Usage: nic use N" in c.text()[mark:], "and refuses one that does not exist", c.text()[mark:])
        finally:
            for guest in (a, b, c):
                guest.stop()
    print("v2 nic tools: all checks passed")


if __name__ == "__main__":
    main()
