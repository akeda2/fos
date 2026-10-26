#!/usr/bin/env python3
"""fosplay plays a FLAC or MP3 file from FOSFS into loop0 while `record device=loop0` captures it; the recording holds
the tone at the right pitch, for FLAC at 48 and 44.1 kHz and MP3 at 44.1 kHz stereo and 22.05 kHz mono. Needs ffmpeg on the host (SKIP without). No audio
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


# (file name, sample rate, channels, ffmpeg output options)
CASES = [("t48.flac", 48000, 2, ["-sample_fmt", "s16"]), ("t44.flac", 44100, 2, ["-sample_fmt", "s16"]),
         ("m44.mp3", 44100, 2, ["-b:a", "192k"]), ("m22.mp3", 22050, 1, ["-b:a", "64k"]),
         ("r44.flac", 48000, 2, ["-sample_fmt", "s16"])]
# Files played with an output rate asked for (rate=44100: the loopback may refuse it; fosplay then falls back and resamples).
PLAY_OPTIONS = {"r44.flac": "rate=44100 "}


def make_audio(path, rate, channels, options):
    subprocess.run(["ffmpeg", "-v", "error", "-y", "-f", "lavfi", "-i", f"sine=frequency={TONE_HZ}:sample_rate={rate}:duration=12",
                    "-ac", str(channels), *options, str(path)], check=True)


def main():
    if not shutil.which("ffmpeg"):
        print("SKIP: fosplay (no ffmpeg)")
        return
    with tempfile.TemporaryDirectory() as tmp:
        files = {}
        for name, rate, channels, options in CASES:
            path = Path(tmp) / name
            make_audio(path, rate, channels, options)
            files[name] = path.read_bytes()
        # A playlist of two short files (FLAC then MP3, different rates), played back to back.
        for name, rate, channels, options in [("p1.flac", 48000, 2, ["-sample_fmt", "s16"]), ("p2.mp3", 44100, 2, ["-b:a", "192k"])]:
            path = Path(tmp) / name
            subprocess.run(["ffmpeg", "-v", "error", "-y", "-f", "lavfi", "-i", f"sine=frequency={TONE_HZ}:sample_rate={rate}:duration=3",
                            "-ac", str(channels), *options, "-metadata", f"title=Song {name[1]}", "-metadata", "artist=Bj\u00f6rk Quartet",
                            str(path)], check=True)
            files[name] = path.read_bytes()
        guest = Guest(tmp, "fosplay", MAC, files=files)
        try:
            guest.wait_for("M3 complete")
            guest.wait_for("fos> ")
            for name, rate, _, _ in CASES:
                take = name.split(".")[0]
                guest.send(f"run record device=loop0 {take} 4 &0")
                guest.pump(2)
                guest.send(f"run fosplay device=loop0 {PLAY_OPTIONS.get(name, "")}play {name} &0")
                deadline = time.monotonic() + 90
                while time.monotonic() < deadline:
                    if "record" not in ask(guest, "ps", "fos> "):
                        break
                    guest.pump(2)
                check("record" not in ask(guest, "ps", "fos> "), f"the {name} recording finishes", guest.text()[-600:])
                guest.pump(8)
            # The playlist: both tracks play, the second named [2/2], and the summary counts them.
            guest.send("run record device=loop0 list 9 &0")
            guest.pump(2)
            guest.send("run fosplay device=loop0 play p1.flac p2.mp3 &0")
            deadline = time.monotonic() + 90
            while time.monotonic() < deadline:
                if "record" not in ask(guest, "ps", "fos> "):
                    break
                guest.pump(2)
            guest.pump(3)
            text = guest.text()
            check("Song 1 - Bj\u00f6rk Quartet" in text and "Song 2 - Bj\u00f6rk Quartet" in text, "titles and artists come from the Vorbis and ID3 tags", text[-800:])
            check("[1/2] p1.flac" in text and "[2/2] p2.mp3" in text, "the playlist names both tracks", text[-800:])
            check("2 track(s)" in text, "the playlist ends with a summary of 2 tracks", text[-800:])
        finally:
            guest.stop()
        out = fosfs_files(Path(tmp) / "fosplay.img")
        for name, rate, _, _ in CASES:
            take = name.split(".")[0]
            blob = out.get(f"{take}.wav")
            check(blob is not None, f"{take}.wav was written", str(sorted(out)) + guest.text()[-900:])
            samples, got = float_wav(blob)
            loud = [x for x in samples if abs(x) > 0.05]
            check(len(loud) > got // 2, f"{name}: the recording holds sound ({len(loud)} of {len(samples)})", guest.text()[-600:])
            middle = len(loud) and len(samples) // 2
            measured = tone_frequency(samples, got, samples[middle - got // 8:middle + got // 8])
            check(abs(measured - TONE_HZ) < 20, f"{name}: recorded tone is {measured:.0f} Hz (wanted {TONE_HZ})", "")
        blob = out.get("list.wav")
        check(blob is not None, "list.wav was written", str(sorted(out)))
        samples, got = float_wav(blob)
        loud = [i for i, x in enumerate(samples) if abs(x) > 0.05]
        check(len(loud) > 5 * got // 2, f"the playlist recording holds both tracks ({len(loud)} loud samples)", "")
    print("PASS: fosplay")


if __name__ == "__main__":
    main()
