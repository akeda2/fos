#!/usr/bin/env python3
"""The kakelfix tape's Mixer page on the loopback device's software mixer (`make test-kakelfix`).
Embedded kernel (writes made inline):
 - `kakelfix device=loop0 show` lists the identity mix (Out L <- Play L, Out R <- Play R at 0.0 dB) and the Dim switch
 - in the tape, `x` switches the selected strip off; a mouse click on the first fader sets it where the click is
   (half way up the travel is about -9 dB); a click on the Quit soft key ends the tape; `show` then reads the new gains
 - the screen shows the mixer (header text drawn, not a blank frame)
 - `3` opens the Analyzer, which captures from loop0 while a `play` service plays a 1 kHz tone into it: the left
   channel's envelope (the first curve colour) grows in from the right edge of the upper lane; Quit there ends the tape
 - `4` opens the Meters page on the same capture: input 1's bar (x 81..105, Nordic PPM) lights for the tone; `3` goes back
 - `2` opens the Recorder; `R` starts a take of loop0's two inputs to the boot disk (the only disk) as FLAC 24, the
   default: disk1/take001.flac. On the Analyzer page (`3`) the header's REC lamp stays lit, the take goes on; `2` and `R`
   stop it. The file in the image afterwards: STREAMINFO says 2 channels, 48 kHz, 24 bits and seconds of samples; the
   reference `flac` (when the host has it) tests it, MD5 included, and its decode holds the tone. (Not the take's
   wall-clock length: with a listener loop0 has no clock and `play` runs as fast as the capture drains,
   crates/kernel-core/src/loopback.rs.)
 - Up (the 30 s span) right before Quit: the Analyzer's settings are in kakelfix.cfg afterwards (saved on quit), the
   Recorder's too
 - kakelfix again with `rec-format=wav rec-ch=2` on the command line: a take of input 2 alone is disk1/take002.wav (the
   number is not reused across formats), one channel of loop0's 32-bit float at 48 kHz, holding the tone
Server kernel (writes go to the worker thread): `x`, then at once a lone Escape, which asks the tape to stop (`read_key`
answers KEY_ABORT, `shell::request_stop`) rather than killing it, so it ends through the writer's join with `x` written; then `show`, at -smp 4.
QEMU's screen is 1280x800, so kakelfix's layout is at scale 1 (crates/kakelfix/src/layout.rs): soft-key slot 10 (Quit, on
every page) is at x 1184..1272, y 726..780 (after two half-key gaps between the groups); strip 0's fader travel is x
12..57, y 152..744. The Analyzer's Both view puts the waveform plot at x 56..1151, y 56..325, the left lane's centre at y 123."""

import os
import re
import select
import shutil
import struct
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_boot import TAG_DATA, check, monitor  # noqa: E402
from term_net import TERM_TAG_SYSRQ as TAG_SYSRQ  # noqa: E402
from test_debug_mode import Machine  # noqa: E402
from test_gfx import screenshot  # noqa: E402
from test_record import fosfs_files  # noqa: E402
from test_usb_audio import write_wav  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent
TAPE = ROOT / "build/fosfor/tapes/kakelfix"
PLAY = ROOT / "build/fosfor/tapes/play"
MF_IMAGE = Path(os.environ.get("FOSFOR_IMAGE_MF_KAKELFIX", ROOT / "build/fosfor-mf/fosfor-kakelfix.img"))
CODE = next(p for p in ["/usr/share/OVMF/OVMF_CODE_4M.fd", "/usr/share/OVMF/OVMF_CODE.fd",
                        "/usr/share/edk2/ovmf/OVMF_CODE.fd"] if Path(p).exists())
QUIT = (1228, 752)
METER_BAR = (range(84, 103, 3), range(300, 690, 2))
LIT = {(0x3c, 0xcf, 0x6a), (0xff, 0xc8, 0x3d), (0xff, 0x4d, 0x4d)}
ENVELOPE = (range(1000, 1150, 3), range(140, 175))
BLUE = (0x4a, 0xa3, 0xff)
FADER_MIDDLE = (32, 448)
# The header's REC lamp and clock, left of the soft keys (x 1176): "REC 00:05" ends 8 pixels before them.
REC_LAMP = (range(1070, 1176), range(4, 22))
RED = (0xff, 0x4d, 0x4d)


def send(machine, data, wait=0.6):
    machine.wire.send(machine.guest, TAG_DATA, data)
    machine.pump(wait)


def sends(text, output):
    """{source: level} of one output's line in `kakelfix show`."""
    m = re.search(rf"^\s*{output}:(.*)$", text, re.M)
    check(m, f"`show` has a line for {output}", text[-800:])
    return dict(re.findall(r"(Play [LR]) ([+-]?\d+\.\d|off|\?)", m.group(1)))


def show(machine):
    start = len(machine.text())
    send(machine, b"run kakelfix device=loop0 show\n", 2)
    return machine.wait_for("Dim:", since=start, timeout=15)


def steps(distance):
    """Counts per packet that move the cursor exactly `distance` (even) pixels: the kernel's pointer gain makes a packet of d
    counts travel 4d + d|d|/2 pixels (`usb_pointer::mouse_input`), so 10 counts go 90, 2 go 10 and 1 goes 4."""
    out = []
    while distance >= 98:
        out.append(10)
        distance -= 90
    while distance % 4:
        out.append(2)
        distance -= 10
    return out + [1] * (distance // 4)


def pointer_to(machine, x, y):
    """Parks the cursor in the top left corner (it is clamped), then walks it to (x, y)."""
    for _ in range(4):
        monitor(str(machine.monitor), "mouse_move -100 -100")
    for count in steps(x):
        monitor(str(machine.monitor), f"mouse_move {count} 0")
    for count in steps(y):
        monitor(str(machine.monitor), f"mouse_move 0 {count}")
    machine.pump(0.3)


def where(machine):
    start = len(machine.text())
    send(machine, b"touch\n", 1)
    m = re.search(r"at (\d+),(\d+),", machine.text()[start:])
    return m and (int(m.group(1)), int(m.group(2)))


def lamp(machine, name):
    """Red pixels of the header's REC lamp."""
    (machine.monitor.parent / name).unlink(missing_ok=True)
    _, _, pixel = screenshot(machine.monitor, name)
    return sum(pixel(x, y) == RED for x in REC_LAMP[0] for y in REC_LAMP[1])


def toggle_take(machine, want_on):
    """`R` until the lamp says the take is on (or off): a term net keystroke is occasionally lost, and `R` toggles."""
    deadline = time.monotonic() + 12
    while True:
        send(machine, b"R", 1.5)
        lit = lamp(machine, "rec.ppm")
        if (lit >= 20) == want_on or time.monotonic() > deadline:
            return lit


def take_wav(blob):
    """(channels, rate, block align, data bytes, any non-zero sample) of a WAV written by the recorder."""
    check(blob[:4] in (b"RIFF", b"RF64") and blob[8:12] == b"WAVE", "the take has a RIFF/WAVE header", blob[:16].hex())
    at, fmt = 12, None
    while at + 8 <= len(blob):
        tag, size = blob[at:at + 4], struct.unpack_from("<I", blob, at + 4)[0]
        if tag == b"fmt ":
            fmt = struct.unpack_from("<HHIIH", blob, at + 8)
        if tag == b"data":
            body = blob[at + 8:at + 8 + size]
            check(len(blob) == at + 8 + size, "the WAV is exactly header + data", f"{len(blob)} vs {at + 8 + size}")
            return fmt[1], fmt[2], fmt[4], size, any(body)
        at += 8 + size
    check(False, "the WAV has fmt and data chunks", blob[:96].hex())


def take_flac(blob, folder, name):
    """(channels, rate, bits, total samples) from a FLAC's STREAMINFO; with the reference `flac` on the host, also its
    integrity test (frame CRCs and the MD5 of the samples) and whether the decoded samples hold anything but zeros."""
    check(blob[:4] == b"fLaC" and blob[4] & 0x7F == 0, f"{name} starts with fLaC and STREAMINFO", blob[:8].hex())
    packed = int.from_bytes(blob[18:26], "big")
    rate, channels, bits, total = packed >> 44, (packed >> 41 & 7) + 1, (packed >> 36 & 31) + 1, packed & (1 << 36) - 1
    if not shutil.which("flac"):
        print(f"SKIP: no `flac` on this host: {name} not decoded")
        return channels, rate, bits, total
    path = Path(folder) / name
    path.write_bytes(blob)
    tested = subprocess.run(["flac", "-t", "-s", str(path)], capture_output=True, text=True)
    check(tested.returncode == 0, f"flac -t passes {name} (frame CRCs, MD5)", tested.stderr[-400:])
    raw = path.with_suffix(".raw")
    subprocess.run(["flac", "-d", "-s", "-f", "--force-raw-format", "--endian=little", "--sign=signed", "-o", str(raw), str(path)], check=True)
    samples = raw.read_bytes()
    check(len(samples) == total * channels * bits // 8, f"{name} decodes to its STREAMINFO length ({len(samples)} bytes)")
    check(any(samples), f"{name} holds the tone (not all zero)")
    return channels, rate, bits, total


def embedded():
    with tempfile.TemporaryDirectory() as tmp:
        tone = Path(tmp) / "tone.wav"
        # Long enough to outlast the Recorder's take at the end: loop0 delivers only what is played.
        write_wav(tone, 48000, 2, 2, 1000, 60)
        machine = Machine(tmp, {}, {"kakelfix": TAPE, "play": PLAY, "tone.wav": tone})
        machine.monitor = Path(tmp) / "monitor"
        machine.start()
        machine.wait_for("M3 complete")
        machine.connect()
        try:
            text = show(machine)
            check("loop0" in text or "Loopback" in text or "loop" in text.lower(), "`show` names the loopback mixer", text[-600:])
            check(sends(text, "Out L") == {"Play L": "+0.0"}, "identity mix: Out L <- Play L at 0.0 dB", text[-600:])
            check(sends(text, "Out R") == {"Play R": "+0.0"}, "identity mix: Out R <- Play R at 0.0 dB", text[-600:])
            check(re.search(r"Dim: Off", text), "the Dim switch is listed, off", text[-600:])

            # Cursor placement is checked in the shell first: a click must land where the layout says.
            pointer_to(machine, *FADER_MIDDLE)
            at = where(machine)
            check(at == FADER_MIDDLE, f"the cursor is at the first fader's middle ({at})", machine.text()[-400:])

            # A service plays a tone into loop0 (paced at 48 kHz until a listener starts) for the Analyzer to capture.
            send(machine, b"run play device=loop0 tone.wav &0\n", 2)
            start = len(machine.text())
            send(machine, b"run kakelfix device=loop0\n", 4)
            width, height, pixel = screenshot(machine.monitor, "kakelfix.ppm")
            check((width, height) == (1280, 800), f"the screen is 1280x800 ({width}x{height})")
            background = pixel(width // 2, height - 2)
            inked = sum(pixel(x, y) != background for x in range(8, 400, 2) for y in range(4, 20))
            check(inked > 50, f"the header is drawn ({inked} pixels)")
            # Strip 0 is the Play L/R pair into Out L/R: off with `x`, then a click half way up its fader.
            send(machine, b"x", 1)
            monitor(str(machine.monitor), "mouse_button 1")
            machine.pump(0.3)
            monitor(str(machine.monitor), "mouse_button 0")
            machine.pump(0.5)
            send(machine, b"3", 1)
            # Bounded wait for the envelope. A term net keystroke is occasionally lost, so `3` (idempotent) is sent again.
            deadline = time.monotonic() + 12
            while True:
                (machine.monitor.parent / "analyzer.ppm").unlink(missing_ok=True)
                width, height, pixel = screenshot(machine.monitor, "analyzer.ppm")
                traced = sum(pixel(x, y) == BLUE for x in ENVELOPE[0] for y in ENVELOPE[1])
                if traced >= 40 or time.monotonic() > deadline:
                    break
                send(machine, b"3", 1)
            check(traced >= 40, f"the Analyzer captures and draws the envelope ({traced} columns)")
            # SysRq while the tape and the play service run: the dump names both and neither stops (Quit below still works).
            mark = len(machine.text())
            machine.wire.send(machine.guest, TAG_SYSRQ)
            dump = machine.wait_for("--- end sysrq ---", since=mark, timeout=10)
            check("foreground tape: kakelfix" in dump, "SysRq names the foreground tape", dump[-1500:])
            played = re.search(r"^ 1 play .*\n\s+calls:.* aout (\d+)", dump, re.M)
            check(played and int(played.group(1)) > 0, "SysRq lists the play service and its audio writes", dump[-1500:])
            check(re.search(r"^loop0: play c1 capture c0", dump, re.M), "SysRq shows who holds loop0's ends", dump[-1500:])
            # The Meters page meters every input of the same capture: the tone lights the first bar.
            deadline = time.monotonic() + 12
            while True:
                send(machine, b"4", 1)
                (machine.monitor.parent / "meters.ppm").unlink(missing_ok=True)
                width, height, pixel = screenshot(machine.monitor, "meters.ppm")
                lit = sum(pixel(x, y) in LIT for x in METER_BAR[0] for y in METER_BAR[1])
                if lit >= 20 or time.monotonic() > deadline:
                    break
            check(lit >= 20, f"the Meters page lights input 1's bar for the tone ({lit} pixels)")
            # The Recorder: a take of both inputs, kept running on another page.
            send(machine, b"2", 1)
            lit = toggle_take(machine, True)
            check(lit >= 20, f"R starts a take: the header's REC lamp is lit ({lit} pixels)")
            began = time.monotonic()
            machine.pump(2)
            send(machine, b"3", 2)
            lit = lamp(machine, "rec-analyzer.ppm")
            check(lit >= 20, f"the take goes on while the Analyzer is shown ({lit} pixels)")
            send(machine, b"2", 1)
            lit = toggle_take(machine, False)
            took = time.monotonic() - began
            check(lit < 20, f"R stops the take: the lamp is out ({lit} pixels)")
            send(machine, b"3", 1)
            # The longest span (Up clamps there, so a lost keystroke does no harm), then Quit at once: the settings wait a
            # second before they are saved, quitting writes them.
            for _ in range(3):
                send(machine, b"\x1b[A", 0.1)
            pointer_to(machine, *QUIT)
            monitor(str(machine.monitor), "mouse_button 1")
            machine.pump(0.3)
            monitor(str(machine.monitor), "mouse_button 0")
            machine.wait_for("fos>", since=start + 10, timeout=10)
            machine.pump(1)
            text = show(machine)
            left, right = sends(text, "Out L"), sends(text, "Out R")
            check(set(left) == {"Play L"} and set(right) == {"Play R"}, f"the pair stays a pair ({left}, {right})", text[-600:])
            check(float(left["Play L"]) < -5 and float(right["Play R"]) < -5, f"the fader click set about -9 dB ({left}, {right})", text[-600:])
            start = len(machine.text())
            send(machine, b"cat settings/kakelfix.cfg\n", 1)
            send(machine, b"cat kakelfix.cfg\n", 1)
            saved = machine.text()[start:]
            check("an-span=30" in saved and "an-view=both" in saved and "mt-scale=" in saved, "the Analyzer's settings are saved in kakelfix.cfg", saved[-600:])
            check("rec-ch=all" in saved and "rec-disk=1" in saved and "rec-name=take" in saved, "the Recorder's settings are saved too", saved[-600:])
            check("rec-format=flac" in saved and "rec-bits=24" in saved, "the format is saved: FLAC 24, the default", saved[-600:])
            # Again with WAV and input 2 alone from the command line: the other writer, and the armed channel picked out. The
            # tone is started again: with a listener loop0 plays faster than real time, so the first run used most of it
            # (if it still plays, the second `play` is refused and the first goes on).
            send(machine, b"run play device=loop0 tone.wav &0\n", 2)
            start = len(machine.text())
            send(machine, b"run kakelfix device=loop0 rec-format=wav rec-ch=2\n", 4)
            send(machine, b"2", 1)
            lit = toggle_take(machine, True)
            check(lit >= 20, f"a WAV take starts ({lit} pixels)")
            machine.pump(3)
            lit = toggle_take(machine, False)
            check(lit < 20, f"the WAV take stops ({lit} pixels)")
            pointer_to(machine, *QUIT)
            monitor(str(machine.monitor), "mouse_button 1")
            machine.pump(0.3)
            monitor(str(machine.monitor), "mouse_button 0")
            machine.wait_for("fos>", since=start + 10, timeout=10)
        finally:
            machine.stop()
        files = fosfs_files(machine.image)
        check("take001.flac" in files, "the first take is disk1/take001.flac", str(sorted(files)))
        channels, rate, bits, total = take_flac(files["take001.flac"], tmp, "take001.flac")
        check((channels, rate, bits) == (2, 48000, 24), f"take001.flac is loop0's 2 channels, 48 kHz, 24 bits ({channels}, {rate}, {bits})")
        check(total > 3 * rate, f"take001.flac holds seconds of samples ({total / rate:.1f} s in a {took:.1f} s take)")
        check("take002.wav" in files and "take001.wav" not in files, "the WAV take is take002.wav (numbers are shared)", str(sorted(files)))
        channels, rate, align, size, sound = take_wav(files["take002.wav"])
        check((channels, rate, align) == (1, 48000, 4), f"take002.wav is input 2 alone, 32-bit float, 48 kHz ({channels}, {rate}, {align})")
        check(size % align == 0 and size > 48000 * 4, f"take002.wav holds whole frames ({size / 192000:.1f} s)")
        check(sound, "take002.wav holds the tone (not all zero)")


def server():
    if not MF_IMAGE.exists():
        raise SystemExit(f"{MF_IMAGE} missing; run `make test-kakelfix`")
    tmp = tempfile.TemporaryDirectory()
    socket_path = str(Path(tmp.name) / "monitor")
    qemu = subprocess.Popen(
        ["qemu-system-x86_64", "-cpu", "max", "-machine", "q35", "-m", "1024", "-smp", "4", "-display", "none",
         "-nic", "user,model=e1000", "-serial", "stdio", "-monitor", f"unix:{socket_path},server,nowait", "-no-reboot",
         "-drive", f"if=pflash,format=raw,readonly=on,file={CODE}", "-drive", f"format=raw,file={MF_IMAGE},snapshot=on"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    log = b""

    def wait_for(text, timeout, start=0):
        nonlocal log
        end = time.time() + timeout
        while time.time() < end and text.encode() not in log[start:]:
            if select.select([qemu.stdout], [], [], 0.2)[0]:
                data = os.read(qemu.stdout.fileno(), 4096)
                if not data:
                    break
                log += data
        return text.encode() in log[start:]

    def type_text(data):
        qemu.stdin.write(data)
        qemu.stdin.flush()

    try:
        check(wait_for("fos> ", 150), "mf: shell prompt", log[-800:].decode("utf8", "replace"))
        boot_log = log.decode("utf8", "replace")
        check("PS/2: mouse on the second port" in boot_log, "mf: the PS/2 mouse (a laptop touchpad's port) is found", boot_log[-1500:])
        mark = first = len(log)
        type_text(b"touch monitor 3\n")
        wait_for("showing finger events", 5, mark)
        for command in ["mouse_move 20 10", "mouse_button 1", "mouse_button 0"]:
            monitor(socket_path, command)
        wait_for("fos> ", 10, mark)
        text = log[mark:].decode("utf8", "replace")
        check("finger 100 down/moved" in text and "finger 100 up" in text, "mf: PS/2 mouse moves and clicks reach the pointer queue",
              text[-800:])
        mark = len(log)
        type_text(b"tape kakelfix device=loop0\n")
        time.sleep(4)
        new = log[mark:].decode("utf8", "replace")
        check("no writer thread" not in new, "mf: the writer thread started", new[-600:])
        type_text(b"x")
        time.sleep(0.2)
        type_text(b"\x1b")
        check(wait_for("exited with code 0", 20, mark), "mf: Escape asks kakelfix to stop and it ends cleanly (writer joined)",
              log[mark:].decode("utf8", "replace")[-800:])
        check(b"killed" not in log[mark:], "mf: Escape did not kill it", log[mark:].decode("utf8", "replace")[-800:])
        wait_for("fos> ", 10, mark)
        mark = len(log)
        type_text(b"tape kakelfix device=loop0 show\n")
        check(wait_for("Dim:", 20, mark), "mf: `show` runs", log[mark:].decode("utf8", "replace")[-800:])
        text = log[mark:].decode("utf8", "replace")
        check(sends(text, "Out L") == {} and sends(text, "Out R") == {}, "mf: `x` from the tape reached the device through the worker", text[-600:])
        new = log[first:].decode("utf8", "replace")
        check("CPU exception" not in new and "killed by exception" not in new, "mf: no fault", new[-800:])
    finally:
        qemu.kill()
        qemu.wait()
        tmp.cleanup()


def main():
    embedded()
    print("kakelfix: embedded kernel checks passed")
    server()
    print("kakelfix: all checks passed")


if __name__ == "__main__":
    main()
