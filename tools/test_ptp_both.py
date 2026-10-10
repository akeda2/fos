#!/usr/bin/env python3
"""One `ptp master both` serves a UDP slave and an Ethernet (l2) slave at the same time.

Three guests share one virtual segment. Each slave must lock to the master over its own transport, lose no Sync,
and get its Delay_Req answered on that transport; the wire must carry the master's messages as IPv4/UDP and as
0x88F7 frames. `both` is refused for a slave and together with `l2`. Run with `make test-ptp-both`."""

import re
import sys
import tempfile
import threading
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_audio_net import Guest, sniff_until  # noqa: E402
from test_boot import check  # noqa: E402
from test_ptp import l2_messages, ptp_messages  # noqa: E402

MASTER_MAC, UDP_MAC, L2_MAC = "52:54:00:12:35:01", "52:54:00:12:35:02", "52:54:00:12:35:03"


def main():
    frames, stop = [], threading.Event()
    sniffer = threading.Thread(target=sniff_until, args=(stop, frames))
    sniffer.start()
    with tempfile.TemporaryDirectory() as tmp:
        master = Guest(tmp, "master", MASTER_MAC)
        udp = Guest(tmp, "udp", UDP_MAC)
        ether = Guest(tmp, "l2", L2_MAC)
        try:
            for guest in (master, udp, ether):
                guest.wait_for("M3 complete")
                guest.wait_for("fos> ")
            mark = udp.send("run ptp both secs=3")
            udp.wait_for("both is for a master", 20, mark)
            mark = udp.send("run ptp master l2 both secs=3")
            udp.wait_for("both is for a master", 20, mark)
            check(True, "both is refused for a slave and together with l2")

            master_mark = master.send("run ptp master both ip=10.7.0.1/24 secs=30")
            master.wait_for("and Ethernet (0x88F7)", 30, master_mark)
            udp_mark = udp.send("run ptp ip=10.7.0.2/24 secs=14")
            ether_mark = ether.send("run ptp l2 secs=14")
            for name, guest, mark in (("UDP", udp, udp_mark), ("Ethernet", ether, ether_mark)):
                guest.wait_for("ptp: master 52:54:00:ff:fe:12:35:01", 30, mark)
                guest.wait_for("path delay", 40, mark)
                text = guest.text()[mark:]
                summary = re.search(r"ptp: (\d+) samples; announce (\d+) sync (\d+) follow-up (\d+) delay-resp (\d+) sync lost (\d+) late (\d+) steps (\d+)", text)
                check(summary is not None, f"the {name} slave prints a summary", text[-1200:])
                samples, _, _, _, delay_resps, lost, _, steps = map(int, summary.groups())
                check(samples >= 40 and delay_resps >= 8, f"the {name} slave locks over its own transport ({samples} samples, {delay_resps} Delay_Resp)")
                check(lost == 0 and steps == 0, f"the {name} slave loses no Sync and sees no step ({lost} lost, {steps} steps)")
            master.wait_for("delay requests answered.", 40, master_mark)
        finally:
            stop.set()
            sniffer.join()
            master.stop()
            udp.stop()
            ether.stop()
    udp_kinds = {(port, kind) for port, kind, _, _ in ptp_messages(frames)}
    l2_kinds = {(event, kind) for event, kind, _, _ in l2_messages(frames)}
    check({(319, 0), (320, 8), (320, 0xB), (319, 1), (320, 9)} <= udp_kinds, "the wire carries the UDP exchange", str(sorted(udp_kinds)))
    check({(True, 0), (False, 8), (False, 0xB), (True, 1), (False, 9)} <= l2_kinds, "the wire carries the Ethernet exchange", str(sorted(l2_kinds)))
    print("PASS: ptp both")


if __name__ == "__main__":
    main()
