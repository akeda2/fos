#!/usr/bin/env python3
"""The FLAC decoder (`crates/audio-decode`) against ffmpeg: FLAC files made by ffmpeg in several shapes (16 and 24 bit,
mono and stereo, 44.1 to 96 kHz, compression levels 0 and 8, a quiet signal) must decode bit for bit like ffmpeg's own
decode. Host only; SKIP without ffmpeg. Run with `make test-flac`."""

import os
import shutil
import struct
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CRATE = ROOT / "crates" / "audio-decode"

# (name, rate, channels, sample format, ffmpeg compression level, volume)
CASES = [("s16-l0", 44100, 2, "s16", 0, 0.8), ("s16-l8", 44100, 2, "s16", 8, 0.8), ("s24-96k", 96000, 2, "s32", 5, 0.8),
         ("mono", 48000, 1, "s16", 5, 0.8), ("quiet", 44100, 2, "s16", 5, 0.002), ("s16-32k", 32000, 2, "s16", 8, 1.0)]


def seek_cases(decode):
    """Seeking: after each chain of seeks (forward, backward, within a block, past the end) the samples that follow must be
    the file's own samples from the block start the decoder reports. Files with and without a SEEKTABLE (`flac` writes one)."""
    failed = 0
    rate, channels = 44100, 2
    chains = ["88200", "132300,44100", "100,50", "0", "176400,300", "5000000", "50000,50001,200000"]
    with tempfile.TemporaryDirectory() as tmp:
        plain, ref = Path(tmp) / "plain.flac", Path(tmp) / "ref"
        expr = "0.8*(0.6*sin(2*PI*440*t)+0.3*sin(2*PI*3100*t)+0.1*(random(0)-0.5))"
        subprocess.run(["ffmpeg", "-v", "error", "-y", "-f", "lavfi", "-i", f"aevalsrc={expr}:s={rate}:d=5", "-ac", str(channels),
                        "-sample_fmt", "s16", str(plain)], check=True)
        subprocess.run(["ffmpeg", "-v", "error", "-y", "-i", str(plain), "-f", "s16le", "-acodec", "pcm_s16le", str(ref)], check=True)
        files = [("no seektable", plain)]
        # A SEEKTABLE made from the decoder's own frame list (about every 0.5 s), spliced in after STREAMINFO.
        frames = Path(tmp) / "frames.txt"
        subprocess.run([str(decode), str(plain), str(frames), "frames"], check=True, capture_output=True)
        points = [tuple(map(int, line.split())) for line in frames.read_text().splitlines()]
        chosen, next_sample = [], 0
        for sample, offset in points:
            if sample >= next_sample:
                chosen.append((sample, offset))
                next_sample = sample + 22050
        chosen.append((2**64 - 1, 0, 0))  # a placeholder point, which a reader must ignore
        body = b"".join(struct.pack(">QQH", *point[:2], 4096) if len(point) == 2 else struct.pack(">QQH", *point) for point in chosen)
        data = plain.read_bytes()
        head, last = data[4], data[4] & 0x80
        table = Path(tmp) / "table.flac"
        table.write_bytes(data[:4] + bytes([head & 0x7F]) + data[5:42] + bytes([3 | last]) + len(body).to_bytes(3, "big") + body + data[42:])
        files.append(("SEEKTABLE", table))
        reference = ref.read_bytes()
        for label, flac in files:
            for chain in chains:
                out = Path(tmp) / "seek.raw"
                done = subprocess.run([str(decode), str(flac), str(out), chain], check=True, capture_output=True, text=True)
                start = int(done.stderr.split("seek:")[-1].split("->")[1].split()[0])
                got = out.read_bytes()
                last = min(int(chain.split(",")[-1]), len(reference) // 4 - 1)
                expected = reference[start * 4:]
                ok = got == expected and (start <= last or start >= len(reference) // 4) and (last - start < 16384 or start >= len(reference) // 4)
                print(f"{'PASS' if ok else 'FAIL'}: seek {chain} ({label}) lands on {start}")
                failed += not ok
    return failed


def mp3_seek_case():
    """MP3 seeks by bitrate: a constant-bitrate file with 500 Hz for 10 s and then 1500 Hz must, after a seek to 15 s, play
    1500 Hz, and a seek back to 2 s must play 500 Hz again. The returned position is the target."""
    import math
    import array
    subprocess.run(["cargo", "+nightly-2026-09-13", "build", "--release", "--example", "decode_mp3", "--manifest-path",
                    str(CRATE / "Cargo.toml")], check=True, capture_output=True)
    decode = Path(os.environ.get("CARGO_TARGET_DIR", CRATE / "target")) / "release" / "examples" / "decode_mp3"
    failed = 0
    with tempfile.TemporaryDirectory() as tmp:
        mp3 = Path(tmp) / "t.mp3"
        subprocess.run(["ffmpeg", "-v", "error", "-y", "-f", "lavfi", "-i",
                        "aevalsrc=0.5*sin(2*PI*if(lt(t\\,10)\\,500\\,1500)*t):s=44100:d=20", "-ac", "2", "-b:a", "128k", "-id3v2_version", "3",
                        "-metadata", "title=x", str(mp3)], check=True)
        for target, wanted in [(15 * 44100, 1500), (2 * 44100, 500), (0, 500)]:
            raw = Path(tmp) / "out.raw"
            done = subprocess.run([str(decode), str(mp3), str(raw), str(target)], check=True, capture_output=True, text=True)
            landed = int(done.stderr.split("seek:")[1].split("->")[1].split()[0])
            pcm = array.array("f")
            pcm.frombytes(raw.read_bytes()[: 4 * 2 * 44100])
            left = pcm[2 * 4410 : 2 * 4410 + 2 * 22050 : 2]  # skip 0.1 s (the decoder needs a few frames), take 0.5 s
            crossings = sum(1 for a, b in zip(left, left[1:]) if a < 0 <= b)
            measured = crossings / (len(left) / 44100)
            ok = landed == target and abs(measured - wanted) < 0.05 * wanted
            print(f"{'PASS' if ok else 'FAIL'}: mp3 seek to {target / 44100:.0f} s plays {measured:.0f} Hz (wanted {wanted}), landed {landed}")
            failed += not ok
    return failed


def main():
    if not shutil.which("ffmpeg"):
        print("SKIP: flac (no ffmpeg)")
        return
    subprocess.run(["cargo", "+nightly-2026-09-13", "build", "--release", "--example", "decode", "--manifest-path",
                    str(CRATE / "Cargo.toml")], check=True, capture_output=True)
    decode = Path(os.environ.get("CARGO_TARGET_DIR", CRATE / "target")) / "release" / "examples" / "decode"
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
    failed += seek_cases(decode)
    failed += mp3_seek_case()
    if failed:
        raise SystemExit(f"FAIL: flac ({failed} case(s))")
    print("PASS: flac")


if __name__ == "__main__":
    main()
