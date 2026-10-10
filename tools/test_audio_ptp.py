#!/usr/bin/env python3
"""`audio-net` on a PTP clock: three guests on one segment, a `ptp master`, an RTP sender and an RTP receiver.

The sender must lock to the master before it captures, stamp its RTP packets on the PTP timeline and name the
grandmaster in its SDP (`a=ts-refclk:ptp=`). The receiver must measure sender-capture-to-arrival from those stamps.
A second sender with `ptp=off` keeps `localmac` in its SDP. QEMU guests share no time base, so only the
plausibility of the figures is checked. Run with `make test-audio-ptp`."""

import re
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_audio_net import Guest  # noqa: E402
from test_boot import check  # noqa: E402

MASTER_MAC, SENDER_MAC, RECEIVER_MAC = "52:54:00:12:36:01", "52:54:00:12:36:02", "52:54:00:12:36:03"


def main():
    with tempfile.TemporaryDirectory() as tmp:
        wav = Path(tmp) / "out.wav"
        master = Guest(tmp, "master", MASTER_MAC)
        sender = Guest(tmp, "sender", SENDER_MAC)
        receiver = Guest(tmp, "receiver", RECEIVER_MAC,
                         ["-device", "qemu-xhci,id=xhci", "-audiodev", f"wav,id=snd,path={wav}", "-device", "usb-audio,audiodev=snd"])
        guests = (master, sender, receiver)
        try:
            for guest in guests:
                guest.wait_for("M3 complete")
                guest.wait_for("fos> ")
            mark = master.send("run ptp master ip=10.7.0.1/24 secs=120")
            master.wait_for("ptp master: NIC 1 10.7.0.1", 30, mark)
            mark_r = receiver.send("run audio-net recv proto=rtp ip=10.7.0.3/24 chan=2 bits=24")
            receiver.wait_for("Listening on NIC 1 for RTP/UDP", 30, mark_r)
            receiver.pump(3.0)
            mark_s = sender.send("run audio-net send proto=rtp ip=10.7.0.2/24 to=10.7.0.3 src=tone=660 secs=8")
            sender.wait_for("PTP: locked to a clock", 40, mark_s)
            check(True, "the sender locks to the master before it starts")
            sender.wait_for("Sent ", 60, mark_s)
            text = sender.text()[mark_s:]
            check("a=ts-refclk:ptp=IEEE1588-2008:52-54-00-FF-FE-12-36-01:0" in text, "its SDP names the grandmaster", text[-900:])
            check("a=mediaclk:direct=0" in text, "and the media clock", text[-900:])
            receiver.pump(1.0)
            mark_e = len(receiver.text())
            receiver.process.stdin.write(b"\x1b")  # Escape: stop the tape
            receiver.process.stdin.flush()
            receiver.wait_for("Received ", 30, mark_e - 200)
            receiver.wait_for("fos> ", 30, mark_e)
            report = receiver.text()[mark_r:]
            check("Locked onto a stream" in report, "the receiver locks onto the stream", report[-900:])
            found = re.search(r"PTP: capture to arrival (-?\d+) us on average \((-?\d+) to (-?\d+)\).* about (-?\d+) us .*\((\d+) packets\)", report)
            check(found is not None, "the receiver reports a PTP delay", report[-900:])
            average, low, high, total, packets = map(int, found.groups())
            check(packets >= 100, f"from {packets} stamped packets")
            # Two QEMU guests have no common time base, so what is checked is that stamps are on one timeline
            # (a stream that is not stamped gives delays far outside this window and is rejected).
            check(-5000 <= low <= high <= 500_000, f"the delay lies in a plausible window ({low} to {high} us)")
            check(total > 0, f"and the total is positive ({total} us)")

            mark_s = sender.send("run audio-net send proto=rtp ptp=off ip=10.7.0.2/24 to=239.1.2.3 src=tone=660 secs=2")
            sender.wait_for("Sent ", 40, mark_s)
            text = sender.text()[mark_s:]
            check("a=ts-refclk:localmac=" in text and "ptp=IEEE" not in text and "PTP: locked" not in text, "ptp=off keeps localmac and does not wait for a clock", text[-900:])
        finally:
            for guest in guests:
                guest.stop()
    print("PASS: audio-ptp")


if __name__ == "__main__":
    main()
