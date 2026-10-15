#!/usr/bin/env python3
"""The v2 shell's `nic` subcommands: `nic sniff` (frames from the network, decoded), `nic discover` and `nic peers`
(another fos machine answers with its name), `nic map` (a guest with two NICs cabled through a QEMU hub hears itself)
and `nic use`."""

import re
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
            check("52:54:00:12:34:a2" in text and "bravo1" in text, "nic discover lists the other machine with the name from its hostname file", text[-500:])
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
            mark = c.send("nic park 1")
            c.pump(0.5)
            check("nic 1: it has a link." in c.text()[mark:], "nic park reads the PHY and refuses an interface with a link", c.text()[mark:])
            mark = c.send("nic unpark 1")
            c.pump(0.5)
            check("nic 1: it is not parked." in c.text()[mark:], "and nic unpark an interface that is not parked", c.text()[mark:])

            # The multicast hash filter: only a joined group gets through the card (RCTL.MPE stays off).
            def received(guest):
                mark = guest.send("nic")
                guest.wait_for("fos> ", 10, mark)
                found = re.search(r"frames: (\d+) received", guest.text()[mark:])
                return int(found.group(1)) if found else -1

            def burst(group):
                frame = bytes.fromhex("0100" + "5e0102" + group) + bytes.fromhex("525400aabbcc") + b"\x88\xb5" + bytes(60)
                for _ in range(40):
                    inject(frame)
                    time.sleep(0.005)
                a.pump(1.0)

            mark = a.send("nic mcast 1 join 01:00:5e:01:02:03")
            a.wait_for("join 01:00:5e:01:02:03 done", 10, mark)
            before = received(a)
            burst("04")
            idle = received(a) - before
            before = received(a)
            burst("03")
            joined = received(a) - before
            check(joined >= 40 and idle < 10, f"a joined multicast group arrives and another does not ({joined} against {idle} of 40)", a.text()[-600:])
            mark = a.send("nic mcast 1 leave 01:00:5e:01:02:03")
            a.wait_for("leave 01:00:5e:01:02:03 done", 10, mark)
            before = received(a)
            burst("03")
            left = received(a) - before
            check(left < 10, f"and it stops arriving after the leave ({left} of 40)", a.text()[-600:])
            mark = a.send("nic mcast 1 join 02:00:00:00:00:01")
            a.wait_for("no hash filter", 10, mark)
            check(True, "a unicast address is refused")
        finally:
            for guest in (a, b, c):
                guest.stop()
    print("nic tools: all checks passed")


if __name__ == "__main__":
    main()
