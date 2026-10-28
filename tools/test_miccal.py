#!/usr/bin/env python3
"""The `miccal` tape (microphone calibration) through the loopback audio device, so the signal it plays is the signal it
measures: the calibration of a perfectly flat "microphone" must come out flat. Checks:
 - reference method with pink noise: `s` writes testmic.cal and testmic-rew.txt to the exFAT disk (relative, 0 dB at 1 kHz,
   flat); the screen shows the touch buttons; fsck.exfat finds the disk clean
 - coupler method with no exFAT disk: the files land on the FOSFS disk
 - stepped sines: the sweep completes and the saved coupler correction is flat
 - bad arguments are refused.
QEMU cannot send touch events, so the buttons are checked as a picture and the keys stand in for the fingers (a key and a
button share one `Action`). Run with `make test-miccal`."""

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

MICCAL = ROOT / "build/cargo-target/x86_64-fos-tape-fp/release/miccal"
AUDIO = ["-audiodev", "none,id=snd", "-device", "intel-hda", "-device", "hda-duplex,audiodev=snd"]


def send(machine, data, wait=0.6):
    machine.wire.send(machine.guest, TAG_DATA, data)
    time.sleep(wait)


def points(text):
    """[(hz, db)] of the data lines of a calibration file."""
    rows = []
    for line in text.splitlines():
        fields = line.split()
        if line and line[0] not in "#*" and len(fields) == 2:
            rows.append((float(fields[0]), float(fields[1])))
    return rows


def session(tmp, arguments, keys, exfat=None, settle=7, screen=False):
    """Boots, runs `miccal ARGUMENTS`, sends (key, wait) pairs, quits; returns (machine text, screenshot)."""
    machine = Machine(tmp, {}, {"miccal": MICCAL})
    machine.extra = list(AUDIO)
    if exfat:
        machine.extra += ["-device", "ich9-ahci,id=ahci", "-drive", f"if=none,id=f2,format=raw,file={exfat}", "-device", "ide-hd,drive=f2,bus=ahci.0"]
    if screen:
        machine.monitor = Path(tmp) / "monitor"
    machine.start()
    shot = None
    try:
        machine.wait_for("M3 complete")
        machine.connect()
        start = len(machine.text())
        send(machine, f"run miccal {arguments}\n".encode(), settle)
        for key, wait in keys:
            send(machine, key, wait)
        if screen:
            shot = screenshot(machine.monitor, "miccal.ppm")
        send(machine, b"\x1b", 2)
        machine.pump(1.0)
        text = machine.text()[start:]
    finally:
        machine.stop()
    return text, shot


def exfat_image(tmp):
    image = Path(tmp) / "save.img"
    image.write_bytes(bytes(64 << 20))
    subprocess.run(["mkfs.exfat", str(image)], check=True, capture_output=True)
    return image


def reference_to_exfat():
    with tempfile.TemporaryDirectory() as tmp:
        image = exfat_image(tmp)
        text, shot = session(tmp, "device=loop0 outdev=loop0 method=reference source=pink level=-20 name=testmic",
                             [(b"2", 0.5), (b"1", 0.5), (b"s", 2)], exfat=image, screen=True)
        files = exfat_files(image)
        check("testmic.cal" in files and "testmic-rew.txt" in files, f"s writes testmic.cal and testmic-rew.txt to the exFAT disk ({list(files)})", text[-1500:])
        cal = files.get("testmic.cal", b"").decode()
        rows = points(cal)
        check("# absolute no" in cal and len(rows) >= 25, f"the fos file is a relative calibration with {len(rows)} bands", cal[:600])
        at_1k = [db for hz, db in rows if 890 < hz < 1120]
        check(at_1k and abs(at_1k[0]) < 0.01, "0 dB at 1 kHz", str(at_1k))
        off = [(hz, db) for hz, db in rows if hz >= 100 and abs(db) > 3.0]
        check(not off, "a loopback is flat: every band from 100 Hz within 3 dB", str(off))
        rew = files.get("testmic-rew.txt", b"").decode()
        check(len(points(rew)) == len(rows), "the REW-style file has the same bands", rew[:300])
        width, height, pixel = shot
        background = pixel(5, height - 140)
        lit = sum(1 for x in range(0, width, 4) if pixel(x, height - 30) != background)
        check(lit > width // 16, f"the touch buttons are drawn along the bottom ({lit} lit samples)", "")
        result = subprocess.run(["fsck.exfat", "-n", str(image)], capture_output=True, text=True)
        check(result.returncode == 0, "fsck.exfat finds the volume clean", result.stdout[-500:] + result.stderr[-500:])


def coupler_to_fosfs():
    with tempfile.TemporaryDirectory() as tmp:
        text, _ = session(tmp, "device=loop0 outdev=loop0 method=coupler source=pink spl=94 name=m2", [(b"1", 0.5), (b"s", 2)])
        check("1 file set(s) saved" in text, "without an exFAT disk the save still succeeds (FOSFS)", text[-1500:])


def steps_are_flat():
    with tempfile.TemporaryDirectory() as tmp:
        image = exfat_image(tmp)
        text, _ = session(tmp, "device=loop0 outdev=loop0 method=coupler source=steps dwell=0.3 level=-20 name=sw", [(b"1", 0.5), (b"s", 2)],
                          exfat=image, settle=45)
        files = exfat_files(image)
        rows = points(files.get("sw.cal", b"").decode())
        check(len(rows) >= 25, f"the stepped-sine sweep measured {len(rows)} bands", text[-1500:])
        spread = max(db for _, db in rows) - min(db for _, db in rows) if rows else 99
        check(spread < 4.0, f"the sweep through the loopback is flat (spread {spread:.2f} dB)", str(rows))


def refusals():
    with tempfile.TemporaryDirectory() as tmp:
        machine = Machine(tmp, {}, {"miccal": MICCAL})
        machine.extra = list(AUDIO)
        machine.start()
        try:
            machine.wait_for("M3 complete")
            machine.connect()
            for arguments in ("method=sine", "source=white", "ref=nothere"):
                start = len(machine.text())
                send(machine, f"run miccal device=loop0 {arguments}\n".encode(), 3)
                machine.pump(1.0)
                text = machine.text()[start:]
                check("fos> " in text and "miccal" in text.lower(), f"{arguments} is refused with a message", text[-500:])
        finally:
            machine.stop()


def main():
    reference_to_exfat()
    coupler_to_fosfs()
    steps_are_flat()
    refusals()
    print("miccal: all checks passed")


if __name__ == "__main__":
    main()
