#!/usr/bin/env python3
"""The `gpiopanel` tape: local pins (`gpio-sim`) and the pins of another machine over the network.

alpha has a simulated pin bank and allows remote writes; bravo has the same `synckey`. On alpha the tape starts armed, GPIO17
(the selected pad) is made an output with `o` and driven high with `h`; after Escape `gpio read 17` shows it. On bravo the
tape runs `host=alpha1`, and the same keys change alpha's pin over Ethernet. A tape without ARM changes nothing. The screen
is checked only for the pad colours (output high is orange). Touch and the real pads stay a Pi 4 check.
Run with `make test-gpiopanel`."""

import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_audio_net import Guest  # noqa: E402
from test_boot import check  # noqa: E402


def run(guest, line, timeout=40):
    mark = guest.send(line)
    guest.wait_for("fos> ", timeout, mark)
    return guest.text()[mark:]


def keys(guest, text, pause=0.4):
    for ch in text:
        guest.process.stdin.write(ch.encode())
        guest.process.stdin.flush()
        guest.pump(pause)


def main():
    with tempfile.TemporaryDirectory() as tmp:
        alpha = Guest(tmp, "gpp-a", "52:54:00:12:37:a1", files={"hostname": "alpha", "synckey": "correct horse\n", "settings": "gpio-sim\ngpio-remote\n"})
        bravo = Guest(tmp, "gpp-b", "52:54:00:12:37:b2", files={"hostname": "bravo", "synckey": "correct horse\n"})
        try:
            for guest in (alpha, bravo):
                guest.wait_for("M3 complete")
                guest.wait_for("fos> ")

            out = run(bravo, "run gpiopanel")
            check("no GPIO pins" in out, "a machine without pins says so", out[-300:])

            mark = alpha.send("run gpiopanel")
            alpha.wait_for("Escape quits.", 30, mark)
            keys(alpha, "oh")
            alpha.pump(0.5)
            keys(alpha, "\x1b", 0.2)
            alpha.wait_for("gpiopanel: stopped.", 10, mark)
            alpha.wait_for("fos> ", 10, mark)
            out = run(alpha, "gpio read 17")
            check("in" in out and "high" not in out, "without ARM nothing changes", out[-300:])

            mark = alpha.send("run gpiopanel armed")
            alpha.wait_for("Escape quits.", 30, mark)
            keys(alpha, "oh")
            alpha.pump(0.5)
            keys(alpha, "\x1b", 0.2)
            alpha.wait_for("gpiopanel: stopped.", 10, mark)
            alpha.wait_for("fos> ", 10, mark)
            out = run(alpha, "gpio read 17")
            check("out" in out and "high" in out, "armed: o and h drive GPIO17 high locally", out[-300:])
            run(alpha, "gpio mode 17 in")
            alpha.pump(2.5)

            mark = bravo.send("run gpiopanel host=alpha1 armed")
            bravo.wait_for("Escape quits.", 30, mark)
            bravo.pump(2)
            keys(bravo, "oh", 1.0)
            bravo.pump(1)
            keys(bravo, "\x1b", 0.2)
            bravo.wait_for("gpiopanel: stopped.", 10, mark)
            bravo.wait_for("fos> ", 10, mark)
            time.sleep(2.5)
            out = run(alpha, "gpio read 17")
            check("out" in out and "high" in out, "bravo drives alpha's GPIO17 over the network", out[-300:])
        finally:
            for guest in (alpha, bravo):
                guest.stop()
    print("gpiopanel: all checks passed")


if __name__ == "__main__":
    main()
