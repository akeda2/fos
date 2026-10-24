#!/usr/bin/env python3
"""The `strings` tape (string machine): booted with a USB sound card whose output QEMU records to a WAV file. The tape starts,
draws its screen (blue faders and controls appear), is silent until a key is typed, and typing `z` (C3, 130.8 Hz) puts that
pitch into the recording; Escape returns the console. MIDI input needs a USB-MIDI device and stays a real-hardware check.
Run with `make test-strings`."""

import math
import struct
import sys
import tempfile
import time
import wave
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_audio_net import Guest  # noqa: E402
from test_boot import check  # noqa: E402
from test_gfx import screenshot  # noqa: E402

MAC = "52:54:00:12:36:01"
C3 = 130.81


def samples(path):
    with wave.open(str(path), "rb") as w:
        channels, width, rate, frames = w.getnchannels(), w.getsampwidth(), w.getframerate(), w.getnframes()
        raw = w.readframes(frames)
    check(width == 2, f"the recording is 16-bit (got {width})")
    values = struct.unpack(f"<{len(raw) // 2}h", raw)
    return [v / 32768 for v in values[::channels]], rate


def goertzel(data, rate, freq):
    k = 2 * math.cos(2 * math.pi * freq / rate)
    s1 = s2 = 0.0
    for x in data:
        s1, s2 = x + k * s1 - s2, s1
    return math.sqrt(max(s1 * s1 + s2 * s2 - k * s1 * s2, 0)) / len(data)


def main():
    with tempfile.TemporaryDirectory() as tmp:
        monitor, wav = Path(tmp) / "monitor", Path(tmp) / "out.wav"
        guest = Guest(tmp, "strings", MAC, extra=["-monitor", f"unix:{monitor},server,nowait", "-device", "qemu-xhci,id=xhci",
                                                  "-audiodev", f"wav,id=snd,path={wav}", "-device", "usb-audio,audiodev=snd"])
        try:
            guest.wait_for("M3 complete")
            guest.wait_for("fos> ")
            mark = guest.send("run strings")
            guest.wait_for("Escape quits.", 30, mark)
            time.sleep(2)
            width, height, pixel = screenshot(monitor, "strings.ppm")
            blues = sum(1 for x in range(0, width, 2) for y in range(0, height, 2) if pixel(x, y)[2] > pixel(x, y)[0] + 40)
            check(blues > 200, "the screen shows the blue controls")
            guest.pump(1)
            for _ in range(10):
                guest.process.stdin.write(b"z")
                guest.process.stdin.flush()
                guest.pump(0.25)
            guest.pump(1)
            guest.process.stdin.write(b"\x1b")
            guest.process.stdin.flush()
            guest.wait_for("strings: stopped.", 10, mark)
        finally:
            guest.stop()
        time.sleep(1)
        data, rate = samples(wav)
        check(len(data) > rate, "the recording holds more than a second of audio")
        loud = max(range(0, len(data) - rate // 4, rate // 4), key=lambda i: sum(abs(x) for x in data[i:i + rate // 4]))
        window = data[loud:loud + rate // 4]
        peak = max(abs(x) for x in window)
        check(peak > 0.01, f"typing a key makes sound (peak {peak:.3f})")
        wanted, other = goertzel(window, rate, C3), goertzel(window, rate, 200.0)
        check(wanted > 5 * other, f"the sound is at C3 ({wanted:.4f} at 130.8 Hz vs {other:.4f} at 200 Hz)")
    print("strings: all checks passed")


if __name__ == "__main__":
    main()
