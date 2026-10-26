#!/usr/bin/env python3
"""Audio loopback devices: `loop0`..`loop3` are virtual audio devices. A service plays a WAV file into loop0 (`play
device=loop0`) while the foreground `record device=loop0` captures it; the recording holds the tone. Also: the `audio`
command lists the loopbacks, a second holder of the same end is refused, and `audio device loop0` is accepted. No audio
hardware is needed. Run with `make test-loopback`."""

import struct
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_audio_net import Guest, tone_frequency  # noqa: E402
from test_boot import check  # noqa: E402
from test_record import fosfs_files  # noqa: E402
from test_usb_audio import write_wav  # noqa: E402

MAC = "52:54:00:12:36:01"
TONE_HZ = 1000


def ask(guest, line, expect, tries=5, wait=8):
    """Repeats a harmless command until `expect` shows in its output (the shell may answer late while a service runs)."""
    for _ in range(tries):
        mark = len(guest.text())
        guest.send(line)
        end = time.monotonic() + wait
        while time.monotonic() < end:
            guest.pump(0.2)
            if expect in guest.text()[mark:]:
                guest.pump(0.5)
                return guest.text()[mark:]
    check(False, f"`{line}` answers with {expect!r}", guest.text()[-400:])


def float_wav(blob):
    """(samples of channel 0, rate) of a 32-bit float WAV (format tag 3)."""
    check(blob[:4] == b"RIFF" and blob[8:12] == b"WAVE", "a RIFF/WAVE header", blob[:16].hex())
    at, fmt = 12, None
    while at + 8 <= len(blob):
        tag, size = blob[at:at + 4], struct.unpack_from("<I", blob, at + 4)[0]
        if tag == b"fmt ":
            fmt = struct.unpack_from("<HHIIHH", blob, at + 8)
        if tag == b"data":
            check(fmt is not None and fmt[0] == 3 and fmt[5] == 32, "the take is a 32-bit float WAV", str(fmt))
            body = blob[at + 8:at + 8 + size]
            return struct.unpack(f"<{len(body) // 4}f", body[:len(body) // 4 * 4])[::fmt[1]], fmt[2]
        at += 8 + size
    check(False, "a data chunk", "")


def main():
    with tempfile.TemporaryDirectory() as tmp:
        tone = Path(tmp) / "tone.wav"
        write_wav(tone, 48000, 2, 2, TONE_HZ, 20)
        guest = Guest(tmp, "loop", MAC, files={"tone.wav": tone.read_bytes()})
        try:
            guest.wait_for("M3 complete")
            guest.wait_for("fos> ")

            listing = ask(guest, "audio", "Loopback 0")
            check("loop0" in listing, "`audio` lists the loopbacks and their names", listing)

            ask(guest, "audio device loop0", "loop0")
            ask(guest, "audio device auto", "fos> ")
            check("no such" not in guest.text().split("audio device loop0")[-1].split("fos> ")[0], "`audio device loop0` is accepted", "")

            # Each end has one holder: here the shell holds one end and a service asking for it is refused.
            ask(guest, "audio device loop0", "loop0")
            ask(guest, "audio start 48000 capture", "capture end: running")
            ask(guest, "run record device=loop0 other 1 &0", "code -3")
            check(True, "a listener on a capture end the shell holds is refused")
            ask(guest, "audio stop", "capture end: stopped")
            ask(guest, "audio start 48000 playback", "playback end: running")
            ask(guest, "run play device=loop0 tone.wav &0", "code -3")
            check(True, "a player on a playing end the shell holds is refused")
            ask(guest, "audio stop", "playback end: stopped")
            ask(guest, "audio device auto", "fos> ")

            # The listener first (with no capture end the playing end discards), then the player.
            guest.send("run record device=loop0 take 3 &0")
            guest.pump(3)
            guest.send("run play device=loop0 tone.wav &0")
            guest.pump(3)
            deadline = time.monotonic() + 90
            while time.monotonic() < deadline:
                if "record" not in ask(guest, "ps", "fos> "):
                    break
                guest.pump(2)
            check("record" not in ask(guest, "ps", "fos> "), "the recording service finishes its 3 s", guest.text()[-400:])
            guest.pump(1)
        finally:
            guest.stop()
        files = fosfs_files(Path(tmp) / "loop.img")
        blob = files.get("take.wav")
        check(blob is not None, "the recording take.wav was written", str(sorted(files)) + guest.text()[-900:])
        samples, rate = float_wav(blob)
        check(rate == 48000, f"the recording is 48000 Hz ({rate})", "")
        loud = [x for x in samples if abs(x) > 0.05]
        check(len(loud) > rate // 2, f"the recording holds sound ({len(loud)} loud samples of {len(samples)})", "")
        middle = len(samples) // 2
        measured = tone_frequency(samples, rate, samples[middle - rate // 4:middle + rate // 4])
        check(abs(measured - TONE_HZ) < 20, f"the recorded tone is {measured:.0f} Hz (wanted {TONE_HZ})", "")
    print("PASS: loopback")


if __name__ == "__main__":
    main()
