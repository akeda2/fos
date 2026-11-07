#!/usr/bin/env python3
"""`audio-net sip`: two guests on one segment, SIP call control in front of the RTP stream.

Guest B (usb-audio, recorded to WAV) waits for a call, guest A places it and sends a 440 Hz tone. Three calls:
a PTP-required callee refuses a stream with no PTP clock (488, the AES67 default); with `require-ptp=off` the call is
set up, the tone arrives and plays, and A's BYE ends B's stream; a second call is accepted afterwards. Run with
`make test-sip`."""

import struct
import sys
import tempfile
import wave
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_audio_net import Guest, tone_frequency  # noqa: E402
from test_boot import check  # noqa: E402

ESC = b"\x1b"


def main():
    with tempfile.TemporaryDirectory() as tmp:
        wav = Path(tmp) / "out.wav"
        b = Guest(tmp, "callee", "52:54:00:12:34:b2",
                  ["-device", "qemu-xhci,id=xhci", "-audiodev", f"wav,id=snd,path={wav}", "-device", "usb-audio,audiodev=snd"])
        a = Guest(tmp, "caller", "52:54:00:12:34:b1")
        try:
            for guest in (b, a):
                guest.wait_for("M3 complete")
                guest.wait_for("fos> ")

            # 1. PTP is required by default and no clock is on this network.
            mark_b = b.send("run audio-net sip ip=192.168.50.2/24")
            b.wait_for("waiting for a call", 30, mark_b)
            b.pump(2.0)
            mark_a = a.send("run audio-net sip ip=192.168.50.1/24 call=192.168.50.2 src=tone=440 secs=3")
            a.wait_for("call failed (488)", 40, mark_a)
            check("PTP required" in b.text()[mark_b:], "the callee announces that it requires PTP", b.text()[mark_b:][-600:])
            b.process.stdin.write(ESC)
            b.process.stdin.flush()
            b.wait_for("fos> ", 20, mark_b + 1)

            # 2. require-ptp=off: the call comes up, audio flows, BYE ends it.
            mark_b = b.send("run audio-net sip ip=192.168.50.2/24 require-ptp=off")
            b.wait_for("waiting for a call", 30, mark_b)
            b.pump(3.0)
            mark_a = a.send("run audio-net sip ip=192.168.50.1/24 call=192.168.50.2 src=tone=440 secs=4 require-ptp=off")
            a.wait_for("call up, sending", 40, mark_a)
            b.wait_for("call up, receiving", 20, mark_b)
            b.wait_for("call over", 40, mark_b)
            a.wait_for("call over", 20, mark_a)
            text = b.text()[mark_b:]
            check("24-bit 2 ch 48000 Hz" in text, "the call negotiated L24/48000/2", text[-600:])

            # 3. The callee is free again: a second call works.
            mark_a = a.send("run audio-net sip ip=192.168.50.1/24 call=192.168.50.2 src=tone=440 secs=2 require-ptp=off")
            a.wait_for("call over", 40, mark_a)
            b.pump(2.0)
            check(b.text().count("call up, receiving") == 2, "a second call was accepted after the first", b.text()[-600:])
            b.process.stdin.write(ESC)
            b.process.stdin.flush()
            b.pump(1.0)
        finally:
            b.stop()
            a.stop()
        with wave.open(str(wav)) as recording:
            rate, channels, count = recording.getframerate(), recording.getnchannels(), recording.getnframes()
            samples = struct.unpack(f"<{count * channels}h", recording.readframes(count))[::channels]
        loud = [i for i, value in enumerate(samples) if abs(value) > 500]
        check(len(loud) > 2 * rate, f"the recording holds sound ({len(loud) / rate:.1f} s of it)", "")
        start = loud[0] + rate // 2  # inside the first call's tone
        measured = tone_frequency(samples, rate, samples[start:start + rate])
        check(abs(measured - 440) < 30, f"the callee played the caller's tone: {measured:.0f} Hz", "")
    print("PASS: SIP call control for audio-net")


if __name__ == "__main__":
    main()
