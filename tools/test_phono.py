#!/usr/bin/env python3
"""The `phono` tape (phono equaliser): booted with an HD Audio duplex codec on QEMU's `none` backend. Proves what QEMU can: the
screen mode starts, draws the response curve (green) and the controls, answers keys (m mono, ] next curve, s surround, x swap rear, w Save) and
writes phono.cfg; `phono service` then reads that file, runs without taking the screen and stops on Escape. The filter maths is
checked by the host tests of `crates/phono`; live listening needs real hardware. Run with `make test-phono`."""

import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_audio_net import Guest  # noqa: E402
from test_boot import check  # noqa: E402
from test_gfx import screenshot  # noqa: E402

MAC = "52:54:00:12:36:01"


def type_keys(guest, keys):
    for key in keys:
        guest.process.stdin.write(bytes([key]))
        guest.process.stdin.flush()
        guest.pump(0.3)


def main():
    with tempfile.TemporaryDirectory() as tmp:
        monitor = Path(tmp) / "monitor"
        guest = Guest(tmp, "phono", MAC, extra=["-monitor", f"unix:{monitor},server,nowait", "-audiodev", "none,id=snd",
                                          "-device", "intel-hda", "-device", "hda-duplex,audiodev=snd"])
        try:
            guest.wait_for("M3 complete")
            guest.wait_for("fos> ")
            mark = guest.send("run phono")
            guest.wait_for("Escape quits.", 30, mark)
            text = guest.text()[mark:]
            check("cannot" not in text and "no audio" not in text, "capture and playback start", text[-500:])
            check("screen, riaa" in text, "screen mode with the RIAA curve", text[-300:])
            time.sleep(2)
            width, height, pixel = screenshot(monitor, "phono.ppm")
            greens = sum(1 for x in range(0, width, 2) for y in range(0, height, 2) if pixel(x, y)[1] > pixel(x, y)[0] + 60)
            check(greens > 100, "the screen shows the live response curve")
            type_keys(guest, b"m]sx")
            # The name field (starts as "lp"): n, more characters (a capital folds down, the space is ignored), Enter; w saves it too.
            type_keys(guest, b"nTs t\r")
            # A take of about two seconds (silence from the null backend), stopped, then the decoded copy.
            type_keys(guest, b"t")
            time.sleep(2.5)
            type_keys(guest, b"t")
            type_keys(guest, b"d")
            time.sleep(4)
            type_keys(guest, b"w")
            guest.pump(1)
            width, height, pixel = screenshot(monitor, "phono2.ppm")
            guest.process.stdin.write(b"\x1b")
            guest.process.stdin.flush()
            guest.wait_for("phono: stopped.", 10, mark)

            mark = guest.send("cat phono.cfg")
            guest.wait_for("fos> ", 10, mark)
            text = guest.text()[mark:]
            check("curve=iec" in text and "mono=on" in text, "Save wrote phono.cfg with the changed settings", text[-300:])
            check("surround=sq" in text and "swap=on" in text, "the surround and swap-rear keys reach phono.cfg", text[-300:])
            check("name=lptst" in text, "the typed name reaches phono.cfg", text[-300:])
            mark = guest.send("ls")
            guest.wait_for("fos> ", 10, mark)
            text = guest.text()[mark:]
            check("lptst.lr.flac" in text, "the raw take was written under the typed name", text[-500:])
            check("lptst.flac" in text.replace("lptst.lr.flac", ""), "the decoded file was written beside it", text[-500:])

            mark = guest.send("run phono service bass=6")
            guest.wait_for("Escape quits.", 30, mark)
            text = guest.text()[mark:]
            check("service, iec" in text, "service mode reads phono.cfg and takes no screen", text[-300:])
            guest.process.stdin.write(b"\x1b")
            guest.process.stdin.flush()
            guest.wait_for("phono: stopped.", 10, mark)

            mark = guest.send("run phono service curve=nonsense")
            guest.wait_for("phono: no curve nonsense", 10, mark)
            check(True, "a bad curve name is refused")
        finally:
            guest.stop()
    print("phono: all checks passed")


if __name__ == "__main__":
    main()
