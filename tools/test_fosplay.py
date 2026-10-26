#!/usr/bin/env python3
"""fosplay plays a FLAC file from FOSFS into loop0 while `record device=loop0` captures it; the recording holds the tone
at the right pitch, for a 48 kHz file and for a 44.1 kHz one. Needs ffmpeg on the host (SKIP without). No audio
hardware or network needed. Run with `make test-fosplay`."""

import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_audio_net import Guest, tone_frequency  # noqa: E402
from test_boot import check  # noqa: E402
from test_loopback import ask, float_wav  # noqa: E402
from test_record import fosfs_files  # noqa: E402

MAC = "52:54:00:12:36:02"
TONE_HZ = 1000


def make_flac(path, rate):
    subprocess.run(["ffmpeg", "-v", "error", "-y", "-f", "lavfi", "-i", f"sine=frequency={TONE_HZ}:sample_rate={rate}:duration=12",
                    "-ac", "2", "-sample_fmt", "s16", str(path)], check=True)


def main():
    if not shutil.which("ffmpeg"):
        print("SKIP: fosplay (no ffmpeg)")
        return
    with tempfile.TemporaryDirectory() as tmp:
        files = {}
        for rate in (48000, 44100):
            path = Path(tmp) / f"t{rate}.flac"
            make_flac(path, rate)
            files[f"t{rate}.flac"] = path.read_bytes()
        guest = Guest(tmp, "fosplay", MAC, files=files)
        try:
            guest.wait_for("M3 complete")
            guest.wait_for("fos> ")
            for rate in (48000, 44100):
                take = f"take{rate // 1000}"
                guest.send(f"run record device=loop0 {take} 4 &0")
                guest.pump(2)
                guest.send(f"run fosplay device=loop0 play t{rate}.flac &0")
                deadline = time.monotonic() + 90
                while time.monotonic() < deadline:
                    if "record" not in ask(guest, "ps", "fos> "):
                        break
                    guest.pump(2)
                check("record" not in ask(guest, "ps", "fos> "), f"the {rate} Hz recording finishes", guest.text()[-600:])
                guest.pump(8)
        finally:
            guest.stop()
        out = fosfs_files(Path(tmp) / "fosplay.img")
        for rate in (48000, 44100):
            blob = out.get(f"take{rate // 1000}.wav")
            check(blob is not None, f"take{rate // 1000}.wav was written", str(sorted(out)) + guest.text()[-900:])
            samples, got = float_wav(blob)
            loud = [x for x in samples if abs(x) > 0.05]
            check(len(loud) > got // 2, f"{rate} Hz file: the recording holds sound ({len(loud)} of {len(samples)})", guest.text()[-600:])
            middle = len(loud) and len(samples) // 2
            measured = tone_frequency(samples, got, samples[middle - got // 8:middle + got // 8])
            check(abs(measured - TONE_HZ) < 20, f"{rate} Hz file: recorded tone is {measured:.0f} Hz (wanted {TONE_HZ})", "")
    print("PASS: fosplay")


if __name__ == "__main__":
    main()
