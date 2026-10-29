#!/usr/bin/env python3
"""The `frt60` tape (room acoustics: RT60, EDT, clarity, waterfall) through the loopback audio device. Checks:
 - balloon method with `simroom=1.0`: Space arms the trigger and plays a synthetic pop (a click, then white noise falling 60 dB
   in 1.0 s) through the output; the loopback carries it to the input, the trigger fires, and the analysis must find a
   T20 of about 1.0 s in the overall figure and in the octave bands from 250 Hz to 2 kHz; `s` writes frt60 CSV, WAV (the
   impulse response, 32-bit float) and PNG to the exFAT disk (fsck.exfat is clean); the four views all draw something
 - third-octave bands (`b`) re-analyse the same recording and the saved CSV has 24 band rows
 - sweep method through the loopback (a perfect "room": the impulse response is one click): the deconvolved response saved as
   WAV is a single sharp peak with a quiet tail
 - without an exFAT disk the files land on the FOSFS disk
 - bad arguments are refused.
QEMU cannot send touch events, so the keys stand in for the fingers (a key and a button share one `Action`).
Run with `make test-frt60`."""

import struct
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_boot import ROOT, TAG_DATA, check  # noqa: E402
from test_debug_mode import Machine  # noqa: E402
from test_gfx import screenshot  # noqa: E402
from test_recorder_fat import exfat_files  # noqa: E402

FRT60 = ROOT / "build/cargo-target/x86_64-fos-tape-fp/release/frt60"
AUDIO = ["-audiodev", "none,id=snd", "-device", "intel-hda", "-device", "hda-duplex,audiodev=snd"]


def send(machine, data, wait=0.6):
    machine.wire.send(machine.guest, TAG_DATA, data)
    time.sleep(wait)


def exfat_image(tmp):
    image = Path(tmp) / "save.img"
    image.write_bytes(bytes(64 << 20))
    subprocess.run(["mkfs.exfat", str(image)], check=True, capture_output=True)
    return image


def session(tmp, arguments, keys, exfat=None, settle=4, shots=()):
    """Boots, runs `frt60 ARGUMENTS`, sends (key, wait) pairs, quits; returns (machine text, [screenshots])."""
    machine = Machine(tmp, {}, {"frt60": FRT60})
    machine.extra = list(AUDIO)
    if exfat:
        machine.extra += ["-device", "ich9-ahci,id=ahci", "-drive", f"if=none,id=f2,format=raw,file={exfat}", "-device", "ide-hd,drive=f2,bus=ahci.0"]
    machine.monitor = Path(tmp) / "monitor"
    machine.start()
    taken = []
    try:
        machine.wait_for("M3 complete")
        machine.connect()
        start = len(machine.text())
        send(machine, f"run frt60 {arguments}\n".encode(), settle)
        for index, (key, wait) in enumerate(keys):
            send(machine, key, wait)
            if index in shots:
                taken.append(screenshot(machine.monitor, f"frt60-{index}.ppm"))
        send(machine, b"\x1b", 2)
        machine.pump(1.0)
        text = machine.text()[start:]
    finally:
        machine.stop()
    return text, taken


def csv_rows(text):
    """{label: [edt, t20, t30, c50, c80, d50, dynamic]} of an frt60 CSV (None for a blank cell)."""
    rows = {}
    for line in text.splitlines():
        if not line or line[0] == "#" or line.startswith("hz,"):
            continue
        cells = line.split(",")
        rows[cells[0]] = [float(c) if c else None for c in cells[1:]]
    return rows


def wav_samples(data):
    check(data[:4] == b"RIFF" and data[8:12] == b"WAVE", "the WAV has a RIFF/WAVE header", repr(data[:16]))
    format_tag, channels, rate, _, _, bits = struct.unpack("<HHIIHH", data[20:36])
    check(format_tag == 3 and channels == 1 and bits == 32, f"mono 32-bit float WAV (tag {format_tag}, {channels} ch, {bits} bit)", "")
    count = (len(data) - 44) // 4
    return rate, struct.unpack(f"<{count}f", data[44:44 + 4 * count])


def lit(shot):
    """How many of a screenshot's plot-area samples differ from its background."""
    width, height, pixel = shot
    background = pixel(2, height // 2)
    return sum(1 for y in range(70, height - 120, 5) for x in range(60, width - 20, 5) if pixel(x, y) != background)


def balloon():
    with tempfile.TemporaryDirectory() as tmp:
        image = exfat_image(tmp)
        keys = [(b" ", 25), (b"v", 1.5), (b"v", 1.5), (b"v", 1.5), (b"v", 1), (b"s", 4), (b"b", 25), (b"s", 4)]
        text, shots = session(tmp, "device=loop0 outdev=loop0 simroom=1.0 len=3", keys, exfat=image, shots=(0, 1, 2, 3, 4), settle=5)
        files = exfat_files(image)
        csvs, wavs, pngs = (sorted(k for k in files if k.endswith(ext)) for ext in (".csv", ".wav", ".png"))
        check(len(csvs) == 2 and len(wavs) == 2 and len(pngs) == 2, f"each s writes a stamped CSV, WAV and PNG ({list(files)})", text[-1500:])
        rows = csv_rows(files[csvs[0]].decode())
        check("all" in rows, "the CSV has the overall row", str(rows)[:300])
        t20 = rows["all"][1]
        check(t20 is not None and abs(t20 / 1.0 - 1.0) < 0.15, f"overall T20 {t20} s is within 15 % of the simulated 1.0 s", str(rows["all"]))
        mid = {hz: r for hz, r in rows.items() if hz != "all" and 250 <= float(hz) <= 2100}
        check(len(mid) >= 4, f"the octave bands 250 Hz to 2 kHz are there ({sorted(mid)})", str(rows)[:500])
        for hz, r in mid.items():
            check(r[1] is not None and abs(r[1] - 1.0) < 0.2, f"T20 of the {hz} Hz band is {r[1]} (1.0 expected)", str(r))
        rate, ir = wav_samples(files[wavs[0]])
        check(rate == 48000 and len(ir) > 48000, f"the WAV holds {len(ir)} samples at {rate} Hz", "")
        check(max(abs(x) for x in ir) > 0.1, "the impulse response is not silent", "")
        check(files[pngs[0]][:8] == b"\x89PNG\r\n\x1a\n", "the PNG has its signature", repr(files[pngs[0]][:8]))
        for index, shot in enumerate(shots):
            name = ["summary", "waterfall", "heat map", "decay", "summary again"][index]
            check(lit(shot) > 150, f"the {name} view draws something ({lit(shot)} lit samples)", "")
        check("2 file set(s) saved" in text, "both saves are reported", text[-800:])
        result = subprocess.run(["fsck.exfat", "-n", str(image)], capture_output=True, text=True)
        check(result.returncode == 0, "fsck.exfat finds the volume clean", result.stdout[-500:] + result.stderr[-500:])
        third = [hz for hz in csv_rows(files[csvs[1]].decode()) if hz != "all"]
        check(len(third) == 24, f"after b the second CSV has 24 third-octave rows ({len(third)})", str(sorted(third)))


def help_screen():
    """h opens the help in the plot area, h again turns the page, any other button closes it."""
    with tempfile.TemporaryDirectory() as tmp:
        keys = [(b" ", 25), (b"h", 1.5), (b"h", 1.5), (b"v", 1.5)]
        text, shots = session(tmp, "device=loop0 outdev=loop0 simroom=1.0 len=3", keys, shots=(0, 1, 2, 3), settle=5)
        for index, name in enumerate(["the plot", "help page 1", "help page 2", "the plot again"]):
            check(lit(shots[index]) > 150, f"{name} draws something ({lit(shots[index])} lit samples)", "")
        width, height, pixel = shots[1]
        same = sum(1 for y in range(70, height - 120, 5) for x in range(60, width - 20, 5) if pixel(x, y) == shots[2][2](x, y))
        total = len(range(70, height - 120, 5)) * len(range(60, width - 20, 5))
        check(same < total * 0.95, f"page 2 differs from page 1 ({same} of {total} samples equal)", "")
        same = sum(1 for y in range(70, height - 120, 5) for x in range(60, width - 20, 5) if pixel(x, y) == shots[0][2](x, y))
        check(same < total * 0.95, f"help page 1 differs from the plot ({same} of {total} samples equal)", "")
        shown = [(x, y) for y in range(70, height - 120, 5) for x in range(60, width - 20, 5) if shots[3][2](x, y) != shots[1][2](x, y)]
        check(len(shown) > 100, f"v closes the help ({len(shown)} samples changed)", "")


def small_text():
    """text=1 gives the small font and short buttons: the plot is taller than at the large size."""
    with tempfile.TemporaryDirectory() as tmp:
        keys = [(b" ", 25), (b"h", 1.5)]
        _, shots = session(tmp, "device=loop0 outdev=loop0 simroom=1.0 len=3 text=1", keys, shots=(0, 1), settle=5)
        for index, name in enumerate(["the plot", "help"]):
            check(lit(shots[index]) > 150, f"text=1: {name} draws something ({lit(shots[index])} lit samples)", "")


def sweep_is_one_click():
    with tempfile.TemporaryDirectory() as tmp:
        image = exfat_image(tmp)
        text, _ = session(tmp, "device=loop0 outdev=loop0 method=sweep level=-12 name=sw", [(b" ", 150), (b"s", 5)], exfat=image, settle=60)
        files = exfat_files(image)
        check("sw.wav" in files, f"the sweep measurement saved sw.wav ({list(files)})", text[-1500:])
        rate, ir = wav_samples(files["sw.wav"])
        peak = max(abs(x) for x in ir)
        check(0.3 < peak < 3.0, f"a loopback's response is a click of about unity gain (peak {peak:.3f})", "")
        tail = ir[int(0.02 * rate):]
        rms = (sum(x * x for x in tail) / len(tail)) ** 0.5
        check(rms < 0.02 * peak, f"the tail after the click is quiet (rms {rms:.5f} against peak {peak:.3f})", "")


def fosfs_fallback():
    with tempfile.TemporaryDirectory() as tmp:
        text, _ = session(tmp, "device=loop0 outdev=loop0 simroom=0.8 len=2 name=f1", [(b" ", 20), (b"s", 4)], settle=5)
        check("1 file set(s) saved" in text, "without an exFAT disk the save still succeeds (FOSFS)", text[-1500:])


def refusals():
    with tempfile.TemporaryDirectory() as tmp:
        machine = Machine(tmp, {}, {"frt60": FRT60})
        machine.extra = list(AUDIO)
        machine.start()
        try:
            machine.wait_for("M3 complete")
            machine.connect()
            for arguments in ("method=pop", "bands=half", "view=3d", "simroom=99", "text=3"):
                start = len(machine.text())
                send(machine, f"run frt60 device=loop0 outdev=loop0 {arguments}\n".encode(), 3)
                machine.pump(1.0)
                text = machine.text()[start:]
                check("fos> " in text and "frt60" in text.lower(), f"{arguments} is refused with a message", text[-500:])
        finally:
            machine.stop()


def main():
    balloon()
    help_screen()
    small_text()
    sweep_is_one_click()
    fosfs_fallback()
    refusals()
    print("frt60: all checks passed")


if __name__ == "__main__":
    main()
