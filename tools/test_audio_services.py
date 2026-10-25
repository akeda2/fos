#!/usr/bin/env python3
"""Audio tapes as services: `run NAME device=N outdev=M &0` claims audio devices for a background tape. With two HD Audio
controllers (devices 1 and 2): the claim shows in `ps`, a second claim, `audio device 1` and `audio output 1` are refused while
the service runs, a foreground tape on the other device starts and ends without stopping the service's stream, and `kill`
gives the device back. Run with `make test-audio-services`."""

import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_audio_net import Guest  # noqa: E402
from test_boot import check  # noqa: E402

MAC = "52:54:00:12:36:01"


def ask(guest, line, expect, tries=5, wait=8):
    """Sends a command that is safe to repeat until `expect` shows in its output: with a service running, the shell now and
    then answers seconds late under QEMU (the commands this is used for refuse or only list, so a repeat does no harm)."""
    for _ in range(tries):
        mark = len(guest.text())
        guest.send(line)
        end = time.monotonic() + wait
        while time.monotonic() < end:
            guest.pump(0.2)
            if expect in guest.text()[mark:]:
                guest.pump(0.5)
                return
    check(False, f"`{line}` answers with {expect!r}", guest.text()[-400:])


def main():
    with tempfile.TemporaryDirectory() as tmp:
        extra = ["-audiodev", "none,id=snd1", "-device", "intel-hda,id=hda1", "-device", "hda-duplex,bus=hda1.0,audiodev=snd1",
                 "-audiodev", "none,id=snd2", "-device", "intel-hda,id=hda2", "-device", "hda-duplex,bus=hda2.0,audiodev=snd2"]
        guest = Guest(tmp, "phono", MAC, extra=extra)
        try:
            guest.wait_for("M3 complete")
            guest.wait_for("fos> ")

            mark = guest.send("run phono service device=1 &0")
            guest.wait_for("Escape quits.", 30, mark)
            guest.pump(2)
            ask(guest, "ps", "audio 1")
            check(True, "`ps` shows the service's audio device")

            ask(guest, "run phono service device=1 &0", "owned by service 1")
            check(True, "a second service cannot claim the same device")

            ask(guest, "audio device 1", "belongs to a service")
            ask(guest, "audio output 1", "belongs to a service")
            check(True, "`audio device` and `audio output` refuse a claimed device")

            ask(guest, "run phono service device=2", "Escape quits.", tries=3, wait=20)
            mark = len(guest.text()) - 600
            text = guest.text()[mark:]
            check("cannot" not in text and "no audio" not in text, "a foreground tape runs on the other device", text[-400:])
            guest.pump(1)
            guest.process.stdin.write(b"\x1b")
            guest.process.stdin.flush()
            guest.wait_for("phono: stopped.", 20, mark)
            guest.pump(1)

            ask(guest, "ps", "audio 1")
            check(True, "the service survived the foreground tape ending")

            ask(guest, "kill 1", "Service phono ended.", wait=15)
            guest.pump(1)
            mark = len(guest.text())
            ask(guest, "audio device 1", "Audio devices")
            check("belongs to a service" not in guest.text()[mark:], "`kill` gives the device back")
        finally:
            guest.stop()
    print("audio services: all checks passed")


if __name__ == "__main__":
    main()
