#!/usr/bin/env python3
"""`record` streaming to disk (v2).

QEMU's USB audio has no capture side, so the tape records a test tone (`tone=HZ`, paced like a device) through its real
disk path. A host-side parse of the image then checks the WAV files: a timed take has the exact length and a valid
header, a take ended with Escape is shorter and consistent, and a take cut off by killing QEMU (a power cut) keeps
what was recorded up to the last two-second checkpoint, with a header that matches the file length. Run with
`make test-v2-record`."""

import struct
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_audio_net import Guest, tone_frequency  # noqa: E402
from test_boot import check  # noqa: E402

SECTOR = 512


def fosfs_files(image: Path):
    """{name: bytes} of the first FOSFS volume found through the GPT of a disk image."""
    data = image.read_bytes()
    entries_lba = struct.unpack_from("<Q", data, SECTOR + 72)[0]
    count, size = struct.unpack_from("<II", data, SECTOR + 80)
    for index in range(count):
        entry = data[entries_lba * SECTOR + index * size:entries_lba * SECTOR + (index + 1) * size]
        first, last = struct.unpack_from("<QQ", entry, 32)
        if entry[:16] != bytes(16) and data[first * SECTOR:first * SECTOR + 8] == b"FOSFS3\0\0":
            files = {}
            for sector in range(1, 9):
                directory = data[(first + sector) * SECTOR:(first + sector + 1) * SECTOR]
                for slot in range(8):
                    raw = directory[slot * 64:(slot + 1) * 64]
                    if raw[0]:
                        start, length = struct.unpack_from("<QQ", raw, 4)
                        files[raw[28:28 + raw[1]].decode()] = data[(first + start) * SECTOR:(first + start) * SECTOR + length]
            return files
    raise SystemExit("FAIL: no FOSFS volume found in the image")


def wav_info(blob: bytes):
    """(channels, rate, bits, data bytes declared, samples of channel 0) of a canonical 44-byte-header WAV."""
    check(blob[:4] == b"RIFF" and blob[8:16] == b"WAVEfmt ", "a RIFF/WAVE header", blob[:16].hex())
    channels, rate = struct.unpack_from("<HI", blob, 22)
    bits = struct.unpack_from("<H", blob, 34)[0]
    declared = struct.unpack_from("<I", blob, 40)[0]
    check(struct.unpack_from("<I", blob, 4)[0] == 36 + declared, "RIFF size = 36 + data size", "")
    body = blob[44:44 + declared]
    samples = struct.unpack(f"<{len(body) // 2}h", body[:len(body) // 2 * 2])[::channels]
    return channels, rate, bits, declared, samples


def main():
    with tempfile.TemporaryDirectory() as tmp:
        guest = Guest(tmp, "record", "52:54:00:12:34:e1")
        image = Path(tmp) / "record.img"
        try:
            guest.wait_for("M3 complete")
            guest.wait_for("fos> ")
            mark = guest.send("run record t1 5 48000 tone=440")
            guest.wait_for("Wrote t1.wav", 60, mark)
            text = guest.text()[mark:]
            check("Recorded 240000 frames (5 s)" in text, "a timed take of 5 s holds 240000 frames", text[-500:])
            mark = guest.send("run record t2 0 48000 tone=880")
            guest.wait_for("Recording 2 channel(s)", 30, mark)
            guest.pump(4.0)
            guest.process.stdin.write(b"\x1b")
            guest.process.stdin.flush()
            guest.wait_for("Wrote t2.wav", 30, mark)
            text = guest.text()[mark:]
            check("stopped early" in text, "Escape ends an open-ended take", text[-500:])
            mark = guest.send("run record t3 0 48000 tone=660")
            guest.wait_for("Recording 2 channel(s)", 30, mark)
            guest.pump(7.0)
        finally:
            guest.process.kill()  # a power cut, in the middle of the third take
            guest.process.wait()
        files = fosfs_files(image)
        channels, rate, bits, declared, samples = wav_info(files["t1.wav"])
        check((channels, rate, bits, declared) == (2, 48000, 16, 240000 * 4), f"t1.wav: 2 channels, 48 kHz, 16 bit, {declared} data bytes", "")
        check(len(files["t1.wav"]) == 44 + declared, "and the file is exactly header + data", "")
        measured = tone_frequency(samples, rate, samples[rate:2 * rate])
        check(abs(measured - 440) < 5, f"the recorded tone is {measured:.1f} Hz (wanted 440)", "")
        channels, rate, bits, declared, samples = wav_info(files["t2.wav"])
        check(3.5 * rate * 4 < declared < 5.5 * rate * 4, f"t2.wav (Escape after ~4 s) holds {declared / 4 / rate:.1f} s", "")
        check(len(files["t2.wav"]) == 44 + declared, "with a header that matches its length", "")
        measured = tone_frequency(samples, rate, samples[rate:2 * rate])
        check(abs(measured - 880) < 8, f"and {measured:.1f} Hz inside (wanted 880)", "")
        channels, rate, bits, declared, samples = wav_info(files["t3.wav"])
        check(2.5 * rate * 4 < declared <= 7.5 * rate * 4, f"t3.wav (power cut after ~7 s) kept {declared / 4 / rate:.1f} s", "")
        check(len(files["t3.wav"]) == 44 + declared, "with a header that matches its length (checkpointed)", "")
        measured = tone_frequency(samples, rate, samples[rate:2 * rate])
        check(abs(measured - 660) < 8, f"and {measured:.1f} Hz inside (wanted 660)", "")
    print("v2 record: all checks passed")


if __name__ == "__main__":
    main()
