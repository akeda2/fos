#!/usr/bin/env python3
"""fosplay plays a FLAC, MP3, WAV or Opus file from FOSFS into loop0 while `record device=loop0` captures it; the recording holds
the tone at the right pitch, for FLAC at 48 and 44.1 kHz and MP3 at 44.1 kHz stereo and 22.05 kHz mono. Needs ffmpeg on the host (SKIP without). No audio
hardware or network needed. Run with `make test-fosplay`."""

import re
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_audio_net import Guest, tone_frequency  # noqa: E402
from test_boot import check, monitor, read_ppm  # noqa: E402
from test_loopback import ask, float_wav  # noqa: E402
from test_record import fosfs_files  # noqa: E402

MAC = "52:54:00:12:36:02"
TONE_HZ = 1000


# (file name, sample rate, channels, ffmpeg output options)
CASES = [("t48.flac", 48000, 2, ["-sample_fmt", "s16"]), ("t44.flac", 44100, 2, ["-sample_fmt", "s16"]),
         ("m44.mp3", 44100, 2, ["-b:a", "192k"]), ("m22.mp3", 22050, 1, ["-b:a", "64k"]),
         ("r44.flac", 48000, 2, ["-sample_fmt", "s16"]),
         ("w44.wav", 44100, 2, ["-c:a", "pcm_s24le"]), ("w8.wav", 22050, 1, ["-c:a", "pcm_u8"]),
         ("o48.opus", 48000, 2, ["-c:a", "libopus", "-b:a", "96k", "-metadata", "title=Opus Tune"]),
         ("o24.opus", 24000, 1, ["-c:a", "libopus", "-b:a", "48k"])]
# Files played with an output rate asked for (rate=44100: the loopback may refuse it; fosplay then falls back and resamples).
PLAY_OPTIONS = {"r44.flac": "rate=44100 "}


def start_service(guest, line):
    """Types `line` until the shell says the service started: a line typed while the shell prints another service's end
    message can be lost."""
    for _ in range(4):
        mark = guest.send(line)
        end = time.monotonic() + 6
        while time.monotonic() < end:
            guest.pump(0.2)
            if "Service" in guest.text()[mark:] and "started" in guest.text()[mark:]:
                return
    check(False, f"`{line}` starts a service", guest.text()[-400:])


def listed(guest, *names):
    """True while `ps` lists one of the services: only its listing lines count, not an asynchronous "Service record
    ended." message that lands in the same stretch of console output."""
    return bool(re.search(rf"^\d+\s+({'|'.join(names)})\b", ask(guest, "ps", "fos> "), re.M))


def make_audio(path, rate, channels, options):
    subprocess.run(["ffmpeg", "-v", "error", "-y", "-f", "lavfi", "-i", f"sine=frequency={TONE_HZ}:sample_rate={rate}:duration=12",
                    "-ac", str(channels), *options, str(path)], check=True)


def main():
    if not shutil.which("ffmpeg"):
        print("SKIP: fosplay (no ffmpeg)")
        return
    with tempfile.TemporaryDirectory() as tmp:
        files = {}
        for name, rate, channels, options in CASES:
            path = Path(tmp) / name
            make_audio(path, rate, channels, options)
            files[name] = path.read_bytes()
        # A playlist of two short files (FLAC then MP3, different rates), played back to back.
        for name, rate, channels, options in [("p1.flac", 48000, 2, ["-sample_fmt", "s16"]), ("p2.mp3", 44100, 2, ["-b:a", "192k"])]:
            path = Path(tmp) / name
            subprocess.run(["ffmpeg", "-v", "error", "-y", "-f", "lavfi", "-i", f"sine=frequency={TONE_HZ}:sample_rate={rate}:duration=3",
                            "-ac", str(channels), *options, "-metadata", f"title=Song {name[1]}", "-metadata", "artist=Bj\u00f6rk Quartet",
                            str(path)], check=True)
            files[name] = path.read_bytes()
        guest = Guest(tmp, "fosplay", MAC, files=files)
        try:
            guest.wait_for("M3 complete")
            guest.wait_for("fos> ")
            for name, rate, _, _ in CASES:
                take = "rec-" + name.split(".")[0]
                guest.send(f"run record device=loop0 {take} 4 &0")
                guest.pump(2)
                guest.send(f"run fosplay device=loop0 gfx=off {PLAY_OPTIONS.get(name, "")}play {name} &0")
                deadline = time.monotonic() + 90
                while time.monotonic() < deadline:
                    if not listed(guest, "record"):
                        break
                    guest.pump(2)
                check(not listed(guest, "record"), f"the {name} recording finishes", guest.text()[-600:])
                guest.pump(8)
            # The playlist: both tracks play, the second named [2/2], and the summary counts them.
            guest.send("run record device=loop0 list 9 &0")
            guest.pump(2)
            guest.send("run fosplay device=loop0 gfx=off play p1.flac p2.mp3 &0")
            deadline = time.monotonic() + 90
            while time.monotonic() < deadline:
                if not listed(guest, "record"):
                    break
                guest.pump(2)
            guest.pump(3)
            text = guest.text()
            check("Song 1 - Bj\u00f6rk Quartet" in text and "Song 2 - Bj\u00f6rk Quartet" in text, "titles and artists come from the Vorbis and ID3 tags", text[-800:])
            check("[1/2] p1.flac" in text and "[2/2] p2.mp3" in text, "the playlist names both tracks", text[-800:])
            check("2 track(s)" in text, "the playlist ends with a summary of 2 tracks", text[-800:])
        finally:
            guest.stop()
        check("Opus Tune" in guest.text() and "Opus" in guest.text(), "the Opus title is shown", guest.text()[-600:])
        out = fosfs_files(Path(tmp) / "fosplay.img")
        for name, rate, _, _ in CASES:
            take = "rec-" + name.split(".")[0]
            blob = out.get(f"{take}.wav")
            check(blob is not None, f"{take}.wav was written", str(sorted(out)) + guest.text()[-900:])
            samples, got = float_wav(blob)
            loud = [x for x in samples if abs(x) > 0.05]
            check(len(loud) > got // 2, f"{name}: the recording holds sound ({len(loud)} of {len(samples)})", guest.text()[-600:])
            middle = len(loud) and len(samples) // 2
            measured = tone_frequency(samples, got, samples[middle - got // 8:middle + got // 8])
            check(abs(measured - TONE_HZ) < 20, f"{name}: recorded tone is {measured:.0f} Hz (wanted {TONE_HZ})", "")
        blob = out.get("list.wav")
        check(blob is not None, "list.wav was written", str(sorted(out)))
        samples, got = float_wav(blob)
        loud = [i for i, x in enumerate(samples) if abs(x) > 0.05]
        check(len(loud) > 5 * got // 2, f"the playlist recording holds both tracks ({len(loud)} loud samples)", "")
    jump_test()
    bass_test()
    ui_test()
    config_test()
    browse_test()
    print("PASS: fosplay")


def jump_test():
    """`f` and `b` while a track plays (the tape runs in the foreground, so the keys reach it). The loopback has no clock, so
    a 40 s track plays in about a second; `f` is therefore pressed every 100 ms from the moment the command is typed, and
    the test only asks that the position is reported and that the jumps left part of the track unplayed."""
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / "seek.flac"
        subprocess.run(["ffmpeg", "-v", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=500:sample_rate=48000:duration=40", "-ac", "2",
                        "-sample_fmt", "s16", str(path)], check=True)
        guest = Guest(tmp, "jump", MAC, files={"seek.flac": path.read_bytes()})
        try:
            guest.wait_for("M3 complete")
            guest.wait_for("fos> ")
            guest.send("run record device=loop0 jump 30 &0")
            guest.pump(2)
            mark = guest.send("run fosplay device=loop0 gfx=off play seek.flac")
            end = time.monotonic() + 4
            while time.monotonic() < end and "Tape returned" not in guest.text()[mark:]:
                guest.process.stdin.write(b"f")
                guest.process.stdin.flush()
                guest.pump(0.1)
            guest.wait_for("Tape returned", 30, since=mark)
            text = guest.text()[mark:]
            check("fosplay: at 0:" in text and " of 0:40." in text, "f jumps and reports the position", text[-800:])
            packets = int(text.split("track(s), ")[1].split(" packets")[0].split()[-1])
            check(packets < 40 * 48000, f"the jumps skipped part of the track ({packets} of {40 * 48000} frames played)", text[-800:])
            guest.send("kill 1")
        finally:
            guest.stop()


def bass_test():
    """`bass=9`: the boost peaks at 100 Hz and the level drops by 9 dB, so 100 Hz comes out 9 dB above 1 kHz (the two files
    have the same level)."""
    with tempfile.TemporaryDirectory() as tmp:
        files = {}
        for name, hz in (("b100.flac", 100), ("b1k.flac", 1000)):
            path = Path(tmp) / name
            subprocess.run(["ffmpeg", "-v", "error", "-y", "-f", "lavfi", "-i", f"sine=frequency={hz}:sample_rate=48000:duration=12",
                            "-af", "volume=0.5", "-ac", "2", "-sample_fmt", "s16", str(path)], check=True)
            files[name] = path.read_bytes()
        guest = Guest(tmp, "bass", MAC, files=files)
        try:
            guest.wait_for("M3 complete")
            guest.wait_for("fos> ")
            for name in files:
                take = "bass-" + name.split(".")[0]
                start_service(guest, f"run record device=loop0 {take} 4 &0")
                guest.pump(2)
                guest.send(f"run fosplay device=loop0 gfx=off bass=9 play {name} &0")
                deadline = time.monotonic() + 60
                while time.monotonic() < deadline:
                    if not listed(guest, "record", "fosplay"):   # the player outlives the 4 s take; the next one needs the device
                        break
            # The last player may still be running (the shell answers late meanwhile): let it end before the next one starts.
            deadline = time.monotonic() + 60
            while time.monotonic() < deadline and listed(guest, "fosplay", "record"):
                guest.pump(1)
            # loop0 has two channels: the upmix must say so and play stereo, not fail.
            mark = guest.send("run fosplay device=loop0 gfx=off upmix=on play b1k.flac &0")
            guest.wait_for("upmix needs 3 playback channels; the device has 2", 60, since=mark)
        finally:
            guest.stop()
        out = fosfs_files(Path(tmp) / "bass.img")
        level = {}
        for name in files:
            blob = out.get("bass-" + name.split(".")[0] + ".wav")
            check(blob is not None, f"{name}: the bass take was written", str(sorted(out)))
            samples, got = float_wav(blob)
            middle = samples[len(samples) // 2 - got // 2:len(samples) // 2 + got // 2]
            level[name] = (sum(x * x for x in middle) / len(middle)) ** 0.5
        ratio = level["b100.flac"] / level["b1k.flac"]
        check(2.2 < ratio < 3.4, f"bass=9 makes 100 Hz {20 * __import__('math').log10(ratio):.1f} dB above 1 kHz (wanted 9)", str(level))


def screenshot(monitor_path, tmp, name):
    shot = Path(tmp) / name
    monitor(str(monitor_path), f"screendump {shot}")
    for _ in range(50):
        if shot.exists() and shot.stat().st_size > 1000:
            break
        time.sleep(0.1)
    time.sleep(0.3)
    return read_ppm(shot)


def browse_test():
    """`fosplay browse` on FOSFS (QEMU has no touch input, so the keys stand in for fingers): the list shows the audio files
    only, j moves the chosen row (amber), Enter plays the folder from that file, q ends the track and brings the list back
    (the chosen row kept), q again leaves the tape."""
    amber = (255, 179, 71)
    with tempfile.TemporaryDirectory() as tmp:
        files = {"notes.txt": b"not audio\n"}
        for name in ("a1.flac", "a2.flac"):
            path = Path(tmp) / name
            subprocess.run(["ffmpeg", "-v", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=500:sample_rate=48000:duration=60", "-ac", "2",
                            "-sample_fmt", "s16", str(path)], check=True)
            files[name] = path.read_bytes()
        monitor_path = Path(tmp) / "monitor"
        guest = Guest(tmp, "browse", MAC, extra=("-monitor", f"unix:{monitor_path},server,nowait"), files=files)
        try:
            guest.wait_for("M3 complete")
            guest.wait_for("fos> ")
            mark = guest.send("run fosplay device=loop0 browse")
            guest.pump(3)

            def count():
                width, height, pixel = screenshot(monitor_path, tmp, "browse.ppm")
                return sum(1 for y in range(0, height, 2) for x in range(0, width, 2) if pixel(x, y) == amber)

            def key(byte):
                guest.process.stdin.write(byte)
                guest.process.stdin.flush()
                guest.pump(1)

            check(count() < 20, "the list opens with nothing chosen", "")
            for _ in range(3):
                key(b"j")
            chosen = count()
            check(chosen > 300, f"j chooses a row, and stops at the last audio file ({chosen} amber samples)", "")
            key(b"\r")
            guest.wait_for("[2/2] a2.flac", 30, since=mark)
            green = (60, 207, 106)
            _, _, pixel = screenshot(monitor_path, tmp, "browse.ppm")
            check(any(pixel(x, 360) == green for x in range(800, 1200, 4)), "the player page shows the level meter while playing", "")
            key(b"q")
            guest.pump(1)
            width, height, pixel = screenshot(monitor_path, tmp, "browse.ppm")
            meter = sum(1 for y in range(0, height, 4) for x in range(0, width, 4) if pixel(x, y) == green)
            check(meter == 0 and count() > 300, "q at a track returns to the list, the chosen row kept", "")
            check("Tape returned" not in guest.text()[mark:], "the tape is still running in the list", "")
            key(b"s")
            width, height, pixel = screenshot(monitor_path, tmp, "settings.ppm")
            check(pixel(700, 540) == amber, "s opens the settings page (the Centre switch is lit)", str(pixel(700, 540)))
        finally:
            guest.stop()


def config_test():
    """fosplay.cfg is read at start: `upmix=on` from the file (not the command line) reaches the player, which on the two-channel
    loop0 says it needs three channels for the saved layout."""
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / "c.flac"
        subprocess.run(["ffmpeg", "-v", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=500:sample_rate=48000:duration=5", "-ac", "2",
                        "-sample_fmt", "s16", str(path)], check=True)
        files = {"c.flac": path.read_bytes(), "fosplay.cfg": b"# saved\nupmix=on\nlayout=4.0\nvolume=0.5\n"}
        guest = Guest(tmp, "config", MAC, files=files)
        try:
            guest.wait_for("M3 complete")
            guest.wait_for("fos> ")
            mark = guest.send("run fosplay device=loop0 gfx=off play c.flac &0")
            guest.wait_for("upmix needs 3 playback channels; the device has 2", 30, since=mark)
            check(True, "fosplay.cfg settings are applied", "")
        finally:
            guest.stop()


def ui_test():
    """The touch screen (QEMU cannot send touch events, so the picture and the keys are what is checked): while a two-file playlist
    plays the screen holds the amber highlight of the current list row; space pauses and the Play button turns amber too; q hands
    the screen back to the console."""
    amber = (255, 179, 71)
    with tempfile.TemporaryDirectory() as tmp:
        files = {}
        for name in ("u1.flac", "u2.flac"):
            path = Path(tmp) / name
            subprocess.run(["ffmpeg", "-v", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=500:sample_rate=48000:duration=60", "-ac", "2",
                            "-sample_fmt", "s16", "-metadata", f"title=Screen {name[1]}", str(path)], check=True)
            files[name] = path.read_bytes()
        monitor_path = Path(tmp) / "monitor"
        guest = Guest(tmp, "ui", MAC, extra=("-monitor", f"unix:{monitor_path},server,nowait"), files=files)
        try:
            guest.wait_for("M3 complete")
            guest.wait_for("fos> ")
            mark = guest.send("run fosplay device=loop0 play u1.flac u2.flac")
            guest.wait_for("[1/2] u1.flac", 30, since=mark)
            guest.pump(2)
            width, height, pixel = screenshot(monitor_path, tmp, "playing.ppm")

            def count(pixel):
                return sum(1 for y in range(0, height, 2) for x in range(0, width, 2) if pixel(x, y) == amber)

            playing = count(pixel)
            check(pixel(width - 2, height - 2) == (16, 16, 16), "the picture has the panel's dark background", str(pixel(width - 2, height - 2)))
            check(playing > 300, f"the current playlist row is highlighted ({playing} amber samples)", "")
            guest.process.stdin.write(b" ")
            guest.process.stdin.flush()
            guest.pump(2)
            _, _, pixel = screenshot(monitor_path, tmp, "paused.ppm")
            paused = count(pixel)
            check(paused > playing + 300, f"pausing lights the Play button ({playing} -> {paused} amber samples)", "")
            guest.process.stdin.write(b"q")
            guest.process.stdin.flush()
            guest.wait_for("Tape returned", 30, since=mark)
            guest.pump(1)
            _, _, pixel = screenshot(monitor_path, tmp, "after.ppm")
            check(count(pixel) < 20, "the screen is the console's again after q", "")
        finally:
            guest.stop()


if __name__ == "__main__":
    main()
