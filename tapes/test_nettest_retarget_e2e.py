#!/usr/bin/env python3
"""nettest: re-point the generator, sink and impaired link from the prompt.

Same four-NIC lab as test_nettest_selftest_e2e.py. The tape starts as
GEN 1 -> [2 link 3] -> SINK 4, then `gen nic 4` and `sink nic 1` reverse the
direction without leaving the tape, and the traffic must arrive on NIC 1.
Run with `python3 tapes/test_nettest_retarget_e2e.py`."""

import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_nettest_selftest_e2e import Lab, check  # noqa: E402


def command(lab, line, wait=0.5):
    """Types LINE at the nettest prompt: read_key sees the local keyboard, so
    each character goes in through the QEMU monitor."""
    mark = len(lab.output)
    for char in line:
        lab.sendkey("spc" if char == " " else char)
    lab.sendkey("ret")
    lab.wait_for(b"nettest> ", mark, 20)
    lab.pump(wait)
    return lab.text(mark)


def main():
    lab = Lab()
    try:
        lab.shell("tape load disk nettest.bin", b"Loaded ")
        mark = len(lab.output)
        lab.process.stdin.write(b"tape run nettest 2 3 gen=1 sink=4 gen.size=500 gen.rate=2000 gen.count=200\n")
        lab.process.stdin.flush()
        lab.wait_for(b"nettest> ", mark, 30)
        text = command(lab, "gen nic 2")
        check("another role" in text, "a NIC in use by the link is refused", text)
        text = command(lab, "gen nic 9")
        check("use: nic N" in text, "a NIC that does not exist is refused", text)
        text = command(lab, "gen nic 4")
        check("another role" in text, "a NIC held by the sink is refused", text)
        text = command(lab, "sink nic off")
        check("switched off" in text, "sink nic off", text)
        text = command(lab, "gen nic 4")
        check("4 is now the NIC" in text, "generator moved to NIC 4", text)
        text = command(lab, "sink nic 1")
        check("1 is now the NIC" in text, "sink moved to NIC 1", text)
        text = command(lab, "gen start", wait=3)
        text += command(lab, "sink show")
        rx = re.search(r"rx (\d+)", text)
        check(rx is not None and int(rx.group(1)) >= 150, f"reversed stream reached NIC 1 ({rx and rx.group(1)} frames)", text)
        text = command(lab, "link 1 4")
        check("another role" in text, "link refuses NICs that have a role", text)
        text = command(lab, "link off")
        check("link off" in text, "link off", text)
        text = command(lab, "link 2 3")
        check("moved" in text, "link back on", text)
        lab.sendkey("f12")
        lab.wait_for(b"fos> ", mark, 30)
    finally:
        lab.stop()


if __name__ == "__main__":
    main()
