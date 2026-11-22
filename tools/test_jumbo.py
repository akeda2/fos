#!/usr/bin/env python3
"""Jumbo frames on the e1000 driver (`nic mtu`, docs/block-sharing.md section 7), three guests on one multicast segment.

- a: MTU 9000, `nic mtu 1 listen` (a 9 KB buffer): a 4000-byte frame injected by the host arrives whole, bytes intact
  (the guest prints its length and byte sum): the card spread it over two 2 KiB descriptors and `receive` joined them.
- b: MTU 9000, `nic mtu 1 send 6000`: a 6000-byte frame transmitted from one 12 KiB buffer reaches a, intact
  (three descriptors on the receiving side).
- c: default MTU. It refuses to send 3000 bytes, a 9001 MTU is refused, and a jumbo frame that reaches it is counted as
  dropped (never delivered truncated) while the guest carries on.

QEMU's e1000 takes jumbo frames; what only real hardware can show is listed in docs/block-sharing.md.
"""

import re
import socket
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_audio_net import GROUP, PORT, Guest  # noqa: E402
from test_boot import check  # noqa: E402

MAC_A, MAC_B, MAC_C = "52:54:00:12:34:b1", "52:54:00:12:34:b2", "52:54:00:12:34:b3"


def pattern_frame(source_mac, size):
    """What `nic mtu N send SIZE` builds: broadcast, our MAC, 0x88b5, payload byte i = i mod 251."""
    header = b"\xff" * 6 + bytes.fromhex(source_mac.replace(":", "")) + b"\x88\xb5"
    return header + bytes(i % 251 for i in range(size - 14))


def inject(frame):
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_TTL, 1)
    sock.sendto(frame, (GROUP, PORT))
    sock.close()


def jumbo_lines(text):
    return [(int(n), int(s)) for n, s in re.findall(r"jumbo frame of (\d+) bytes, byte sum (\d+)", text)]


def main():
    with tempfile.TemporaryDirectory() as tmp:
        a, b, c = Guest(tmp, "jumbo-a", MAC_A), Guest(tmp, "jumbo-b", MAC_B), Guest(tmp, "jumbo-c", MAC_C)
        try:
            for guest in (a, b, c):
                guest.wait_for("M3 complete")
                guest.wait_for("fos> ")
            for guest in (a, b):
                mark = guest.send("nic mtu 1 9000")
                guest.wait_for("fos> ", 20, mark)
                check("MTU 9000" in guest.text()[mark:], f"{guest.name}: nic mtu 1 9000 is accepted", guest.text()[mark:])
            mark = c.send("nic mtu 1 9001")
            c.wait_for("fos> ", 20, mark)
            check("refused" in c.text()[mark:], "c: a 9001 MTU is refused", c.text()[mark:])
            mark = c.send("nic mtu 1 send 3000")
            c.wait_for("fos> ", 20, mark)
            check("REFUSED" in c.text()[mark:], "c: a 3000-byte frame is refused at MTU 1500", c.text()[mark:])

            # Host to a: one frame of 4000 bytes, two descriptors.
            mark = a.send("nic mtu 1 listen")
            a.pump(1.0)
            frame = pattern_frame("52:54:00:aa:bb:cc", 4000)
            inject(frame)
            a.wait_for("listened 5 s", 20, mark)
            got = jumbo_lines(a.text()[mark:])
            check((4000, sum(frame)) in got, "a: a 4000-byte frame from the host arrives whole, bytes intact", a.text()[mark:])
            a.wait_for("fos> ", 20, mark)

            # b to a: 6000 bytes through the 12 KiB transmit buffer.
            mark = a.send("nic mtu 1 listen")
            a.pump(1.0)
            sent = b.send("nic mtu 1 send 6000")
            b.wait_for("fos> ", 20, sent)
            check("6000 bytes: sent" in b.text()[sent:], "b: a 6000-byte frame is accepted for sending", b.text()[sent:])
            a.wait_for("listened 5 s", 20, mark)
            got = jumbo_lines(a.text()[mark:])
            expected = pattern_frame(MAC_B, 6000)
            check((6000, sum(expected)) in got, "a: and arrives whole from b, bytes intact", a.text()[mark:])
            a.wait_for("fos> ", 20, mark)

            # c never raised its MTU: both jumbo frames were counted and dropped, nothing was cut, and it still answers.
            mark = c.send("nic mtu 1")
            c.wait_for("fos> ", 20, mark)
            text = c.text()[mark:]
            dropped = re.search(r"(\d+) jumbo frame\(s\) joined, (\d+) dropped", text)
            check(dropped is not None and int(dropped.group(2)) >= 2 and "MTU 1500" in text,
                  "c: jumbo frames at MTU 1500 are dropped and counted, the MTU stays 1500", text)
            mark = c.send("nic mtu 1 9000")
            c.wait_for("fos> ", 20, mark)
            check("MTU 9000" in c.text()[mark:], "c: and the interface still takes commands afterwards", c.text()[mark:])
        finally:
            for guest in (a, b, c):
                guest.stop()
    print("PASS: jumbo")


if __name__ == "__main__":
    main()
