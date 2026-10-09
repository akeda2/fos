#!/usr/bin/env python3
"""Machines announce their names on their own: no probe is needed before `nic peers` (or a tape) knows the others.

Guest A has a `hostname` file ("alpha"), guest B has none (so it is `fos-` and the last three MAC bytes). Both boot on one
segment; after a few seconds each knows the other's name. Then `audio-net list` on B shows A in the kernel's peer list
(the tape reads it with `peer_info`) and uses the kernel's name for itself, and `to=alpha1` over raw Ethernet resolves
without any stream announcement from A."""

import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_audio_net import Guest  # noqa: E402
from test_boot import check  # noqa: E402


def main():
    with tempfile.TemporaryDirectory() as tmp:
        a = Guest(tmp, "host-a", "52:54:00:12:34:b1", files={"hostname": "alpha"})
        b = Guest(tmp, "host-b", "52:54:00:12:34:b2")
        try:
            for guest in (a, b):
                guest.wait_for("M3 complete")
                guest.wait_for("fos> ")
            a.pump(8)
            b.pump(1)

            mark = a.send("nic peers")
            a.wait_for("fos> ", 10, mark)
            text = a.text()[mark:]
            check("52:54:00:12:34:b2" in text and "fos-1234b2" in text, "A knows B by its default name without probing", text[-400:])
            mark = b.send("nic peers")
            b.wait_for("fos> ", 10, mark)
            text = b.text()[mark:]
            check("52:54:00:12:34:b1" in text and "alpha1" in text, "B knows A by its hostname file's name without probing", text[-400:])

            mark = b.send("run audio-net list proto=raw secs=2")
            b.wait_for("fos> ", 30, mark)
            text = b.text()[mark:]
            check("alpha1" in text and "52:54:00:12:34:b1" in text, "audio-net list shows the machines the kernel knows", text[-600:])

            mark_a = a.send("run audio-net send proto=raw src=tone=440 secs=14")
            a.pump(2)
            mark = b.send("run audio-net list proto=raw secs=6")
            b.wait_for("fos> ", 30, mark)
            text = b.text()[mark:]
            check("alpha1" in text and "sends" in text, "A's stream announcement carries its hostname-file name", text[-700:])
            a.wait_for("fos> ", 30, mark_a)

            mark = b.send("run audio-net send proto=raw to=alpha1 src=tone=440 secs=2")
            b.wait_for("fos> ", 40, mark)
            text = b.text()[mark:]
            check("'alpha1' is 52:54:00:12:34:b1" in text, "to=NAME is resolved from the kernel's table (no announcement from A)", text[-800:])
            check("fos-1234b2" in text or "Sending" in text or "sending" in text, "audio-net goes on to send", text[-800:])
        finally:
            for guest in (a, b):
                guest.stop()
    print("hostnames: all checks passed")


if __name__ == "__main__":
    main()
