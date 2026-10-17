#!/usr/bin/env python3
"""The `ltas` tape (hard-float, graphic) against QEMU's HD Audio codec. QEMU's `none` audio backend gives silence as input
and swallows the output, so this proves what QEMU can: the tape starts capture and playback, takes the screen and draws the
plot (background, grid, text, the curves at the floor), keeps analysing (frames counted), answers keys without failing,
returns the console on Escape and reports no capture overruns. Levels, the generator's signal and real input are checked
by the host tests of `crates/ltas` and `crates/noise` and on real hardware. Run with `make test-ltas`."""

import re
import struct
import subprocess
import sys
import zlib
import tempfile
import wave
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_boot import ROOT, TAG_DATA, check  # noqa: E402
from test_debug_mode import Machine  # noqa: E402
from test_gfx import screenshot  # noqa: E402
from test_recorder_fat import exfat_files  # noqa: E402

LTAS = ROOT / "build/cargo-target/x86_64-fos-tape-fp/release/ltas"
BACKGROUND, GRID_MAJOR = (0x10, 0x14, 0x18), (0x3a, 0x44, 0x4d)


def split_devices():
    """Input on the HD Audio device (QEMU: silence), output to the USB device: its WAV recording must hold the generator's tone."""
    with tempfile.TemporaryDirectory() as tmp:
        machine = Machine(tmp, {}, {"ltas": LTAS})
        wav = Path(tmp) / "usb.wav"
        machine.extra = ["-device", "qemu-xhci,id=xhci", "-audiodev", f"wav,id=usb,path={wav}", "-device", "usb-audio,audiodev=usb",
                         "-audiodev", "none,id=hda", "-device", "intel-hda", "-device", "hda-duplex,audiodev=hda"]
        machine.start()
        try:
            machine.wait_for("M3 complete")
            machine.connect()
            start = len(machine.text())
            machine.wire.send(machine.guest, TAG_DATA, b"usb rescan\n")
            machine.wait_for("fos>", since=start, timeout=20)
            start = len(machine.text())
            machine.wire.send(machine.guest, TAG_DATA, b"run ltas device=2 outdev=1 noise=sine tone=1000 level=-12\n")
            time.sleep(6)
            machine.wire.send(machine.guest, TAG_DATA, b"\x1b")
            text = machine.wait_for("frame(s) analysed", since=start, timeout=20)
            check("capture overruns 0" in text and "cannot" not in text, "split run: HD Audio in, USB out, no errors", text[-1500:])
        finally:
            machine.stop()
        check(wav.exists() and wav.stat().st_size > 44, "the USB output recorded the generator", "no wav file")
        with wave.open(str(wav)) as recording:
            count, rate = recording.getnframes(), recording.getframerate()
            data = struct.unpack(f"<{count * recording.getnchannels()}h", recording.readframes(count))[:: recording.getnchannels()]
        loud = [v for v in data if abs(v) > 500]
        crossings = sum(1 for a, b in zip(loud, loud[1:]) if a < 0 <= b)
        check(len(loud) > rate, f"it holds sound ({len(loud)} loud samples)", "")
        check(900 < crossings * rate / max(len(loud), 1) < 1100, "at about 1000 Hz", f"{crossings * rate / max(len(loud), 1):.0f}")


def forced_sse2():
    """`simd=sse2` runs the baseline FFT whatever the CPU has; an unknown value is refused."""
    with tempfile.TemporaryDirectory() as tmp:
        machine = Machine(tmp, {}, {"ltas": LTAS})
        machine.extra = ["-audiodev", "none,id=snd", "-device", "intel-hda", "-device", "hda-duplex,audiodev=snd"]
        machine.start()
        try:
            machine.wait_for("M3 complete")
            machine.connect()
            start = len(machine.text())
            machine.wire.send(machine.guest, TAG_DATA, b"run ltas simd=sse3\n")
            text = machine.wait_for("simd= takes auto, sse2 or avx2", since=start, timeout=20)
            start = len(machine.text())
            machine.wire.send(machine.guest, TAG_DATA, b"run ltas simd=sse2\n")
            time.sleep(5)
            machine.wire.send(machine.guest, TAG_DATA, b"\x1b")
            text = machine.wait_for("frame(s) analysed", since=start, timeout=20)
            check("ltas: SSE2 FFT" in text, "simd=sse2 forces the baseline FFT", text[-600:])
        finally:
            machine.stop()


def png_pixels(blob):
    """(width, height, rows) of an 8-bit RGB PNG written by crates/gfx (filters None and Up only)."""
    check(blob[:8] == b"\x89PNG\r\n\x1a\n", "the PNG has its signature", blob[:8].hex())
    width, height = struct.unpack(">II", blob[16:24])
    data, at = b"", 8
    while at < len(blob):
        length, kind = struct.unpack(">I4s", blob[at:at + 8])
        if kind == b"IDAT":
            data += blob[at + 8:at + 8 + length]
        at += 12 + length
    raw = zlib.decompress(data)
    rows, previous = [], bytes(width * 3)
    for y in range(height):
        line = raw[y * (width * 3 + 1):(y + 1) * (width * 3 + 1)]
        row = line[1:] if line[0] == 0 else bytes((a + b) & 255 for a, b in zip(line[1:], previous))
        rows.append(row)
        previous = row
    return width, height, rows


def save_files():
    """`s` writes ltas-DATE-TIME.csv and .png to the exFAT disk; the CSV has the header and rows, the PNG decodes and shows
    the plot; fsck.exfat finds the volume clean."""
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        exfat = tmp / "save.img"
        exfat.write_bytes(bytes(64 << 20))
        subprocess.run(["mkfs.exfat", str(exfat)], check=True, capture_output=True)
        machine = Machine(tmp, {}, {"ltas": LTAS})
        machine.extra = ["-audiodev", "none,id=snd", "-device", "intel-hda", "-device", "hda-duplex,audiodev=snd", "-device", "ich9-ahci,id=ahci",
                         "-drive", f"if=none,id=f2,format=raw,file={exfat}", "-device", "ide-hd,drive=f2,bus=ahci.0"]
        machine.start()
        try:
            machine.wait_for("M3 complete")
            machine.connect()
            start = len(machine.text())
            machine.wire.send(machine.guest, TAG_DATA, b"run ltas noise=pink level=-30\n")
            time.sleep(5)
            machine.wire.send(machine.guest, TAG_DATA, b"s")
            time.sleep(3)
            machine.wire.send(machine.guest, TAG_DATA, b"\x1b")
            machine.wait_for("frame(s) analysed", since=start, timeout=20)
        finally:
            machine.stop()
        files = exfat_files(exfat)
        names = sorted(name for name in files if name.startswith("ltas-"))
        check(len(names) == 2 and names[0][:-4] == names[1][:-4] and names[0].endswith(".csv") and names[1].endswith(".png"), f"a CSV and a PNG with the same stamped name ({names})", str(list(files)))
        csv = files[names[0]].decode().splitlines()
        check(csv[0].startswith("# ltas ") and csv[3].startswith("centre_hz,") and len(csv) == 4 + 31, f"the CSV has the header and 31 band rows ({len(csv)} lines)", "\n".join(csv[:6]))
        width, height, rows = png_pixels(files[names[1]])
        check((width, height) == (1280, 800) and tuple(rows[height - 5][15:18]) == BACKGROUND, f"the PNG decodes to the {width}x{height} screen", "")
        result = subprocess.run(["fsck.exfat", "-n", str(exfat)], capture_output=True, text=True)
        check(result.returncode == 0, "fsck.exfat finds the volume clean", result.stdout[-500:] + result.stderr[-500:])


def main():
    with tempfile.TemporaryDirectory() as tmp:
        machine = Machine(tmp, {}, {"ltas": LTAS})
        machine.monitor = Path(tmp) / "monitor"
        machine.extra = ["-audiodev", "none,id=snd", "-device", "intel-hda", "-device", "hda-duplex,audiodev=snd"]
        machine.start()
        try:
            machine.wait_for("M3 complete")
            machine.connect()
            start = len(machine.text())
            machine.wire.send(machine.guest, TAG_DATA, b"run ltas noise=pink level=-30 res=1/6\n")
            time.sleep(5)
            width, height, pixel = screenshot(machine.monitor, "ltas.ppm")
            check(width >= 640, f"QEMU produced a screendump ({width}x{height})", machine.text()[start:][-1500:])
            check(pixel(5, height - 5) == BACKGROUND, "the tape took the screen (background colour)", str(pixel(5, height - 5)))
            check(pixel(56, 60) == GRID_MAJOR, "the plot frame is drawn", str(pixel(56, 60)))
            lit = sum(1 for y in range(4, 36) for x in range(56, 600) if pixel(x, y) != BACKGROUND)
            check(lit > 300, f"the status line is drawn ({lit} lit pixels)", "")
            for key in (b"v", b"w", b"a", b"n", b"t", b"g", b"+", b"f", b"b", b"3", b"3", b".", b",", b"m", b"m", b"r", b" ", b" "):
                machine.wire.send(machine.guest, TAG_DATA, key)
                time.sleep(0.4)
            _, _, after = screenshot(machine.monitor, "ltas2.ppm")
            check(after(5, height - 5) == BACKGROUND and after(56, 60) == GRID_MAJOR, "the keys left the display intact", "")
            machine.wire.send(machine.guest, TAG_DATA, b"\x1b")
            text = machine.wait_for("frame(s) analysed", since=start, timeout=20)
            frames = int(text.split("ltas: ")[-1].split(" frame")[0])
            check(frames >= 3, f"the analyser ran ({frames} frames)", text[-1500:])
            check("capture overruns 0" in text, "no capture overruns", text[-500:])
            check(re.search(r"ltas: (AVX2\+FMA|SSE2) FFT \d+: \d+ us per analysis", text) is not None, "the exit line names the FFT code path and its timing", text[-600:])
            time.sleep(0.5)
            _, _, last = screenshot(machine.monitor, "ltas3.ppm")
            check(last(56, 60) != GRID_MAJOR, "the plot is gone when the tape ends", "")
        finally:
            machine.stop()
    forced_sse2()
    split_devices()
    save_files()
    print("ltas: all checks passed")


if __name__ == "__main__":
    main()
