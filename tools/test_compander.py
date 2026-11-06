#!/usr/bin/env python3
"""The `compander` tape (stereo dbx Type I/II compander box): booted with an HD Audio duplex codec on QEMU's `none` backend (silence
in, output swallowed), so this proves what QEMU can: the tape starts capture and playback together with the arguments it is
given (type, mode, tape standards, speed, tone, trim), takes the screen and draws the meters and buttons, answers typed keys
(e Encode, d Decode, 1/2 type, b Bypass, o tone, s/m/r equalisation, arrows trim and tone level) without
failing, rejects a bad argument and returns the console on Escape. The sound is checked by the host tests of `crates/compander`
and `tapes/compander`; levels, latency and the match with real dbx-encoded tapes need real hardware. Run with `make test-dbx`."""

import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_audio_net import Guest  # noqa: E402
from test_boot import check  # noqa: E402
from test_gfx import screenshot  # noqa: E402

MAC = "52:54:00:12:36:01"


def main():
    with tempfile.TemporaryDirectory() as tmp:
        monitor = Path(tmp) / "monitor"
        guest = Guest(tmp, "compander", MAC, extra=["-monitor", f"unix:{monitor},server,nowait", "-audiodev", "none,id=snd",
                                         "-device", "intel-hda", "-device", "hda-duplex,audiodev=snd"])
        try:
            guest.wait_for("M3 complete")
            guest.wait_for("fos> ")
            mark = guest.send("run compander type=3")
            guest.wait_for("type= takes 1 or 2.", 15, mark)
            guest.wait_for("fos> ", 15, mark)
            mark = guest.send("run compander source=tape")
            guest.wait_for("source= takes in or out.", 15, mark)
            guest.wait_for("fos> ", 15, mark)
            mark = guest.send("run compander tape=cassette")
            guest.wait_for("machine= and tape= take nab, iec", 15, mark)
            guest.wait_for("fos> ", 15, mark)
            mark = guest.send("run compander type=1 mode=encode machine=iec tape=nab speed=7.5 style=plot trim=-1.5 tone=pink level=-20 view=spectrum source=out")
            guest.wait_for("Escape quits.", 30, mark)
            text = guest.text()[mark:]
            check("Type I ENCODE" in text and "7.5 ips, tape NAB on machine IEC" in text, "the arguments are applied", text[-500:])
            check("cannot" not in text and "no audio" not in text, "capture and playback start together", text[-500:])
            time.sleep(2)
            width, height, pixel = screenshot(monitor, "compander.ppm")
            greens = sum(1 for x in range(0, width, 2) for y in range(0, height, 2) if pixel(x, y)[1] > pixel(x, y)[0] + 60)
            check(greens > 100, "the screen shows the meters and buttons")
            ambers = sum(1 for x in range(0, width, 2) for y in range(0, height, 2) if pixel(x, y)[0] > pixel(x, y)[2] + 100 and pixel(x, y)[1] > pixel(x, y)[2] + 60)
            check(ambers > 50, "the spectrum shows the pink noise on both channels", ambers)
            for key in [bytes([k]) for k in b"d2be1tosmrfiphs"] + [b"\x1b[A", b"\x1b[B", b"\x1b[C", b"\x1b[D"]:
                guest.process.stdin.write(key)
                guest.process.stdin.flush()
                guest.pump(0.3)
            guest.pump(1)
            guest.process.stdin.write(b"w")
            guest.process.stdin.flush()
            guest.pump(1)
            guest.process.stdin.write(b"\x1b")
            guest.process.stdin.flush()
            guest.wait_for("compander: stopped", 10, mark)
            text = guest.text()[mark:]
            check("overrun" not in text.lower(), "no errors while it ran", text[-500:])
            mark = guest.send("cat settings/compander.cfg")
            guest.wait_for("fos> ", 10, mark)
            text = guest.text()[mark:]
            check("type=2" in text and "mode=encode" in text and "speed=30" in text and "view=meters" in text and "style=bars" in text, "Save wrote compander.cfg with the settings", text[-300:])
            mark = guest.send("run compander")
            guest.wait_for("Escape quits.", 30, mark)
            text = guest.text()[mark:]
            check("Type II" in text, "the next run starts from compander.cfg", text[-300:])
            guest.process.stdin.write(b"\x1b")
            guest.process.stdin.flush()
            guest.wait_for("compander: stopped", 10, mark)
        finally:
            guest.stop()
    print("compander: all checks passed")


if __name__ == "__main__":
    main()
