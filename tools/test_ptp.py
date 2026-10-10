#!/usr/bin/env python3
"""`ptp` between two v2 guests on one virtual segment: one is the PTP master, the other a slave.

The slave must find the master from its Announce, complete Sync/Follow_Up and Delay_Req/Delay_Resp
exchanges, lose no Sync and report a small offset and a positive path delay. The frames on the wire are
checked too: IPv4/UDP to 224.0.1.129, event messages on port 319 and general messages on 320, PTP version 2.
Run with `make test-ptp`."""

import re
import struct
import sys
import tempfile
import threading
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_audio_net import Guest, sniff_until  # noqa: E402
from test_boot import check  # noqa: E402

MASTER_MAC, SLAVE_MAC = "52:54:00:12:35:01", "52:54:00:12:35:02"


def ptp_messages(frames):
    """(destination port, message type, version, length) of every PTP-looking IPv4/UDP frame to 224.0.1.129."""
    found = []
    for _, data in frames:
        if len(data) < 14 + 20 + 8 + 34 or data[12:14] != b"\x08\x00" or data[14 + 9] != 17:
            continue
        if data[14 + 16:14 + 20] != bytes([224, 0, 1, 129]) or data[:6] != bytes([0x01, 0x00, 0x5E, 0x00, 0x01, 0x81]):
            continue
        header = (data[14] & 0x0F) * 4
        port = struct.unpack_from(">H", data, 14 + header + 2)[0]
        message = data[14 + header + 8:]
        found.append((port, message[0] & 0x0F, message[1] & 0x0F, struct.unpack_from(">H", message, 2)[0]))
    return found


def main():
    frames, stop = [], threading.Event()
    sniffer = threading.Thread(target=sniff_until, args=(stop, frames))
    sniffer.start()
    with tempfile.TemporaryDirectory() as tmp:
        master = Guest(tmp, "master", MASTER_MAC)
        slave = Guest(tmp, "slave", SLAVE_MAC)
        try:
            for guest in (master, slave):
                guest.wait_for("M3 complete")
                guest.wait_for("fos> ")
            master_mark = master.send("run ptp master ip=10.7.0.1/24 secs=24")
            master.wait_for("ptp master: NIC 1 10.7.0.1", 30, master_mark)
            slave_mark = slave.send("run ptp ip=10.7.0.2/24 secs=14")
            slave.wait_for("ptp: waiting for an Announce", 30, slave_mark)
            slave.wait_for("ptp: master 52:54:00:ff:fe:12:35:01", 20, slave_mark)
            check(True, "the slave finds the master from its Announce")
            slave.wait_for("ptp: offset mean", 40, slave_mark)
            text = slave.text()[slave_mark:]
            summary = re.search(r"ptp: (\d+) samples; announce (\d+) sync (\d+) follow-up (\d+) delay-resp (\d+) sync lost (\d+) late (\d+) steps (\d+)", text)
            check(summary is not None, "the slave prints a summary", text[-1200:])
            samples, announces, syncs, follow_ups, delay_resps, lost, _late, steps = map(int, summary.groups())
            check(samples >= 40, f"Sync/Follow_Up pairs complete at the master's 8 per second ({samples} samples)")
            check(announces >= 5 and syncs >= samples and follow_ups >= samples, "announce, sync and follow-up counts agree")
            check(delay_resps >= 8, f"Delay_Req/Delay_Resp exchanges complete ({delay_resps})")
            check(lost == 0 and steps == 0, f"no Sync lost and no clock step ({lost} lost, {steps} steps)")
            tail = re.search(r"offset mean (-?[\d.]+) us, sd ([\d.]+) us, min (-?[\d.]+) us, max (-?[\d.]+) us; path delay ([\d.]+) us", text)
            check(tail is not None, "the slave prints offset statistics", text[-600:])
            delay = float(tail.group(5))
            check(0 < delay < 5000, f"the path delay is positive and small ({delay} us)")
            slave.wait_for("ptp: jitter", 10, slave_mark)
            text = slave.text()[slave_mark:]
            rate = re.search(r"ptp: jitter ([\d.]+) us .*clock rate ([+-][\d.]+) ppm", text)
            check(rate is not None, "the slave reports jitter and the clock rate", text[-500:])
            jitter, ppm = float(rate.group(1)), float(rate.group(2))
            # Two QEMU guests do not share a time base, so the rate need not be zero (it was about -500 ppm).
            check(jitter < 2000, f"the per-second jitter is small ({jitter} us)")
            check(abs(ppm) < 5000, f"the clock rate difference is measured and plausible ({ppm} ppm)")
            check(bool(re.search(r"\d+s off +-?[\d.]+ sd", text)), "a line per second shows offset and spread")
            master.wait_for("delay requests answered.", 30, master_mark)
            check(re.search(r"([1-9]\d*) delay requests answered", master.text()[master_mark:]) is not None, "the master answered the Delay_Req messages")
        finally:
            stop.set()
            sniffer.join()
            master.stop()
            slave.stop()
    messages = ptp_messages(frames)
    kinds = {(port, kind) for port, kind, _, _ in messages}
    check({(319, 0), (320, 8), (320, 0xB), (319, 1), (320, 9)} <= kinds, "the wire carries Sync and Delay_Req on 319 and Follow_Up, Announce and Delay_Resp on 320", str(sorted(kinds)))
    check(all(version == 2 for _, _, version, _ in messages), "every message is PTP version 2")
    lengths = {kind: length for _, kind, _, length in messages}
    check(lengths.get(0) == 44 and lengths.get(8) == 44 and lengths.get(0xB) == 64 and lengths.get(9) == 54, "message lengths are 44 (Sync, Follow_Up, Delay_Req), 64 (Announce), 54 (Delay_Resp)", str(lengths))
    print("PASS: ptp")


if __name__ == "__main__":
    main()
