#!/usr/bin/env python3
"""`audio-net serve`: Ogg Opus over HTTP from a v2 guest (QEMU user-mode networking, port 8000 forwarded).

The guest takes a DHCP lease from slirp (10.0.2.15) and serves a 440 Hz test tone. The host fetches
`/live.opus` for a few seconds and checks the bytes as an Ogg stream (every page's CRC, the OpusHead and OpusTags
packets, a continuing granule position), that `ffprobe` and `ffmpeg` accept and decode it to a 440 Hz tone, then that `/`
is the player page, an unknown path is a 404, and a second client is served once the first has gone. Run with
`make test-v2-audio-serve`."""

import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time
import wave
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_audio_net import Guest  # noqa: E402
from test_boot import CODE_CANDIDATES, IMAGE, VARS_CANDIDATES, check, find  # noqa: E402

HOST_PORT = 18000


def crc32_ogg(data):
    crc = 0
    for byte in data:
        crc ^= byte << 24
        for _ in range(8):
            crc = ((crc << 1) ^ 0x04C11DB7) & 0xFFFFFFFF if crc & 0x80000000 else (crc << 1) & 0xFFFFFFFF
    return crc


def pages(data):
    """Splits an Ogg byte stream into (granule, serial, sequence, packet, crc_ok); stops at a truncated page."""
    out, at = [], 0
    while at + 27 <= len(data):
        if data[at:at + 4] != b"OggS":
            raise SystemExit(f"FAIL: no OggS at byte {at}")
        count = data[at + 26]
        if at + 27 + count > len(data):
            break
        size = sum(data[at + 27:at + 27 + count])
        end = at + 27 + count + size
        if end > len(data):
            break
        page = bytearray(data[at:end])
        stored = struct.unpack_from("<I", page, 22)[0]
        page[22:26] = bytes(4)
        granule, serial, sequence = struct.unpack_from("<QII", page, 6)
        out.append((granule, serial, sequence, bytes(page[27 + count:]), crc32_ogg(page) == stored))
        at = end
    return out


def fetch(path, seconds=None, port=HOST_PORT):
    """GET `path`; returns (header, body) after reading for `seconds` (or until the server closes the connection)."""
    sock = socket.create_connection(("127.0.0.1", port), timeout=10)
    sock.sendall(f"GET {path} HTTP/1.1\r\nHost: fos\r\n\r\n".encode())
    data, end = b"", time.monotonic() + (seconds or 30)
    sock.settimeout(1.0)
    while time.monotonic() < end:
        try:
            chunk = sock.recv(65536)
        except socket.timeout:
            continue
        if not chunk:
            break
        data += chunk
    sock.close()
    header, _, body = data.partition(b"\r\n\r\n")
    return header.decode(errors="replace"), body


def boot(tmp, name="serve"):
    """A guest on QEMU user-mode networking (slirp DHCP: 10.0.2.15), host ports 18000 and 18080 forwarded to 8000 and 8080."""
    if True:
        guest = Guest(tmp, name, "52:54:00:12:34:d1")
        guest.stop()
        code, variables = find("FOS_OVMF_CODE", CODE_CANDIDATES), find("FOS_OVMF_VARS", VARS_CANDIDATES)
        vars_copy, disk = Path(tmp) / "serve-vars2.fd", Path(tmp) / "serve2.img"
        shutil.copy(variables, vars_copy)
        shutil.copy(IMAGE, disk)
        command = ["qemu-system-x86_64", "-machine", "q35", "-m", "512", "-display", "none", "-serial", "stdio", "-no-reboot",
                   "-drive", f"if=pflash,format=raw,readonly=on,file={code}", "-drive", f"if=pflash,format=raw,file={vars_copy}",
                   "-drive", f"format=raw,file={disk}",
                   "-netdev", f"user,id=net0,hostfwd=tcp:127.0.0.1:{HOST_PORT}-:8000,hostfwd=tcp:127.0.0.1:{HOST_PORT + 80}-:8080", "-device", "e1000,netdev=net0,mac=52:54:00:12:34:d1"]
        guest.process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        guest.raw = bytearray()
        guest.wait_for("M3 complete")
        guest.wait_for("fos> ")
        return guest


def main():
    with tempfile.TemporaryDirectory() as tmp:
        guest = boot(tmp)
        try:
            mark = guest.send("run audio-net serve ip=dhcp src=tone=440 secs=120")
            guest.wait_for("Serving Ogg Opus", 60, mark)
            guest.pump(1.0)

            header, body = fetch("/live.opus", 4.0)
            check(header.startswith("HTTP/1.1 200") and "audio/ogg" in header, "GET /live.opus is 200 audio/ogg", header)
            found = pages(body)
            check(len(found) >= 100, f"about four seconds of 20 ms pages arrive ({len(found)} pages)", guest.text()[-800:])
            check(all(page[4] for page in found), "every Ogg page has a valid CRC", "")
            check(found[0][3][:8] == b"OpusHead" and found[0][3][9] == 2, "the first page is OpusHead (2 channels)", repr(found[0][3][:19]))
            check(found[1][3][:8] == b"OpusTags", "the second page is OpusTags", "")
            check(all(found[i][2] == i for i in range(len(found))), "page sequence numbers count up without a gap", "")
            granules = [page[0] for page in found[2:]]
            check(all(b - a == 960 for a, b in zip(granules, granules[1:])) and granules[0] == 312 + 960, "granule positions advance by 960 samples (20 ms)", str(granules[:4]))
            ogg_path = Path(tmp) / "got.opus"
            ogg_path.write_bytes(body[:body.rfind(b"OggS")])  # whole pages only
            if shutil.which("ffprobe"):
                probe = subprocess.run(["ffprobe", "-v", "error", "-show_entries", "stream=codec_name,channels,sample_rate", "-of", "default=nw=1", str(ogg_path)], capture_output=True, text=True)
                check("codec_name=opus" in probe.stdout and "channels=2" in probe.stdout, "ffprobe sees a stereo Opus stream", probe.stdout + probe.stderr)
                wav_path = Path(tmp) / "got.wav"
                subprocess.run(["ffmpeg", "-v", "error", "-y", "-i", str(ogg_path), "-ac", "1", "-ar", "48000", str(wav_path)], check=True)
                with wave.open(str(wav_path)) as wav:
                    frames = wav.readframes(wav.getnframes())
                samples = struct.unpack(f"<{len(frames) // 2}h", frames)[4800:]
                crossings = sum(1 for a, b in zip(samples, samples[1:]) if a < 0 <= b)
                hertz = crossings * 48000 / len(samples)
                check(abs(hertz - 440) < 8 and max(samples) > 2000, f"the stream decodes to a 440 Hz tone ({hertz:.0f} Hz)", "")
            if shutil.which("ogginfo"):
                info = subprocess.run(["ogginfo", str(ogg_path)], capture_output=True, text=True)
                check("Opus stream" in info.stdout or "opus" in info.stdout.lower(), "ogginfo reads it", info.stdout[-400:] + info.stderr[-400:])

            header, body = fetch("/")
            check(header.startswith("HTTP/1.1 200") and b"<audio" in body, "GET / is the player page", header)
            header, body = fetch("/nope")
            check(header.startswith("HTTP/1.1 404"), "an unknown path is a 404", header)
            header, body = fetch("/live.opus", 2.0)
            check(header.startswith("HTTP/1.1 200") and len(pages(body)) > 20, "a second client is served after the first", header)
        finally:
            guest.stop()
    panel_test()
    print("v2 audio-net serve: all checks passed")


def panel_test():
    """The settings panel offers Serve: the mode row reaches it, the rows that do not apply disappear, the HTTP port row
    shows 8000 by default and S serves there (the live view names the URL); with 8080 typed the stream moves there, and the
    saved audio.cfg says `mode serve` and `http 8080`."""
    with tempfile.TemporaryDirectory() as tmp:
        guest = boot(tmp, "serve-panel")
        keys = lambda text: (guest.process.stdin.write(text.encode()), guest.process.stdin.flush(), guest.pump(0.5))
        try:
            mark = guest.send("run audio-net")
            guest.wait_for("audio-net: settings", 30, mark)
            keys("lll")
            form = guest.text()[mark:]
            check("Serve (Ogg Opus over HTTP" in form.split("Mode")[-1][:200], "the Mode row reaches Serve", form[-600:])
            check("8000  (the default" in form, "the HTTP port row shows the default 8000", form[-900:])
            keys("jjjjj")
            check("> HTTP port" in guest.text()[-400:], "moving down lands on the HTTP port row (the rows that do not apply are skipped)", guest.text()[-600:])
            keys("s")
            guest.wait_for("http://10.0.2.15:8000/live.opus", 40, mark)
            header, body = fetch("/live.opus", 3.0)
            found = pages(body)
            check(header.startswith("HTTP/1.1 200") and len(found) > 50 and all(page[4] for page in found), f"S serves Ogg Opus on the default port ({len(found)} pages)", header)
            check("Tx " in guest.text()[mark:] and "=====" in guest.text()[mark:], "the live view shows the counter and meters", guest.text()[-500:])
            guest.process.stdin.write(b"\x1b")
            guest.process.stdin.flush()
            guest.wait_for("Press any key for the settings", 20, mark)
            keys(" ")
            keys("\r8080\r")
            check("8080" in guest.text()[-600:], "a port can be typed", guest.text()[-600:])
            keys("s")
            guest.wait_for("http://10.0.2.15:8080/live.opus", 40, mark)
            header, body = fetch("/live.opus", 3.0, HOST_PORT + 80)
            check(header.startswith("HTTP/1.1 200") and len(pages(body)) > 50, "and the stream is there", header)
            guest.process.stdin.write(b"\x1b")
            guest.process.stdin.flush()
            guest.wait_for("Press any key for the settings", 20, mark)
            keys(" ")
            keys("w")
            check("Saved audio.cfg" in guest.text(), "W saves audio.cfg", guest.text()[-400:])
            keys("q")
            guest.wait_for("fos> ", 30, mark)
            cat = guest.send("cat audio.cfg")
            guest.pump(1.5)
            config = guest.text()[cat:]
            check("mode serve" in config and "http 8080" in config and "proto rtp" in config and "codec opus" in config and "\nto " not in config, "the saved cfg holds mode serve and the port", config)
        finally:
            guest.stop()


if __name__ == "__main__":
    main()
