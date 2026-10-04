#!/usr/bin/env python3
"""`audio-net` between two v2 guests on one virtual segment, over raw Ethernet and over RTP/UDP.

Guest B (the receiver) has a `usb-audio` device whose output QEMU records to a WAV file; guest A (the
sender) needs no audio hardware and sends a test tone (`src=tone=440`). Over raw Ethernet the tone must
arrive, be played and be recorded at the right frequency and length, the receiver must report no lost
packets, and a stream with another channel count/bit depth (mono, 16-bit) must be converted and played
too. Run with `make test-v2-audio-net`."""

import os
import select
import socket
import threading
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


def fosfs_add(image, files):
    """Adds `files` ({name: text}) to the first FOSFS volume of a disk image (a first-fit append after the last extent)."""
    data = bytearray(Path(image).read_bytes())
    entries_lba = struct.unpack_from("<Q", data, 512 + 72)[0]
    count, size = struct.unpack_from("<II", data, 512 + 80)
    for index in range(count):
        entry = data[entries_lba * 512 + index * size:entries_lba * 512 + (index + 1) * size]
        first = struct.unpack_from("<Q", entry, 32)[0]
        if entry[:16] != bytes(16) and data[first * 512:first * 512 + 8] == b"FOSFS3\0\0":
            break
    else:
        raise SystemExit("FAIL: no FOSFS volume in the image")
    for name, text in files.items():
        body = text.encode()
        end, slot = 9, None
        for sector in range(1, 9):
            for number in range(8):
                at = (first + sector) * 512 + number * 64
                if data[at]:
                    start, _, sectors = struct.unpack_from("<QQQ", data, at + 4)
                    end = max(end, start + sectors)
                elif slot is None:
                    slot = at
        sectors = -(-len(body) // 512)
        data[(first + end) * 512:(first + end) * 512 + len(body)] = body
        data[slot:slot + 64] = bytes(64)
        data[slot] = 1
        data[slot + 1] = len(name)
        struct.pack_into("<QQQ", data, slot + 4, end, len(body), sectors)
        data[slot + 28:slot + 28 + len(name)] = name.encode()
    Path(image).write_bytes(data)


class Guest:
    def __init__(self, tmp, name, mac, extra=(), files=None):
        code, variables = find("FOS_OVMF_CODE", CODE_CANDIDATES), find("FOS_OVMF_VARS", VARS_CANDIDATES)
        vars_copy = Path(tmp) / f"{name}-vars.fd"
        shutil.copy(variables, vars_copy)
        disk = Path(tmp) / f"{name}.img"
        shutil.copy(IMAGE, disk)
        if files:
            fosfs_add(disk, files)
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


def checksum(data):
    if len(data) % 2:
        data += b"\0"
    total = sum(struct.unpack(f">{len(data) // 2}H", data))
    while total >> 16:
        total = (total & 0xFFFF) + (total >> 16)
    return (~total) & 0xFFFF


def sniff(seconds, frames):
    """Collects the Ethernet frames the QEMU multicast netdev carries for `seconds` into `frames`."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM, socket.IPPROTO_UDP)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    sock.bind(("", PORT))
    sock.setsockopt(socket.IPPROTO_IP, socket.IP_ADD_MEMBERSHIP, socket.inet_aton(GROUP) + socket.inet_aton("0.0.0.0"))
    sock.settimeout(0.2)
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        try:
            data, _ = sock.recvfrom(4096)
            frames.append((time.monotonic(), data))
        except socket.timeout:
            pass
    sock.close()


def sniff_until(stop, frames):
    """Like `sniff`, but runs until the event `stop` is set."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM, socket.IPPROTO_UDP)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    sock.bind(("", PORT))
    sock.setsockopt(socket.IPPROTO_IP, socket.IP_ADD_MEMBERSHIP, socket.inet_aton(GROUP) + socket.inet_aton("0.0.0.0"))
    sock.settimeout(0.2)
    while not stop.is_set():
        try:
            data, _ = sock.recvfrom(4096)
            frames.append((time.monotonic(), data))
        except socket.timeout:
            pass
    sock.close()


def igmp_query_frame():
    """A general IGMPv2 membership query (maximum response 1 s) from a made-up querier, as a raw Ethernet frame."""
    igmp = bytes([0x11, 10, 0, 0, 0, 0, 0, 0])
    igmp = igmp[:2] + struct.pack(">H", checksum(igmp)) + igmp[4:]
    ip = bytes([0x46, 0xC0]) + struct.pack(">H", 24 + 8) + bytes([0, 0, 0x40, 0, 1, 2, 0, 0, 192, 168, 50, 254, 224, 0, 0, 1, 0x94, 4, 0, 0])
    ip = ip[:10] + struct.pack(">H", checksum(ip)) + ip[12:]
    return bytes.fromhex("01005e000001") + bytes.fromhex("525400123400") + b"\x08\x00" + ip + igmp


def igmp_messages(frames, mac):
    """(time, type, group) of the IGMP messages sent by `mac` among the captured frames."""
    found = []
    for stamp, frame in frames:
        if len(frame) >= 14 + 24 + 8 and frame[6:12] == mac and frame[12:14] == b"\x08\x00" and frame[23] == 2:
            igmp = frame[14 + 24:]
            found.append((stamp, igmp[0], ".".join(str(b) for b in (igmp[4:8] if igmp[0] != 0x22 else igmp[12:16]))))
    return found


def check_rtp(frames):
    """Parses the captured frames as RTP over UDP over IPv4 and checks what AES67 expects of the stream."""
    packets = []
    for stamp, frame in frames:
        if len(frame) < 14 + 20 + 8 + 12 or frame[12:14] != b"\x08\x00" or frame[14 + 9] != 17:
            continue
        ip = frame[14:34]
        if ip[0] != 0x45 or checksum(ip) != 0:
            raise SystemExit("FAIL: an IPv4 header checksum is wrong")
        udp = frame[34:]
        length = struct.unpack(">H", udp[4:6])[0]
        pseudo = ip[12:20] + bytes([0, 17]) + struct.pack(">H", length)
        if struct.unpack(">H", udp[6:8])[0] != 0 and checksum(pseudo + udp[:length]) != 0:
            raise SystemExit("FAIL: a UDP checksum is wrong")
        rtp = udp[8:length]
        if rtp[0] >> 6 != 2:
            continue
        packets.append((stamp, rtp[1] & 0x7F, struct.unpack(">H", rtp[2:4])[0], struct.unpack(">I", rtp[4:8])[0], struct.unpack(">I", rtp[8:12])[0], len(rtp) - 12))
    return packets


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
            for run in ("a", "b", "c", "d", "e", "f", "g"):
                if run == "a":
                    # Stereo 24-bit, 440 Hz, 3 seconds, to everyone.
                    mark = receiver.send("run audio-net recv")
                    receiver.wait_for("Listening on NIC 1", 30, mark)
                    receiver.pump(3.0)  # QEMU's emulated USB audio keeps the receiver busy for a while after it starts
                    mark = sender.send("run audio-net send src=tone=440 secs=3")
                    sender.wait_for("Sent ", 60, mark)
                elif run == "c":
                    # RTP over UDP: stereo 24-bit 660 Hz between two static addresses (ARP resolves the peer).
                    mark = receiver.send("run audio-net recv proto=rtp ip=192.168.50.2/24 chan=2 bits=24")
                    receiver.wait_for("Listening on NIC 1 for RTP/UDP", 30, mark)
                    receiver.pump(3.0)
                    captured = []
                    thread = threading.Thread(target=sniff, args=(4.0, captured))
                    thread.start()
                    mark = sender.send("run audio-net send proto=rtp ip=192.168.50.1/24 to=192.168.50.2 src=tone=660 secs=3")
                    sender.wait_for("Sent ", 60, mark)
                    thread.join()
                    sdp = sender.text()[mark:]
                    check("a=rtpmap:96 L24/48000/2" in sdp and "a=ptime:1" in sdp and "m=audio 5004 RTP/AVP 96" in sdp, "the sender prints an AES67-style SDP (L24/48000/2, ptime 1)", sdp[:600])
                elif run == "d":
                    # Opus over RTP: stereo 770 Hz, 20 ms frames, 96 kbit/s.
                    mark = receiver.send("run audio-net recv proto=rtp codec=opus ip=192.168.50.2/24 chan=2")
                    receiver.wait_for("Listening on NIC 1 for Opus/RTP", 30, mark)
                    receiver.pump(3.0)
                    captured = []
                    thread = threading.Thread(target=sniff, args=(4.0, captured))
                    thread.start()
                    mark = sender.send("run audio-net send proto=rtp codec=opus bitrate=96 ip=192.168.50.1/24 to=192.168.50.2 src=tone=770 secs=3")
                    sender.wait_for("Sent ", 60, mark)
                    thread.join()
                    sdp = sender.text()[mark:]
                    check("a=rtpmap:96 opus/48000/2" in sdp and "a=ptime:20" in sdp, "the Opus sender prints an SDP with opus/48000/2 and ptime 20", sdp[:600])
                    rtp = check_rtp(captured)
                    check(len(rtp) >= 140, f"the sniffer saw {len(rtp)} Opus packets (about 150 expected)", "")
                    check(all(p[5] < 400 for p in rtp), "each far smaller than the 3840 bytes of the PCM it carries", str(max(p[5] for p in rtp)))
                    stamps = {(b[3] - a[3]) & 0xFFFFFFFF for a, b in zip(rtp, rtp[1:])}
                    check(stamps == {960}, "timestamps step by 960 (20 ms at 48 kHz)", str(stamps))
                elif run == "e":
                    # Three channels as two Opus streams (a pair and a mono one) to a multicast group: ports 5004 and 5006.
                    captured = []
                    thread = threading.Thread(target=sniff, args=(3.0, captured))
                    thread.start()
                    mark = sender.send("run audio-net send proto=rtp codec=opus ch=1-3 frame=10 ip=192.168.50.1/24 to=239.1.2.3 src=tone=500 secs=2")
                    sender.wait_for("Sent ", 60, mark)
                    thread.join()
                    text = sender.text()[mark:]
                    check("2 Opus stream(s)" in text and "m=audio 5006 RTP/AVP 96" in text, "three channels become two streams, the second on port 5006", text[:700])
                    rtp = check_rtp(captured)
                    streams = {p[4] for p in rtp}
                    check(len(streams) == 2, f"two SSRCs on the wire ({len(rtp)} packets)", "")
                    by_stream = {s: [p for p in rtp if p[4] == s] for s in streams}
                    stamp_sets = [{p[3] for p in v} for v in by_stream.values()]
                    check(stamp_sets[0] == stamp_sets[1], "both streams carry the same timestamps, so a receiver can line them up", "")
                    continue
                elif run == "f":
                    # Multicast with IGMP: the receiver joins 239.1.2.3, answers a query, and leaves at the end.
                    mark = receiver.send("run audio-net recv proto=rtp codec=opus ip=192.168.50.2/24 chan=2 group=239.1.2.3")
                    receiver.wait_for("Joined multicast group 239.1.2.3", 30, mark)
                    receiver.wait_for("Listening on NIC 1 for Opus/RTP", 30, mark)
                    receiver.pump(3.0)
                    captured, stop = [], threading.Event()
                    thread = threading.Thread(target=sniff_until, args=(stop, captured))
                    thread.start()
                    time.sleep(0.5)
                    query_sent = time.monotonic()
                    inject = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
                    inject.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_TTL, 1)
                    inject.sendto(igmp_query_frame(), (GROUP, PORT))
                    inject.close()
                    receiver.pump(2.5)
                    answers = [m for m in igmp_messages(captured, bytes.fromhex("5254001234b2")) if m[1] == 0x16 and m[0] >= query_sent]
                    check(len(answers) >= 1 and answers[0][2] == "239.1.2.3", f"the receiver answers an IGMP query with a report for 239.1.2.3 ({len(answers)} seen)", "")
                    mark = sender.send("run audio-net send proto=rtp codec=opus ip=192.168.50.1/24 to=239.1.2.3 src=tone=550 secs=3")
                    sender.wait_for("Sent ", 60, mark)
                    receiver.pump(1.0)
                    mark = len(receiver.text())
                    receiver.process.stdin.write(b"\x1b")
                    receiver.process.stdin.flush()
                    receiver.wait_for("Received ", 30, mark - 200)
                    receiver.wait_for("fos> ", 30, mark)
                    stop.set()
                    thread.join()
                    report = receiver.text()[mark - 200:]
                    leaves = [m for m in igmp_messages(captured, bytes.fromhex("5254001234b2")) if m[1] == 0x17]
                    check(len(leaves) == 1 and leaves[0][2] == "239.1.2.3", "it sends an IGMP leave for the group when it stops", str(leaves))
                    packets = int(report.split("Received ")[-1].split(" ")[0])
                    check(packets >= 140, f"it received {packets} Opus packets from the multicast group", report)
                    continue
                elif run == "g":
                    # AVTP (IEEE 1722 AAF): stereo 24-bit 600 Hz to the default stream address.
                    mark = receiver.send("run audio-net recv proto=avtp")
                    receiver.wait_for("Listening on NIC 1 for AVTP", 30, mark)
                    receiver.pump(3.0)
                    captured = []
                    thread = threading.Thread(target=sniff, args=(4.0, captured))
                    thread.start()
                    mark = sender.send("run audio-net send proto=avtp src=tone=600 secs=3")
                    sender.wait_for("Sent ", 60, mark)
                    thread.join()
                    frames = [f for _, f in captured if f[12:14] == b"\x22\xf0"]
                    check(len(frames) >= 2800, f"the sniffer saw {len(frames)} AVTP frames", "")
                    first = frames[0]
                    check(first[0:6] == bytes.fromhex("91e0f000fe01"), "sent to the stream address 91:e0:f0:00:fe:01", first[:6].hex())
                    h = first[14:]
                    check(h[0] == 0x02 and h[1] >> 7 == 1 and (h[1] >> 4) & 7 == 0, "AAF subtype, sv set, version 0", h[:2].hex())
                    check(h[16] == 3 and h[19] == 24 and h[17] >> 4 == 5 and ((h[17] & 3) << 8 | h[18]) == 2, "format INT24, 24 bits, 48 kHz (nsr 5), 2 channels", h[16:20].hex())
                    check(int.from_bytes(h[20:22], "big") == 288 and len(h) - 24 >= 288, "stream_data_length 288 bytes (48 frames of stereo L24)", "")
                    check(h[4:10] == first[6:12], "stream id starts with the sender's MAC", h[4:12].hex())
                    steps = {(b[14 + 2] - a[14 + 2]) & 0xFF for a, b in zip(frames, frames[1:])}
                    check(steps == {1}, "the 8-bit sequence number steps by 1 (wrapping)", str(steps))
                else:
                    # Mono 16-bit, 880 Hz, to the receiver's MAC, after the receiver has been restarted.
                    mark = receiver.send("run audio-net recv from=52:54:00:12:34:b1 target=8")
                    receiver.wait_for("Listening on NIC 1", 30, mark)
                    receiver.pump(3.0)
                    mark = sender.send("run audio-net send src=tone=880 ch=1 bits=16 to=52:54:00:12:34:b2 secs=3")
                    sender.wait_for("Sent ", 60, mark)
                if run == "e":
                    continue
                receiver.pump(1.0)
                mark = len(receiver.text())
                receiver.process.stdin.write(b"\x1b")  # Escape: stop the tape
                receiver.process.stdin.flush()
                receiver.wait_for("Received ", 30, mark - 200)
                receiver.wait_for("fos> ", 30, mark)
                report = receiver.text()[mark - 200:]
                locked = report.split("Locked onto a stream:")[-1].split("\n")[0]
                if run == "g":
                    check("2 channel(s), 24-bit, 48000 Hz" in locked, "the receiver locks onto the AVTP stream", locked)
                elif run == "d":
                    check("Opus packets" in report, "the receiver decodes the Opus stream", report)
                elif run == "c":
                    check("2 channel(s), 24-bit, 48000 Hz, from 192.168.50.1" in locked, "the receiver locks onto the RTP stream from the sender's IP address", locked)
                elif run == "a":
                    check("2 channel(s), 24-bit, 48000 Hz" in locked, "the receiver locks onto a stereo 24-bit 48 kHz stream", locked)
                else:
                    check("1 channel(s), 16-bit, 48000 Hz" in locked, "and onto a mono 16-bit one, from a chosen sender", locked)
                packets = int(report.split("Received ")[-1].split(" ")[0])
                lost = int(report.split(" packets: ")[-1].split(" lost")[0]) if run != "d" else int(report.split("packets: ")[-1].split(" lost")[0])
                check(packets >= (140 if run == "d" else 2900), f"it received {packets} packets", report)
                check(lost <= 5, f"with {lost} lost", report)
                if run == "c":
                    rtp = check_rtp(captured)
                    check(len(rtp) >= 2800, f"the sniffer saw {len(rtp)} well-formed RTP/UDP/IPv4 packets (IP and UDP checksums valid)", "")
                    check({p[1] for p in rtp} == {96} and len({p[4] for p in rtp}) == 1, "all payload type 96 with one SSRC", "")
                    check(all(p[5] == 288 for p in rtp), "each carrying 288 bytes: 48 frames of stereo L24 (1 ms)", "")
                    steps = {(b[2] - a[2]) & 0xFFFF for a, b in zip(rtp, rtp[1:])}
                    stamps = {(b[3] - a[3]) & 0xFFFFFFFF for a, b in zip(rtp, rtp[1:])}
                    check(steps == {1} and stamps == {48}, "sequence numbers step by 1 and timestamps by 48 frames", f"{steps} {stamps}")
        finally:
            receiver.stop()
            sender.stop()
        with wave.open(str(wav)) as recording:
            rate, channels, count = recording.getframerate(), recording.getnchannels(), recording.getnframes()
            samples = struct.unpack(f"<{count * channels}h", recording.readframes(count))[::channels]
        loud = [i for i, value in enumerate(samples) if abs(value) > 500]
        check(len(loud) > 12 * rate, f"the recording holds sound ({len(loud) / rate:.1f} s of it)", "")
        # Split into the separate tones at silences longer than half a second; measure each one's middle second.
        segments, begin = [], loud[0]
        for before, after in zip(loud, loud[1:]):
            if after - before > rate // 2:
                segments.append((begin, before))
                begin = after
        segments.append((begin, loud[-1]))
        segments = [seg for seg in segments if seg[1] - seg[0] > rate]
        wanted = [("raw, stereo 24-bit", 440, 20), ("raw, mono 16-bit, converted", 880, 30), ("RTP/UDP, stereo 24-bit", 660, 25), ("Opus/RTP", 770, 30), ("Opus/RTP multicast", 550, 30), ("AVTP", 600, 30)]
        check(len(segments) == 6, f"the recording holds {len(segments)} separate tones (wanted 6)", str(segments))
        for (name, frequency, tolerance), (start, end) in zip(wanted, segments):
            middle = (start + end) // 2
            measured = tone_frequency(samples, rate, samples[middle - rate // 2:middle + rate // 2])
            check(abs(measured - frequency) < tolerance, f"the {name} tone is {measured:.0f} Hz (wanted {frequency})", "")
    dhcp_test()
    duplex_test()
    config_test()
    peers_test()
    panel_test()
    duplex_names_test()
    codec_follows_listener_test()
    panel_wait_test()
    print("v2 audio-net: all checks passed")


def duplex_test():
    """`duplex`: each guest sends and plays at once. A sends a 440 Hz tone, B an 880 Hz one (QEMU's USB audio has no capture side); both run the
    send and receive loops as two tasks of one tape. each recording must hold the other's tone."""
    with tempfile.TemporaryDirectory() as tmp:
        wav_a, wav_b = Path(tmp) / "a.wav", Path(tmp) / "b.wav"
        usb = lambda wav: ["-device", "qemu-xhci,id=xhci", "-audiodev", f"wav,id=snd,path={wav}", "-device", "usb-audio,audiodev=snd"]
        a = Guest(tmp, "duplex-a", "52:54:00:12:34:d1", usb(wav_a))
        b = Guest(tmp, "duplex-b", "52:54:00:12:34:d2", usb(wav_b))
        try:
            for guest in (a, b):
                guest.wait_for("M3 complete")
                guest.wait_for("fos> ")
            mark_a = a.send("run audio-net duplex src=tone=440 from=52:54:00:12:34:d2 secs=8 target=10")
            mark_b = b.send("run audio-net duplex src=tone=880 from=52:54:00:12:34:d1 secs=8 target=10")
            for guest, mark in ((a, mark_a), (b, mark_b)):
                guest.wait_for("Received ", 90, mark)
                guest.wait_for("fos> ", 30, mark)
            text_a, text_b = a.text()[mark_a:], b.text()[mark_b:]
            check("Sent " in text_a and "Sent " in text_b, "both guests sent", text_a[-800:] + text_b[-800:])
            for name, text in (("A", text_a), ("B", text_b)):
                packets = int(text.split("Received ")[-1].split(" ")[0])
                check(packets >= 4000, f"{name} received {packets} packets while sending", text[-800:])
        finally:
            a.stop()
            b.stop()
        for name, wav, wanted in (("A", wav_a, 880), ("B", wav_b, 440)):
            with wave.open(str(wav)) as recording:
                rate, channels, count = recording.getframerate(), recording.getnchannels(), recording.getnframes()
                samples = struct.unpack(f"<{count * channels}h", recording.readframes(count))[::channels]
            loud = [i for i, value in enumerate(samples) if abs(value) > 500]
            check(len(loud) > 3 * rate, f"{name}'s recording holds sound ({len(loud) / rate:.1f} s)", "")
            middle = (loud[0] + loud[-1]) // 2
            measured = tone_frequency(samples, rate, samples[middle - rate // 2:middle + rate // 2])
            check(abs(measured - wanted) < 30, f"{name} played the other's tone: {measured:.0f} Hz (wanted {wanted})", "")


def config_test():
    """The configuration file and the boot script. Receiver A starts `audio-net @rx.cfg` from its `start` file at boot
    and sender B `audio-net @tx.cfg` (a file of `key value` lines and comments); A must play B's tone. Then a sender
    whose peer is not there must keep retrying, and Escape must end it."""
    with tempfile.TemporaryDirectory() as tmp:
        wav = Path(tmp) / "cfg.wav"
        rx = "# the receiver\nmode recv\nproto raw   # layer 2, no addresses\ntarget 8\n"
        tx = "mode send\nproto raw\nsrc tone=550\nsecs 3\n"
        a = Guest(tmp, "cfg-a", "52:54:00:12:34:f1", ["-device", "qemu-xhci,id=xhci", "-audiodev", f"wav,id=snd,path={wav}", "-device", "usb-audio,audiodev=snd"],
                  {"rx.cfg": rx, "start": "run audio-net @rx.cfg\n"})
        b = Guest(tmp, "cfg-b", "52:54:00:12:34:f2", files={"tx.cfg": tx})
        try:
            a.wait_for("Listening on NIC 1 for raw Ethernet", 90)
            check("audio-net recv: raw Ethernet PCM on NIC 1" in a.text() and "(MAC 52:54:00:12:34:f1)" in a.text(), "the start file launches the receiver from its cfg and it reports its MAC", a.text()[-900:])
            b.wait_for("fos> ", 90)
            a.pump(3.0)  # the emulated USB audio keeps the receiver busy for a while after it starts
            mark_b = b.send("run audio-net @tx.cfg")
            b.wait_for("Sent ", 120, mark_b)
            a.pump(2.0)
            mark = len(a.text())
            a.process.stdin.write(b"\x1b")
            a.process.stdin.flush()
            a.wait_for("Received ", 30, mark - 300)
            report = a.text()[mark - 300:]
            packets = int(report.split("Received ")[-1].split(" ")[0])
            check(packets >= 2500, f"the cfg-driven receiver got {packets} packets from the cfg-driven sender", report)
            check("trying again" not in a.text(), "and Escape ended it without a retry", "")
            a.wait_for("fos> ", 30, mark)
            mark = b.send("run audio-net send proto=rtp ip=192.168.50.1/24 to=192.168.50.9 src=tone=440")
            end = time.monotonic() + 60
            while b.text()[mark:].count("trying again in 5 s") < 2 and time.monotonic() < end:
                b.pump(0.5)
            text = b.text()[mark:]
            check(text.count("trying again in 5 s") >= 2, "a sender whose peer is not there keeps retrying", text[-800:])
            b.process.stdin.write(b"\x1b")
            b.process.stdin.flush()
            b.wait_for("Tape returned", 30, mark)
            n = b.text()[mark:].count("trying again in 5 s")
            b.pump(8.0)
            check(b.text()[mark:].count("trying again in 5 s") == n, "Escape stops the retrying", "")
        finally:
            a.stop()
            b.stop()


def peers_test():
    """Announcements and the peer list. B sends a multicast Opus stream as 'studio-a' and A, which has never been told
    its parameters, lists it, then receives it with `source=studio-a`. Then A listens as 'hall' and B sends PCM to
    `to=hall`, resolved from A's announcement; when A stops its announcement is withdrawn."""
    with tempfile.TemporaryDirectory() as tmp:
        wav = Path(tmp) / "peers.wav"
        a = Guest(tmp, "peers-a", "52:54:00:12:34:a1", ["-device", "qemu-xhci,id=xhci", "-audiodev", f"wav,id=snd,path={wav}", "-device", "usb-audio,audiodev=snd"])
        b = Guest(tmp, "peers-b", "52:54:00:12:34:a2")
        try:
            for guest in (a, b):
                guest.wait_for("M3 complete")
                guest.wait_for("fos> ")
            mark_b = b.send("run audio-net send proto=rtp codec=opus name=studio-a ip=192.168.50.1/24 to=239.1.2.3 src=tone=500 secs=40")
            b.wait_for("Announcing this stream as 'studio-a'", 60, mark_b)
            mark = a.send("run audio-net list ip=192.168.50.2/24 secs=8")
            a.wait_for("fos> ", 60, mark)
            listing = a.text()[mark:]
            row = [line for line in listing.splitlines() if "studio-a" in line and "sends" in line]
            check(len(row) == 1, "the peer list shows the sender 'studio-a'", listing[-900:])
            check("192.168.50.1 52:54:00:12:34:a2" in row[0] and "Opus" in row[0] and "239.1.2.3:5004" in row[0], "with its IP, MAC, codec and multicast address", row[0])
            mark = a.send("run audio-net recv source=studio-a ip=192.168.50.2/24 secs=10 target=30")
            a.wait_for("Received ", 90, mark)
            a.wait_for("fos> ", 30, mark)
            text = a.text()[mark:]
            check("Found 'studio-a': 192.168.50.1" in text and "Joined multicast group 239.1.2.3" in text, "source=studio-a finds the sender and joins its group", text[-900:])
            packets = int(text.split("Received ")[-1].split(" ")[0])
            check(packets >= 300, f"and receives its Opus stream ({packets} packets) without being told codec, channels or port", text[-600:])
            b.wait_for("Sent ", 60, mark_b)
            # A listens as 'hall'; B finds it by name.
            mark_a = a.send("run audio-net recv proto=rtp ip=192.168.50.2/24 name=hall secs=30 target=30")
            a.wait_for("Listening on NIC 1 for RTP/UDP", 60, mark_a)
            a.pump(3.0)
            mark_b = b.send("run audio-net send proto=rtp ip=192.168.50.1/24 to=hall src=tone=700 secs=3")
            b.wait_for("Sent ", 90, mark_b)
            text_b = b.text()[mark_b:]
            check("Found 'hall': 192.168.50.2" in text_b and "Sending to 192.168.50.2" in text_b, "to=hall finds the listener and sends to its address", text_b[-900:])
            a.pump(1.0)
            mark = len(a.text())
            a.process.stdin.write(b"\x1b")
            a.process.stdin.flush()
            a.wait_for("Received ", 30, mark_a)
            a.wait_for("fos> ", 30, mark)
            packets = int(a.text()[mark_a:].split("Received ")[-1].split(" ")[0])
            check(packets >= 2500, f"the listener received {packets} packets", a.text()[-600:])
            # Its announcement is withdrawn: a new list from B no longer shows 'hall'.
            mark = b.send("run audio-net list ip=192.168.50.1/24 secs=6")
            b.wait_for("fos> ", 60, mark)
            check("hall" not in b.text()[mark:].split("Listening for announcements")[-1], "once the listener stops, its announcement is withdrawn", b.text()[mark:][-700:])
        finally:
            a.stop()
            b.stop()
        with wave.open(str(wav)) as recording:
            rate, channels, count = recording.getframerate(), recording.getnchannels(), recording.getnframes()
            samples = struct.unpack(f"<{count * channels}h", recording.readframes(count))[::channels]
        loud = [i for i, value in enumerate(samples) if abs(value) > 500]
        segments, begin = [], loud[0]
        for before, after in zip(loud, loud[1:]):
            if after - before > rate // 2:
                segments.append((begin, before))
                begin = after
        segments.append((begin, loud[-1]))
        segments = [seg for seg in segments if seg[1] - seg[0] > rate]
        check(len(segments) == 2, f"two tones were played ({len(segments)})", str(segments))
        for wanted, (start, end) in zip((500, 700), segments):
            middle = (start + end) // 2
            measured = tone_frequency(samples, rate, samples[middle - rate // 2:middle + rate // 2])
            check(abs(measured - wanted) < 30, f"a tone of {measured:.0f} Hz was played (wanted {wanted})", "")


def panel_test():
    """The settings panel (`audio-net` with no arguments), driven by keystrokes over the serial line: B edits its IP
    address and peer and starts sending (a test tone: QEMU's USB audio has no input), A switches to Receive, sets its
    address and starts. Both live views must show counters and meters; Escape stops, W saves audio.cfg."""
    keys = lambda guest, text: (guest.process.stdin.write(text.encode()), guest.process.stdin.flush(), guest.pump(0.4))
    with tempfile.TemporaryDirectory() as tmp:
        wav = Path(tmp) / "panel.wav"
        a = Guest(tmp, "panel-a", "52:54:00:12:34:b1", ["-device", "qemu-xhci,id=xhci", "-audiodev", f"wav,id=snd,path={wav}", "-device", "usb-audio,audiodev=snd"])
        b = Guest(tmp, "panel-b", "52:54:00:12:34:b2")
        try:
            for guest in (a, b):
                guest.wait_for("M3 complete")
                guest.wait_for("fos> ")
            mark_a = a.send("run audio-net")
            mark_b = b.send("run audio-net")
            a.wait_for("audio-net: settings", 30, mark_a)
            b.wait_for("audio-net: settings", 30, mark_b)
            # A: mode Receive; IP address.
            keys(a, "l")
            keys(a, "jjjl")  # addressing: static
            check("Static" in a.text()[mark_a:], "the Addressing row toggles between DHCP and static", a.text()[-500:])
            keys(a, "j\r" + "\x7f" * 20 + "192.168.50.2/24\r")
            # B: IP address, then the peer's address.
            keys(b, "jjjj")  # Network, NIC, Addressing, then Peer: the IP and gateway rows do not apply with DHCP
            check("> Peer" in b.text()[-700:], "moving down skips the rows that do not apply (DHCP)", b.text()[-700:])
            keys(b, "k")
            check("> Addressing" in b.text()[-500:], "and moving up skips them the same way", b.text()[-500:])
            keys(b, "l")  # static
            keys(b, "j\r" + "\x7f" * 20 + "192.168.50.1/24\r")
            check("this machine is called fos-1234b2" in b.text()[mark_b:], "the form says what this machine is called (automatic name from the MAC)", b.text()[-600:])
            # The peer is picked by name from the machines announced on the network: A's panel is open (idle) with its address set.
            keys(b, "p")
            end = time.monotonic() + 25
            while "fos-1234b1" not in b.text()[mark_b:].split("machines on the network")[-1] and time.monotonic() < end:
                b.pump(0.5)
            picker = b.text()[mark_b:].split("machines on the network")[-1]
            check("fos-1234b1" in picker and "192.168.50.2" in picker and "idle" in picker, "the peer picker lists the other machine by name, with its address, while it is idle (no stream running)", picker[:600])
            keys(b, "1")
            check("fos-1234b1" in b.text()[-900:], "choosing it fills in the Peer row", b.text()[-900:])
            check("192.168.50.1/24" in b.text()[mark_b:], "the form shows the edited address", b.text()[-600:])
            keys(a, "s")
            a.pump(3.0)
            keys(b, "s")
            a.pump(5.0)
            b.pump(2.0)
            text_a, text_b = a.text()[mark_a:], b.text()[mark_b:]
            check("Tx " in text_b and "Lost 0" in text_a, "both live views show counters", text_a[-500:] + text_b[-500:])
            sent = [int(part.split(" ")[0]) for part in text_b.split("Tx ")[1:] if part.split(" ")[0].isdigit()]
            received = [int(part.split(" ")[0]) for part in text_a.split("Rx ")[1:] if part.split(" ")[0].isdigit()]
            check(sent and max(sent) >= 3000, f"the sender's counter reached {max(sent) if sent else 0}", text_b[-500:])
            check(received and max(received) >= 2000, f"the receiver's counter reached {max(received) if received else 0}", text_a[-500:])
            check("=====" in text_b and "=====" in text_a and "Nordic" in text_a and "+5." in text_b, "and both draw level meters on a Nordic scale (a -12 dBFS tone reads +6)", "")
            # Escape stops the stream and returns to the panel; W saves the configuration.
            for guest in (b, a):
                guest.process.stdin.write(b"\x1b")
                guest.process.stdin.flush()
                guest.pump(1.5)
                check("Press any key for the settings" in guest.text(), f"Escape stops the stream ({guest.name})", guest.text()[-400:])
                keys(guest, " ")
            keys(b, "w")
            check("Saved audio.cfg" in b.text(), "W saves audio.cfg", b.text()[-400:])
            keys(b, "q")
            b.wait_for("fos> ", 30)
            mark = b.send("cat audio.cfg")
            b.pump(1.5)
            config = b.text()[mark:]
            check("mode send" in config and "ip 192.168.50.1/24" in config and "to fos-1234b1" in config and "src tone" not in config, "the saved cfg holds the settings (and not the test tone)", config)
        finally:
            a.stop()
            b.stop()


def duplex_names_test():
    """Two machines in duplex find each other by name although neither has anything to announce until it starts:
    each announces where it listens while it looks, so the other can start sending; the stream announcements follow.
    C starts first and must wait for D."""
    with tempfile.TemporaryDirectory() as tmp:
        wavs = {name: Path(tmp) / f"{name}.wav" for name in "cd"}
        usb = lambda wav: ["-device", "qemu-xhci,id=xhci", "-audiodev", f"wav,id=snd,path={wav}", "-device", "usb-audio,audiodev=snd"]
        c = Guest(tmp, "dn-c", "52:54:00:12:34:c3", usb(wavs["c"]))
        d = Guest(tmp, "dn-d", "52:54:00:12:34:d4", usb(wavs["d"]))
        try:
            for guest in (c, d):
                guest.wait_for("M3 complete")
                guest.wait_for("fos> ")
            mark_c = c.send("run audio-net duplex proto=rtp name=c ip=192.168.50.1/24 to=d source=d src=tone=440 secs=40")
            c.pump(8.0)
            check("Looking for the announcement of the listener 'd'" in c.text()[mark_c:], "the first machine waits for its peer", c.text()[-600:])
            mark_d = d.send("run audio-net duplex proto=rtp name=d ip=192.168.50.2/24 to=c source=c src=tone=880 secs=25")
            d.wait_for("Received ", 120, mark_d)
            d.wait_for("fos> ", 30, mark_d)
            c.wait_for("Received ", 120, mark_c)
            for name, guest, mark in (("c", c, mark_c), ("d", d, mark_d)):
                text = guest.text()[mark:]
                check("Found 'd'" in text if name == "c" else "Found 'c'" in text, f"{name} found its peer by name", text[-700:])
                packets = int(text.split("Received ")[-1].split(" ")[0])
                check(packets >= 2500, f"{name} received {packets} packets while sending", text[-500:])
        finally:
            c.stop()
            d.stop()
        for name, wanted in (("c", 880), ("d", 440)):
            with wave.open(str(wavs[name])) as recording:
                rate, channels, count = recording.getframerate(), recording.getnchannels(), recording.getnframes()
                samples = struct.unpack(f"<{count * channels}h", recording.readframes(count))[::channels]
            loud = [i for i, value in enumerate(samples) if abs(value) > 500]
            check(len(loud) > 3 * rate, f"{name}'s recording holds sound ({len(loud) / rate:.1f} s)", "")
            middle = (loud[0] + loud[-1]) // 2
            measured = tone_frequency(samples, rate, samples[middle - rate // 2:middle + rate // 2])
            check(abs(measured - wanted) < 30, f"{name} played its peer's tone: {measured:.0f} Hz (wanted {wanted})", "")


def codec_follows_listener_test():
    """A sender set to PCM that finds its peer by name sends what that peer announced it listens for (Opus here), so the
    two ends cannot disagree; a sender given a plain address keeps its own setting."""
    with tempfile.TemporaryDirectory() as tmp:
        a = Guest(tmp, "cf-a", "52:54:00:12:34:e1", ["-device", "qemu-xhci,id=xhci", "-audiodev", f"wav,id=snd,path={Path(tmp) / 'cf.wav'}", "-device", "usb-audio,audiodev=snd"])
        b = Guest(tmp, "cf-b", "52:54:00:12:34:e2")
        try:
            for guest in (a, b):
                guest.wait_for("M3 complete")
                guest.wait_for("fos> ")
            mark_a = a.send("run audio-net recv proto=rtp codec=opus name=hall ip=192.168.50.2/24 secs=30")
            a.wait_for("Listening on NIC 1 for Opus/RTP", 60, mark_a)
            a.pump(3.0)
            mark_b = b.send("run audio-net send proto=rtp ip=192.168.50.1/24 to=hall src=tone=500 secs=4")
            b.wait_for("Sent ", 90, mark_b)
            text = b.text()[mark_b:]
            check("Opus stream(s)" in text, "a PCM-configured sender that looks the peer up by name sends Opus, as the peer announced", text[-700:])
            a.pump(2.0)
            check("Receiving Opus from 192.168.50.1" in a.text()[mark_a:], "and the Opus receiver gets it", a.text()[-500:])
        finally:
            a.stop()
            b.stop()


def panel_wait_test():
    """A receiver started from the panel before its sender keeps waiting (it does not give up after one look) and stays
    findable: the sender's panel lists it, picks it, starts, and the receiver then gets the stream."""
    keys = lambda guest, text: (guest.process.stdin.write(text.encode()), guest.process.stdin.flush(), guest.pump(0.4))
    with tempfile.TemporaryDirectory() as tmp:
        a = Guest(tmp, "pw-a", "52:54:00:12:34:b3", ["-device", "qemu-xhci,id=xhci", "-audiodev", f"wav,id=snd,path={Path(tmp) / 'pw.wav'}", "-device", "usb-audio,audiodev=snd"])
        b = Guest(tmp, "pw-b", "52:54:00:12:34:b4")
        try:
            for guest in (a, b):
                guest.wait_for("M3 complete")
                guest.wait_for("fos> ")
            mark_a = a.send("run audio-net")
            mark_b = b.send("run audio-net")
            a.wait_for("audio-net: settings", 30, mark_a)
            b.wait_for("audio-net: settings", 30, mark_b)
            # A: Receive from the sender 'fos-1234b4' (not running yet), static address, start.
            keys(a, "l")
            keys(a, "jjjl")
            keys(a, "j\r" + "\x7f" * 20 + "192.168.50.3/24\r")
            keys(a, "jj\rfos-1234b4\r")
            keys(a, "s")
            a.pump(20.0)  # longer than one 15 s look: it must go on waiting
            check("Looking for the announcement of the sender 'fos-1234b4'" in a.text()[mark_a:], "the receiver waits for its sender", a.text()[-400:])
            # B: Send, static address, pick the waiting receiver from the list, start.
            keys(b, "jjjl")
            keys(b, "j\r" + "\x7f" * 20 + "192.168.50.4/24\r")
            keys(b, "p")
            end = time.monotonic() + 25
            while "fos-1234b3" not in b.text()[mark_b:].split("machines on the network")[-1] and time.monotonic() < end:
                b.pump(0.5)
            picker = b.text()[mark_b:].split("machines on the network")[-1]
            check("fos-1234b3" in picker and "listening" in picker, "the waiting receiver is listed as listening (it announces while it waits)", picker[:500])
            keys(b, "1")
            keys(b, "s")
            a.pump(10.0)
            received = [int(part.split(" ")[0]) for part in a.text()[mark_a:].split("Rx ")[1:] if part.split(" ")[0].isdigit()]
            check(received and max(received) >= 500, f"the receiver then gets the stream ({max(received) if received else 0} packets)", a.text()[-600:])
        finally:
            a.stop()
            b.stop()


def dhcp_test():
    """`ip=dhcp` against QEMU's built-in DHCP server (slirp hands out 10.0.2.15 with gateway 10.0.2.2)."""
    with tempfile.TemporaryDirectory() as tmp:
        guest = Guest(tmp, "dhcp", "52:54:00:12:34:c1")
        # Replace the multicast NIC with a user-mode one (it has a DHCP server).
        guest.stop()
        code, variables = find("FOS_OVMF_CODE", CODE_CANDIDATES), find("FOS_OVMF_VARS", VARS_CANDIDATES)
        vars_copy = Path(tmp) / "dhcp-vars2.fd"
        shutil.copy(variables, vars_copy)
        disk = Path(tmp) / "dhcp2.img"
        shutil.copy(IMAGE, disk)
        command = ["qemu-system-x86_64", "-machine", "q35", "-m", "512", "-display", "none", "-serial", "stdio", "-no-reboot",
                   "-drive", f"if=pflash,format=raw,readonly=on,file={code}", "-drive", f"if=pflash,format=raw,file={vars_copy}",
                   "-drive", f"format=raw,file={disk}", "-netdev", "user,id=net0", "-device", "e1000,netdev=net0,mac=52:54:00:12:34:c2"]
        guest.process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        guest.raw = bytearray()
        try:
            guest.wait_for("M3 complete")
            guest.wait_for("fos> ")
            mark = guest.send("run audio-net send proto=rtp ip=dhcp to=10.0.2.2 src=tone=440 secs=1")
            guest.wait_for("Sent ", 60, mark)
            text = guest.text()[mark:]
            check("Network: 10.0.2.15 on NIC 1 (MAC 52:54:00:12:34:c2)." in text, "ip=dhcp obtains a lease (10.0.2.15) and streams from it", text[:800])
        finally:
            guest.stop()


if __name__ == "__main__":
    main()
