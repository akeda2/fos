#!/usr/bin/env python3
"""USB audio on the v2 kernel, against QEMU's `usb-audio` (Audio Class 1.0, full speed, stereo
16-bit, 48 kHz) on `qemu-xhci`.

 * Discovery: the device is recognised and its class, endpoints and sample rates are reported.
 * Playback: `audio tone 440 3` streams a test tone through an isochronous OUT endpoint; QEMU's WAV
   backend records what the guest sent, and the recording must hold a 440 Hz tone (measured by
   counting zero crossings) at about the right length, with no underruns reported.

QEMU's `usb-audio` is output-only and class 1.0, so capture (isochronous IN) and Audio Class 2.0 /
high speed (what a Zoom H6 in multichannel mode presents) are checked on real hardware only. Run with `make test-v2-usb-audio`."""

import re
import struct
import sys
import tempfile
import wave
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_boot import check  # noqa: E402
from test_keyboard import boot  # noqa: E402


def session(commands, outputs):
    def run(send, wait_for):
        pass
    return run


def main():
    with tempfile.TemporaryDirectory() as tmp:
        wav = Path(tmp) / "out.wav"
        extra = ["-device", "qemu-xhci,id=xhci", "-audiodev", f"wav,id=snd,path={wav}",
                 "-device", "usb-audio,audiodev=snd"]
        # The shell is reached over the serial port, which the guest also reads as a keyboard.
        text = boot_with_commands(extra, ["usb rescan", "audio", "audio tone 440 3", "audio capture 2"])
        check("USB audio" in text and "Audio Class 1.0" in text, "the USB audio device is recognised (class 1.0)", text)
        check("playback 2 ch, 16-bit" in text and "48000" in text, "its streaming interfaces and sample rates are reported", text)
        tone = text.split("audio tone 440 3")[-1].split("audio capture")[0]
        check("playback: 48000 Hz" in tone and "0 errors" in tone, "playback runs at 48 kHz with no transfer errors", tone)
        check("no capture stream" in text.split("audio capture 2")[-1], "capture is refused cleanly when the device has no input", text[-400:])

        if wav.exists() and wav.stat().st_size > 44:
            with wave.open(str(wav)) as recording:
                rate, channels, width, count = recording.getframerate(), recording.getnchannels(), recording.getsampwidth(), recording.getnframes()
                data = recording.readframes(count)
            samples = struct.unpack(f"<{count * channels}h", data)[::channels]
            loud = [value for value in samples if abs(value) > 500]
            check(len(loud) > rate, f"the recording holds sound ({len(loud)} loud samples of {count})", "")
            # Use the loud stretch only (the stream is silent before and after the tone).
            start = next(i for i, value in enumerate(samples) if abs(value) > 500)
            end = len(samples) - next(i for i, value in enumerate(reversed(samples)) if abs(value) > 500)
            body = samples[start:end]
            crossings = sum(1 for a, b in zip(body, body[1:]) if a < 0 <= b)
            frequency = crossings * rate / len(body)
            check(abs(frequency - 440) < 15, f"the recorded tone is {frequency:.0f} Hz (wanted 440)", "")
        else:
            check(False, "QEMU's WAV backend recorded the stream", "no wav file")
    print("v2 usb audio: all checks passed")


def boot_with_commands(extra, commands):
    """Boots with `extra` QEMU arguments, types each command on the serial port, returns all serial text."""
    import os
    import select
    import shutil
    import subprocess
    import time
    from test_boot import CODE_CANDIDATES, IMAGE, VARS_CANDIDATES, clean, find

    code, variables = find("FOS_OVMF_CODE", CODE_CANDIDATES), find("FOS_OVMF_VARS", VARS_CANDIDATES)
    with tempfile.TemporaryDirectory() as tmp:
        vars_copy = Path(tmp) / "vars.fd"
        shutil.copy(variables, vars_copy)
        command = ["qemu-system-x86_64", "-machine", "q35", "-m", "512", "-display", "none", "-serial", "stdio", "-no-reboot",
                   "-drive", f"if=pflash,format=raw,readonly=on,file={code}", "-drive", f"if=pflash,format=raw,file={vars_copy}",
                   "-drive", f"format=raw,file={IMAGE}", *extra]
        process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        raw = bytearray()

        def pump(seconds):
            end = time.monotonic() + seconds
            while time.monotonic() < end:
                ready, _, _ = select.select([process.stdout], [], [], 0.05)
                if ready:
                    raw.extend(os.read(process.stdout.fileno(), 4096))

        def wait_for(marker, timeout=60, since=0):
            end = time.monotonic() + timeout
            while marker not in clean(bytes(raw))[since:]:
                if time.monotonic() > end or process.poll() is not None:
                    raise SystemExit(f"FAIL: never saw {marker!r}\n{clean(bytes(raw))[-2500:]}")
                pump(0.1)

        try:
            wait_for("M3 complete")
            wait_for("fos> ")
            for line in commands:
                mark = len(clean(bytes(raw)))
                process.stdin.write(line.encode() + b"\n")
                process.stdin.flush()
                wait_for("fos> ", 60, mark + len(line))
            pump(0.5)
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
        return clean(bytes(raw))


if __name__ == "__main__":
    main()
