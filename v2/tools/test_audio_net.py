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
            check("Network: 10.0.2.15 on NIC 1." in text, "ip=dhcp obtains a lease (10.0.2.15) and streams from it", text[:800])
        finally:
            guest.stop()


if __name__ == "__main__":
    main()
