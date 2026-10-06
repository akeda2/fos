#!/usr/bin/env python3
"""`recorder` (v2): meters and pre-roll from the moment the tape starts, a take started by hand, and takes started and
ended by the silence trigger. QEMU's USB audio has no capture side, so the tape uses its test tone (`tone=HZ`, with
`gate=ON,OFF` seconds of tone and silence). The WAV files are parsed from the disk image afterwards.
Run with `make test-v2-recorder`."""

import struct
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_audio_net import Guest, tone_frequency  # noqa: E402
from test_boot import check  # noqa: E402
from test_record import fosfs_files, wav  # noqa: E402


def loud(samples, start, end):
    return max((abs(s) for s in samples[start:end]), default=0)


def main():
    with tempfile.TemporaryDirectory() as tmp:
        guest = Guest(tmp, "recorder", "52:54:00:12:34:e2")
        image = Path(tmp) / "recorder.img"
        try:
            guest.wait_for("M3 complete")
            guest.wait_for("fos> ")
            # Silence trigger: tone 0-2 s, quiet 2-6 s, tone 6-8 s, quiet 8-12 s, ...
            mark = guest.send("run recorder auto tone=440 gate=2,4 pre=3 thresh=-40 sound=100 silence=1500")
            guest.wait_for("fos recorder", 30, mark)
            guest.wait_for("Saved 1 file", 30, mark)
            guest.wait_for("Recording disk1/take002.wav", 30, mark)
            guest.wait_for("Saved 1 file", 30, guest.text().index("Recording disk1/take002.wav", mark))
            guest.process.stdin.write(b"q")
            guest.process.stdin.flush()
            guest.wait_for("file(s) written", 30, mark)
            # Manual take with pre-roll: started by Space after 4 s, stopped 3 s later.
            mark = guest.send("run recorder man tone=440 pre=2")
            guest.wait_for("fos recorder", 30, mark)
            # Rename the files in the UI: F, erase the old name, type `EDTx` (letters are lower-cased), Backspace, Enter.
            for keys in (b"f", b"\x7f\x7f\x7f", b"EDTx", b"\x7f", b"\r"):
                guest.process.stdin.write(keys)
                guest.process.stdin.flush()
                guest.pump(0.4)
            guest.wait_for("Files will be called edtnnn.wav", 10, mark)
            guest.pump(3.0)
            guest.process.stdin.write(b" ")
            guest.process.stdin.flush()
            guest.wait_for("Recording disk1/edt001.wav", 10, mark)
            guest.pump(3.0)
            guest.process.stdin.write(b" ")
            guest.process.stdin.flush()
            guest.wait_for("Saved 1 file", 10, mark)
            guest.process.stdin.write(b"q")
            guest.process.stdin.flush()
            guest.wait_for("file(s) written", 30, mark)
            # Channel selection: the test tone is 440 Hz on channel 1 and 880 Hz on channel 2. `ch=2` records only the right
            # channel; in another run `C` picks channel 2 in the UI, and a bad choice is refused.
            mark = guest.send("run recorder cha ch=2 tone=440 pre=1")
            guest.wait_for("recording 2 (1)", 30, mark)
            guest.pump(1.5)
            for keys in (b" ",):
                guest.process.stdin.write(keys)
                guest.process.stdin.flush()
            guest.wait_for("Recording disk1/cha001.wav", 10, mark)
            guest.pump(2.0)
            guest.process.stdin.write(b" ")
            guest.process.stdin.flush()
            guest.wait_for("Saved 1 file", 15, mark)
            guest.process.stdin.write(b"q")
            guest.process.stdin.flush()
            guest.wait_for("file(s) written", 30, mark)
            mark = guest.send("run recorder chb tone=440 pre=1")
            guest.wait_for("recording all (2)", 30, mark)
            for keys in (b"c", b"7", b"\r"):
                guest.process.stdin.write(keys)
                guest.process.stdin.flush()
                guest.pump(0.4)
            guest.wait_for("The device has only 2 channels", 10, mark)
            for keys in (b"c", b"2", b"\r"):
                guest.process.stdin.write(keys)
                guest.process.stdin.flush()
                guest.pump(0.4)
            guest.wait_for("Recording channel(s) 2 (1 of 2)", 10, mark)
            guest.pump(1.5)
            guest.process.stdin.write(b" ")
            guest.process.stdin.flush()
            guest.wait_for("Recording disk1/chb001.wav", 10, mark)
            guest.pump(2.0)
            guest.process.stdin.write(b" ")
            guest.process.stdin.flush()
            guest.wait_for("Saved 1 file", 15, mark)
            guest.process.stdin.write(b"q")
            guest.process.stdin.flush()
            guest.wait_for("file(s) written", 30, mark)
        finally:
            guest.process.kill()
            guest.process.wait()
        files = fosfs_files(image)
        check({"take001.wav", "take002.wav", "edt001.wav"} <= set(files), "take001.wav, take002.wav and edt001.wav exist", str(sorted(files)))
        channels, rate, bits, samples, declared = wav(files["take001.wav"])
        check((channels, rate, bits) == (2, 48000, 16), "take001.wav is 2 channels, 48 kHz, 16 bit", "")
        seconds = len(samples) / rate
        check(3.2 < seconds < 4.2, f"take001.wav (tone 0-2 s, ends 1.5 s after) is {seconds:.1f} s", "")
        check(loud(samples, rate // 2, 3 * rate // 2) > 4000, "with the tone inside", "")
        channels, rate, bits, samples, declared = wav(files["take002.wav"])
        seconds = len(samples) / rate
        check(5.4 < seconds < 6.8, f"take002.wav (tone at 6-8 s, 3 s of pre-roll) is {seconds:.1f} s", "")
        # Tone starts at 6 s; the take began after silence ended take 1 (3.6 s) so the pre-roll holds ~2.4 s of quiet.
        check(loud(samples, 0, rate) == 0, "take002.wav starts with silence from the pre-roll", "")
        check(loud(samples, int(3.2 * rate), int(4.5 * rate)) > 4000, "and holds the tone after it", "")
        channels, rate, bits, samples, declared = wav(files["edt001.wav"])
        seconds = len(samples) / rate
        check(4.4 < seconds < 5.9, f"edt001.wav (2 s of pre-roll + 3 s taken by hand) is {seconds:.1f} s", "")
        check(loud(samples, 0, rate) > 4000, "and starts with tone from before the key press", "")
    for name in ("cha001.wav", "chb001.wav"):
        channels, rate, bits, samples, declared = wav(files[name])
        check(channels == 1, f"{name} has one channel", str(channels))
        measured = tone_frequency(samples, rate, samples[rate // 2:rate // 2 + rate])
        check(abs(measured - 880) < 12, f"{name} holds channel 2 (880 Hz), measured {measured:.0f}", "")
    print("v2 recorder: all checks passed")


if __name__ == "__main__":
    main()
