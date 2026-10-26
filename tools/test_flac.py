#!/usr/bin/env python3
"""The FLAC decoder (`crates/audio-decode`) against ffmpeg: FLAC files made by ffmpeg in several shapes (16 and 24 bit,
mono and stereo, 44.1 to 96 kHz, compression levels 0 and 8, a quiet signal) must decode bit for bit like ffmpeg's own
decode. Host only; SKIP without ffmpeg. Run with `make test-flac`."""

import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CRATE = ROOT / "crates" / "audio-decode"

# (name, rate, channels, sample format, ffmpeg compression level, volume)
CASES = [("s16-l0", 44100, 2, "s16", 0, 0.8), ("s16-l8", 44100, 2, "s16", 8, 0.8), ("s24-96k", 96000, 2, "s32", 5, 0.8),
         ("mono", 48000, 1, "s16", 5, 0.8), ("quiet", 44100, 2, "s16", 5, 0.002), ("s16-32k", 32000, 2, "s16", 8, 1.0)]


def main():
    if not shutil.which("ffmpeg"):
        print("SKIP: flac (no ffmpeg)")
        return
    subprocess.run(["cargo", "+nightly-2026-09-13", "build", "--release", "--example", "decode", "--manifest-path",
                    str(CRATE / "Cargo.toml")], check=True, capture_output=True)
    decode = CRATE / "target" / "release" / "examples" / "decode"
    failed = 0
    with tempfile.TemporaryDirectory() as tmp:
        for name, rate, channels, fmt, level, volume in CASES:
            flac, ours, ref = (Path(tmp) / f"{name}.{ext}" for ext in ("flac", "ours", "ref"))
            # A tone, a second partial and a little noise, so every predictor and residual path gets used.
            expr = f"{volume}*(0.6*sin(2*PI*440*t)+0.3*sin(2*PI*3100*t)+0.1*(random(0)-0.5))"
            subprocess.run(["ffmpeg", "-v", "error", "-y", "-f", "lavfi", "-i", f"aevalsrc={expr}:s={rate}:d=3", "-ac", str(channels),
                            "-sample_fmt", fmt, "-compression_level", str(level), str(flac)], check=True)
            subprocess.run([str(decode), str(flac), str(ours)], check=True, capture_output=True)
            width = 3 if fmt == "s32" else 2
            codec = {2: "pcm_s16le", 3: "pcm_s24le"}[width]
            subprocess.run(["ffmpeg", "-v", "error", "-y", "-i", str(flac), "-f", codec.removeprefix("pcm_"), "-acodec", codec,
                            str(ref)], check=True)
            same = ours.read_bytes() == ref.read_bytes()
            print(f"{'PASS' if same else 'FAIL'}: {name}: {ours.stat().st_size} bytes {'identical to' if same else 'DIFFERENT from'} ffmpeg")
            failed += not same
    if failed:
        raise SystemExit(f"FAIL: flac ({failed} case(s))")
    print("PASS: flac")


if __name__ == "__main__":
    main()
