#!/usr/bin/env python3
"""Intel HD Audio on the v2 kernel, against QEMU's `intel-hda` controller with the `hda-duplex` codec.

 * Discovery: the controller, its codec and the widget graph are found, a playback path (output pin 3 <- DAC 2) and a
   capture path (ADC 4 <- input pin 5) are chosen, and an interrupt path (MSI, then INTx with `msi=off`, then polled
   with `no-msi` in the settings file) passes its self-test. The `audio` listing, `audio modes` and the `hda` dump show it.
 * Playback: `audio tone 440 3` streams a tone; QEMU's WAV backend records what the codec played, and the recording
   must hold a 440 Hz tone. Audio keeps flowing while the foreground is blocked (`sleep`). A bad rate is refused.
 * The same tape calls as USB audio: the unchanged `play` tape plays a stereo 16-bit file and a mono 24-bit one on
   the HD Audio device, `record` records from it.
 * Capture: QEMU's WAV backend has no input side ("Could not create a backend for voice `adc'"), so capture runs
   against the `none` backend, whose input is silence. That still proves the stream runs, the controller's DMA
   overwrote the 0x55 pattern the driver pre-fills the capture buffer with (a peak of 0 means it did), the period
   interrupts arrive, and the `record` tape writes a take of the right length. QEMU cannot supply a real input
   *signal*, so what the driver does with one (levels, gain, the microphone bias) is checked on real hardware only.
 * Both device kinds at once: with a USB audio device present as well, `audio` lists both, USB stays the automatic
   choice, `audio device 2` switches to HD Audio and the tone lands in the HD Audio recording, not the USB one.

Run with `make test-hda`."""

import re
import struct
import subprocess
import sys
import tempfile
import wave
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_boot import ROOT, check  # noqa: E402
from test_usb_audio import boot_with_commands, segments, write_wav  # noqa: E402

LOADER = ROOT / "build/cargo-target/x86_64-unknown-uefi/release/fosfor-loader.efi"
KERNEL = ROOT / "build/cargo-target/x86_64-fos-kernel/release/fosfor-kernel"
TAPES = ROOT / "build/cargo-target/x86_64-fos-tape/release"


def make_image(tmp, files):
    """A copy of the standard image with extra files: {name: Path}, tapes by name from the tape build directory."""
    image = Path(tmp) / "hda.img"
    command = [sys.executable, str(ROOT / "tools/build_image.py"), "--loader", str(LOADER), "--kernel", str(KERNEL), "--output", str(image)]
    for name, path in files.items():
        command += ["--file", f"{name}={path}"]
    subprocess.run(command, check=True, capture_output=True)
    return image


def read_wav(path):
    with wave.open(str(path)) as recording:
        rate, channels, count = recording.getframerate(), recording.getnchannels(), recording.getnframes()
        samples = struct.unpack(f"<{count * channels}h", recording.readframes(count))[::channels]
    return rate, samples


def frequency_of(part, rate):
    crossings = sum(1 for a, b in zip(part, part[1:]) if a < 0 <= b)
    return crossings * rate / len(part)


def check_tone(path, wanted, label):
    check(path.exists() and path.stat().st_size > 44, f"QEMU's WAV backend recorded the HD Audio stream ({label})", "no wav file")
    rate, samples = read_wav(path)
    loud = [value for value in samples if abs(value) > 500]
    check(len(loud) > rate, f"the recording holds sound ({len(loud)} loud samples of {len(samples)})", "")
    start = next(i for i, value in enumerate(samples) if abs(value) > 500)
    end = len(samples) - next(i for i, value in enumerate(reversed(samples)) if abs(value) > 500)
    frequency = frequency_of(samples[start:end], rate)
    check(abs(frequency - wanted) < 15, f"the recorded tone is {frequency:.0f} Hz (wanted {wanted})", "")
    return rate, samples


def discovery_and_playback():
    with tempfile.TemporaryDirectory() as tmp:
        wav = Path(tmp) / "out.wav"
        extra = ["-audiodev", f"wav,id=snd,path={wav}", "-device", "intel-hda", "-device", "hda-duplex,audiodev=snd"]
        text = boot_with_commands(extra, ["audio", "hda", "audio modes", "audio start 12345 playback", "audio use playback 9", "audio tone 440 3",
                                          "audio start 48000 playback", "sleep 3", "audio", "audio stop", "audio start 48000 capture", "audio start 48000 both", "audio stop", "audio start 48000 both", "sleep 1", "audio", "audio stop"])
        check(re.search(r"HD Audio 00:03\.0 \(8086:2668\): version 1\.0, 4 input / 4 output stream\(s\), (64|32)-bit DMA, 1 codec\(s\)", text) is not None, "the HD Audio controller is found and reset", text[:3000])
        check("codec 0 1af4:0022: audio function group 0x1, 4 widget(s)" in text, "its codec (QEMU's hda-duplex) and function group are found", text[:3000])
        check("playback path: pin 0x3 via 1 widget(s) to 0x2" in text and "capture path: pin 0x5 via 1 widget(s) from 0x4" in text, "the widget graph yields a playback path (pin 3 <- DAC 2) and a capture path (ADC 4 <- pin 5)", text[:3000])
        check(re.search(r"interrupts: MSI on vector 0x[0-9a-f]+ \(the self-test message arrived\)", text.split("HD Audio 00:03.0")[1]) is not None, "the controller takes its interrupts by MSI (the self-test message arrived)", text[:3000])
        listing = text.split("fos> audio")[1]
        check("HD Audio 00:03.0 sample rates:" in listing and "48000" in listing and "capture: stopped (2 ch, 16-bit)" in listing and "playback: stopped (2 ch, 16-bit)" in listing, "`audio` lists the device, its rates and both directions", listing)
        dump = text.split("fos> hda")[1].split("fos> ")[0]
        check("0x02 output" in dump and "0x03 pin" in dump and "0x04 input" in dump and "0x05 pin" in dump and "<- 0x2" in dump and "position buffer on" in dump, "`hda` dumps the widgets, their connections and the DMA position buffer state", dump)
        modes = text.split("fos> audio modes")[1].split("fos> ")[0]
        check("1: 2 ch, 16-bit," in modes and "Playback modes" in modes and "Capture modes" in modes, "`audio modes` lists the converters' modes", modes)
        check("the device does not offer that sample rate" in text.split("audio start 12345 playback")[1].split("fos> ")[0], "a sample rate the converter lacks is refused", text[-600:])
        check("no such mode" in text.split("audio use playback 9")[1].split("fos> ")[0], "a mode that does not exist is refused", text[-600:])
        tone = text.split("audio tone 440 3")[-1].split("audio start 48000 playback")[0]
        check("playback: 48000 Hz, 2 ch 16-bit" in tone and "0 errors" in tone, "playback runs at 48 kHz with no stream errors", tone)
        blocked = text.split("audio start 48000 playback")[-1].split("audio stop")[0]
        periods = [int(n) for n in re.findall(r"playback: 48000 Hz, 2 ch 16-bit, (\d+) periods", blocked)]
        check(periods and periods[-1] > 250, f"audio keeps streaming while the foreground is blocked ({periods} periods after a 3 s sleep; ~280 expected)", blocked[-600:])
        check("already running" in text.split("audio start 48000 both")[1].split("fos> ")[0], "a second start while streams run is refused", text[-800:])
        both = text.split("audio start 48000 both")[-1].split("fos> audio stop")[0]
        check("capture: 48000 Hz" in both and "playback: 48000 Hz" in both, "capture and playback can be started together", both[-800:])
        check_tone(wav, 440, "audio tone")


def tapes_playback():
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        write_wav(tmp / "a.wav", 48_000, 2, 2, 440, 1.5)   # stereo 16-bit
        write_wav(tmp / "b.wav", 48_000, 1, 3, 880, 1.5)   # mono 24-bit: converted to the device's format
        image = make_image(tmp, {"play": TAPES / "play", "a.wav": tmp / "a.wav", "b.wav": tmp / "b.wav", "debug": ROOT / "tools/default-debug.txt"})
        wav = tmp / "out.wav"
        extra = ["-audiodev", f"wav,id=snd,path={wav}", "-device", "intel-hda", "-device", "hda-duplex,audiodev=snd"]
        text = boot_with_commands(extra, ["run play a.wav", "run play b.wav", "run play missing.wav"], image)
        check("Playing a.wav: 48000 Hz, 2 channel(s), 16-bit" in text and "Finished" in text, "the unchanged `play` tape plays a stereo 16-bit WAV file on HD Audio", text[-1500:])
        check("Playing b.wav: 48000 Hz, 1 channel(s), 24-bit" in text and "on 2 output channel(s) of 16-bit" in text, "and a mono 24-bit file (converted to the device's 16-bit stereo)", text[-1500:])
        check("cannot read missing.wav" in text, "a missing file is reported", text[-500:])
        rate, samples = read_wav(wav)
        loud = [i for i, value in enumerate(samples) if abs(value) > 500]
        start, end = loud[0], loud[-1]
        check((end - start) / rate > 2.5, f"the recording holds both tones ({(end - start) / rate:.1f} s of sound)", "")
        for part, wanted in ((samples[start:start + rate], 440), (samples[end - rate:end], 880)):
            frequency = frequency_of(part, rate)
            check(abs(frequency - wanted) < 20, f"{'the first' if wanted == 440 else 'the last'} second is {frequency:.0f} Hz (wanted {wanted})", "")


def capture_tests():
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        image = make_image(tmp, {"record": TAPES / "record", "debug": ROOT / "tools/default-debug.txt"})
        extra = ["-audiodev", "none,id=snd", "-device", "intel-hda", "-device", "hda-duplex,audiodev=snd"]
        text = boot_with_commands(extra, ["audio capture 3", "audio duplex 2", "run record cap 3 48000"], image)
        capture = text.split("fos> audio capture 3")[1].split("fos> ")[0]
        frames = [int(n) for n in re.findall(r"\[ ?\d s\] (\d+) frames", capture)]
        check(len(frames) == 3 and all(f > 44_000 for f in frames), f"capture delivers about 48000 frames a second ({frames})", capture)
        check(all(peak == 0 for peak in map(int, re.findall(r"peak (\d+)", capture))), "the captured frames are silence: the controller's DMA overwrote the 0x55 fill pattern", capture)
        check("capture: 48000 Hz, 2 ch 16-bit" in capture and " 0 errors" in capture and "(0 overruns)" in capture, "no stream errors and no ring overruns while capturing", capture)
        duplex = text.split("fos> audio duplex 2")[1].split("fos> ")[0]
        check("capture: 48000 Hz" in duplex and "playback: 48000 Hz" in duplex and "0 errors" in duplex, "duplex (capture and playback together) runs without errors", duplex)
        record = text.split("fos> run record cap 3 48000")[1]
        check(re.search(r"Recording 2 channel\(s\), 16-bit, 48000 Hz into cap\.wav", record) is not None, "the unchanged `record` tape records from the HD Audio input", record[-1200:])
        frames = re.search(r"Recorded (\d+) frames", record)
        check(frames is not None and 140_000 <= int(frames.group(1)) <= 148_000, f"the take holds the right length ({frames.group(1) if frames else None} frames of 144000)", record[-1200:])


def interrupt_paths():
    """INTx (the controller without MSI) and polled (`no-msi`)."""
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        settings = tmp / "settings"
        settings.write_text("no-msi\n")
        polled_image = make_image(tmp, {"settings": settings, "debug": ROOT / "tools/default-debug.txt"})
        for label, device, image, expected in (
            ("legacy INTx", "intel-hda,msi=off", None, "legacy INTx, I/O APIC pin"),
            ("polled (no-msi)", "intel-hda", polled_image, "HD Audio interrupts: polled (settings: no-msi)"),
        ):
            wav = tmp / f"{label.split()[0]}.wav"
            extra = ["-audiodev", f"wav,id=snd,path={wav}", "-device", device, "-device", "hda-duplex,audiodev=snd"]
            text = boot_with_commands(extra, ["audio tone 440 3"], image)
            check(expected in text, f"{label}: the interrupt path is chosen ({expected!r})", text[:3500])
            tone = text.split("fos> audio tone 440 3")[1]
            check("playback: 48000 Hz, 2 ch 16-bit" in tone and "0 errors" in tone, f"{label}: playback runs", tone)
            check_tone(wav, 440, label)


def both_devices():
    """USB audio and HD Audio together: USB is the automatic choice, `audio device 2` picks HD Audio."""
    with tempfile.TemporaryDirectory() as tmp:
        usb_wav, hda_wav = Path(tmp) / "usb.wav", Path(tmp) / "hda.wav"
        extra = ["-device", "qemu-xhci,id=xhci", "-audiodev", f"wav,id=usbsnd,path={usb_wav}", "-device", "usb-audio,audiodev=usbsnd",
                 "-audiodev", f"wav,id=hdasnd,path={hda_wav}", "-device", "intel-hda", "-device", "hda-duplex,audiodev=hdasnd"]
        text = boot_with_commands(extra, ["usb rescan", "audio", "audio device 3", "audio device 2", "audio tone 880 3", "audio device auto", "audio"])
        listing = text.split("fos> audio")[1].split("fos> ")[0]
        check("Audio devices" in listing and "*1: USB audio" in listing and " 2: HD Audio 00:" in listing, "`audio` lists USB audio and HD Audio, USB audio being the automatic choice", listing)
        check("no such device" in text.split("audio device 3")[1].split("fos> ")[0], "a device number that does not exist is refused", text[-800:])
        picked = text.split("fos> audio device 2")[1].split("fos> ")[0]
        check(" 1: USB audio" in picked and "*2: HD Audio" in picked, "`audio device 2` selects the HD Audio device", picked)
        check("playback: 48000 Hz" in text.split("audio tone 880 3")[1].split("fos> ")[0], "playback on the picked device runs", text[-800:])
        check_tone(hda_wav, 880, "HD Audio picked next to USB audio")
        if usb_wav.exists() and usb_wav.stat().st_size > 44:
            rate, samples = read_wav(usb_wav)
            check(not [value for value in samples if abs(value) > 500], "and nothing reached the USB device", "")
        check("*1: USB audio" in text.split("audio device auto")[-1], "`audio device auto` returns to the automatic choice", text[-800:])


def tape_picks_device():
    """A tape chooses the device itself (`play device=2`, `audio_use_device`) and the operator's choice is not disturbed."""
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        usb_wav, hda_wav = tmp / "usb.wav", tmp / "hda.wav"
        write_wav(tmp / "a.wav", 48_000, 2, 2, 660, 1.5)
        image = make_image(tmp, {"play": TAPES / "play", "a.wav": tmp / "a.wav", "debug": ROOT / "tools/default-debug.txt"})
        extra = ["-device", "qemu-xhci,id=xhci", "-audiodev", f"wav,id=usbsnd,path={usb_wav}", "-device", "usb-audio,audiodev=usbsnd",
                 "-audiodev", f"wav,id=hdasnd,path={hda_wav}", "-device", "intel-hda", "-device", "hda-duplex,audiodev=hdasnd"]
        text = boot_with_commands(extra, ["usb rescan", "run play device=2 a.wav", "audio", "run play device=9 a.wav"], image)
        check("Playing a.wav: 48000 Hz, 2 channel(s), 16-bit" in text.split("run play device=2 a.wav")[1].split("fos> ")[0],
              "`play device=2` plays on the second device (HD Audio) although USB audio is the automatic choice", text[-1500:])
        check_tone(hda_wav, 660, "tape-chosen HD Audio")
        if usb_wav.exists() and usb_wav.stat().st_size > 44:
            rate, samples = read_wav(usb_wav)
            check(not [value for value in samples if abs(value) > 500], "and nothing reached the USB device", "")
        after = text.split("fos> audio")[-1].split("fos> ")[0]
        check("*1: USB audio" in after, "after the tape ended the automatic choice is back (the tape's pick is forgotten)", after)
        check("there is no such audio device" in text, "a device number that does not exist is refused by the tape call", text[-600:])


def main():
    discovery_and_playback()
    tapes_playback()
    capture_tests()
    interrupt_paths()
    both_devices()
    tape_picks_device()
    print("hda: all checks passed")


if __name__ == "__main__":
    main()
