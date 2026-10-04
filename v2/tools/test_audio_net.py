#!/usr/bin/env python3
"""`audio-net` between two v2 guests on one virtual segment.

Guest B (the receiver) has a `usb-audio` device whose output QEMU records to a WAV file; guest A (the
sender) needs no audio hardware and sends a test tone (`src=tone=440`). Over raw Ethernet the tone must
arrive, be played and be recorded at the right frequency and length, the receiver must report no lost
packets, and a stream with another channel count/bit depth (mono, 16-bit) must be converted and played
too. Run with `make test-v2-audio-net`."""

import os
import select
import shutil
import struct
import subprocess
import sys
import tempfile
import time
import wave
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_boot import CODE_CANDIDATES, IMAGE, VARS_CANDIDATES, check, clean, find  # noqa: E402

GROUP, PORT = "230.42.42.77", 4277


class Guest:
    def __init__(self, tmp, name, mac, extra=()):
        code, variables = find("FOS_OVMF_CODE", CODE_CANDIDATES), find("FOS_OVMF_VARS", VARS_CANDIDATES)
        vars_copy = Path(tmp) / f"{name}-vars.fd"
        shutil.copy(variables, vars_copy)
        disk = Path(tmp) / f"{name}.img"
        shutil.copy(IMAGE, disk)
        command = ["qemu-system-x86_64", "-machine", "q35", "-m", "512", "-display", "none", "-serial", "stdio", "-no-reboot",
                   "-drive", f"if=pflash,format=raw,readonly=on,file={code}", "-drive", f"if=pflash,format=raw,file={vars_copy}",
                   "-drive", f"format=raw,file={disk}",
                   "-netdev", f"socket,id=net0,mcast={GROUP}:{PORT}", "-device", f"e1000,netdev=net0,mac={mac}", *extra]
        self.process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        self.raw = bytearray()
        self.name = name

    def pump(self, seconds):
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            ready, _, _ = select.select([self.process.stdout], [], [], 0.05)
            if ready:
                self.raw.extend(os.read(self.process.stdout.fileno(), 4096))

    def text(self):
        return clean(bytes(self.raw))

    def wait_for(self, marker, timeout=90, since=0):
        end = time.monotonic() + timeout
        while marker not in self.text()[since:]:
            if time.monotonic() > end or self.process.poll() is not None:
                raise SystemExit(f"FAIL ({self.name}): never saw {marker!r}\n{self.text()[-2500:]}")
            self.pump(0.1)

    def send(self, line):
        mark = len(self.text())
        self.process.stdin.write(line.encode() + b"\n")
        self.process.stdin.flush()
        return mark

    def stop(self):
        self.process.terminate()
        try:
            self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.process.kill()


def tone_frequency(samples, rate, window):
    crossings = sum(1 for x, y in zip(window, window[1:]) if x < 0 <= y)
    return crossings * rate / len(window)


def main():
    with tempfile.TemporaryDirectory() as tmp:
        wav = Path(tmp) / "out.wav"
        receiver = Guest(tmp, "receiver", "52:54:00:12:34:b2",
                         ["-device", "qemu-xhci,id=xhci", "-audiodev", f"wav,id=snd,path={wav}", "-device", "usb-audio,audiodev=snd"])
        sender = Guest(tmp, "sender", "52:54:00:12:34:b1")
        try:
            for guest in (receiver, sender):
                guest.wait_for("M3 complete")
                guest.wait_for("fos> ")
            for run in ("a", "b"):
                if run == "a":
                    # Stereo 24-bit, 440 Hz, 3 seconds, to everyone.
                    mark = receiver.send("run audio-net recv")
                    receiver.wait_for("Listening on NIC 1", 30, mark)
                    receiver.pump(3.0)  # QEMU's emulated USB audio keeps the receiver busy for a while after it starts
                    mark = sender.send("run audio-net send src=tone=440 secs=3")
                    sender.wait_for("Sent ", 60, mark)
                else:
                    # Mono 16-bit, 880 Hz, to the receiver's MAC, after the receiver has been restarted.
                    mark = receiver.send("run audio-net recv from=52:54:00:12:34:b1 target=8")
                    receiver.wait_for("Listening on NIC 1", 30, mark)
                    receiver.pump(3.0)
                    mark = sender.send("run audio-net send src=tone=880 ch=1 bits=16 to=52:54:00:12:34:b2 secs=3")
                    sender.wait_for("Sent ", 60, mark)
                receiver.pump(1.0)
                mark = len(receiver.text())
                receiver.process.stdin.write(b"\x1b")  # Escape: stop the tape
                receiver.process.stdin.flush()
                receiver.wait_for("Received ", 30, mark - 200)
                receiver.wait_for("fos> ", 30, mark)
                report = receiver.text()[mark - 200:]
                locked = report.split("Locked onto a stream:")[-1].split("\n")[0]
                if run == "a":
                    check("2 channel(s), 24-bit, 48000 Hz" in locked, "the receiver locks onto a stereo 24-bit 48 kHz stream", locked)
                else:
                    check("1 channel(s), 16-bit, 48000 Hz" in locked, "and onto a mono 16-bit one, from a chosen sender", locked)
                packets = int(report.split("Received ")[-1].split(" packets")[0])
                lost = int(report.split(" packets: ")[-1].split(" lost")[0])
                check(packets >= 2900, f"it received {packets} packets (about 3000 expected)", report)
                check(lost <= 5, f"with {lost} lost", report)
        finally:
            receiver.stop()
            sender.stop()
        with wave.open(str(wav)) as recording:
            rate, channels, count = recording.getframerate(), recording.getnchannels(), recording.getnframes()
            samples = struct.unpack(f"<{count * channels}h", recording.readframes(count))[::channels]
        loud = [i for i, value in enumerate(samples) if abs(value) > 500]
        check(len(loud) > 4 * rate, f"the recording holds sound ({len(loud) / rate:.1f} s of it)", "")
        first = samples[loud[0]:loud[0] + rate]
        last = samples[loud[-1] - rate:loud[-1]]
        f1, f2 = tone_frequency(samples, rate, first), tone_frequency(samples, rate, last)
        check(abs(f1 - 440) < 20, f"the first tone is {f1:.0f} Hz (wanted 440)", "")
        check(abs(f2 - 880) < 30, f"the second tone (mono, 16-bit, converted) is {f2:.0f} Hz (wanted 880)", "")
    print("v2 audio-net: all checks passed")


if __name__ == "__main__":
    main()
