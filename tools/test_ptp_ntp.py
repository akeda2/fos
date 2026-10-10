#!/usr/bin/env python3
"""`ptp` master with NTP time, slave with `settime`: the date travels from an NTP server to a guest's clock.

A fake NTP server lives on the virtual segment (10.7.0.99: it answers ARP and UDP port 123, with a time one day ahead of the host's).
The master asks it (`ptp master ntp=10.7.0.99`), becomes a PTP master with real time (TAI, timeSource NTP), and the slave
(`ptp settime tz=+2`) sets its clock and RTC from it, which `date` then shows. A second master without `ntp=` has no real time, and
a `settime` slave must leave its clock alone. Run with `make test-ptp-ntp`."""

import socket
import struct
import sys
import tempfile
import threading
import time
from datetime import datetime, timedelta, timezone
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_audio_net import GROUP, PORT, Guest  # noqa: E402
from test_boot import check  # noqa: E402

MASTER_MAC, SLAVE_MAC = "52:54:00:12:35:01", "52:54:00:12:35:02"
SERVER_MAC, SERVER_IP = bytes.fromhex("525400123599"), bytes([10, 7, 0, 99])
SKEW = 86400  # the fake server's clock is a day ahead of the host's


def checksum(data):
    if len(data) % 2:
        data += b"\0"
    total = sum(struct.unpack(f">{len(data) // 2}H", data))
    while total >> 16:
        total = (total & 0xFFFF) + (total >> 16)
    return ~total & 0xFFFF


def ntp_timestamp(unix):
    return struct.pack(">II", int(unix) + 2208988800, int((unix % 1) * (1 << 32)))


def udp_frame(destination_mac, destination_ip, port, payload):
    udp = struct.pack(">HHHH", 123, port, 8 + len(payload), 0) + payload
    pseudo = SERVER_IP + destination_ip + struct.pack(">BBH", 0, 17, len(udp))
    udp = udp[:6] + struct.pack(">H", checksum(pseudo + udp) or 0xFFFF) + udp[8:]
    ip = struct.pack(">BBHHHBBH", 0x45, 0, 20 + len(udp), 0, 0, 64, 17, 0) + SERVER_IP + destination_ip
    ip = ip[:10] + struct.pack(">H", checksum(ip)) + ip[12:]
    return destination_mac + SERVER_MAC + b"\x08\x00" + ip + udp


def serve(stop, answered):
    """Answers ARP for SERVER_IP and NTP requests to it, forever until `stop`."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM, socket.IPPROTO_UDP)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    sock.bind(("", PORT))
    sock.setsockopt(socket.IPPROTO_IP, socket.IP_ADD_MEMBERSHIP, socket.inet_aton(GROUP) + socket.inet_aton("0.0.0.0"))
    sock.settimeout(0.2)
    out = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    out.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_TTL, 1)
    while not stop.is_set():
        try:
            data, _ = sock.recvfrom(4096)
        except socket.timeout:
            continue
        if len(data) < 42 or data[6:12] == SERVER_MAC:
            continue
        if data[12:14] == b"\x08\x06" and data[20:22] == b"\x00\x01" and data[38:42] == SERVER_IP:
            sender_mac, sender_ip = data[22:28], data[28:32]
            reply = data[6:12] + SERVER_MAC + b"\x08\x06" + struct.pack(">HHBBH", 1, 0x0800, 6, 4, 2) + SERVER_MAC + SERVER_IP + sender_mac + sender_ip
            out.sendto(reply, (GROUP, PORT))
        elif data[12:14] == b"\x08\x00" and data[23] == 17 and data[30:34] == SERVER_IP and data[36:38] == b"\x00\x7b" and len(data) >= 42 + 48:
            now = time.time() + SKEW
            request = data[42:90]
            payload = bytes([0x24, 2, 4, 0xEC]) + bytes(4) + bytes(4) + b"LOCL" + ntp_timestamp(now - 3600) + request[40:48] + ntp_timestamp(now) + ntp_timestamp(now)
            out.sendto(udp_frame(data[6:12], data[26:30], struct.unpack(">H", data[34:36])[0], payload), (GROUP, PORT))
            answered.append(now)


def main():
    stop, answered = threading.Event(), []
    server = threading.Thread(target=serve, args=(stop, answered))
    server.start()
    with tempfile.TemporaryDirectory() as tmp:
        master = Guest(tmp, "master", MASTER_MAC)
        slave = Guest(tmp, "slave", SLAVE_MAC)
        try:
            for guest in (master, slave):
                guest.wait_for("M3 complete")
                guest.wait_for("fos> ")
            slave_before = slave.send("date")
            slave.wait_for("(local time, from the RTC)", 10, slave_before)

            master_mark = master.send("run ptp master ip=10.7.0.1/24 ntp=10.7.0.99 secs=36")
            master.wait_for("ptp: NTP time ", 30, master_mark)
            text = master.text()[master_mark:]
            check(len(answered) >= 1, f"the NTP server was asked ({len(answered)} requests)")
            expected = datetime.fromtimestamp(answered[0], timezone.utc)
            check(expected.strftime("%Y-%m-%d") in text, "the master reports the NTP date", text[-400:])
            master.wait_for("ptp master: NIC 1 10.7.0.1", 20, master_mark)

            slave_mark = slave.send("run ptp ip=10.7.0.2/24 settime tz=+2 secs=18")
            slave.wait_for("ptp: clock and RTC set to ", 40, slave_mark)
            text = slave.text()[slave_mark:]
            check("(local time, UTC+2:00) from the master" in text, "the slave says what it set and in which zone", text[-400:])
            slave.wait_for("ptp: jitter", 30, slave_mark)
            check(text.count("clock and RTC set") == 1, "the clock is set once")
            date_mark = slave.send("date")
            slave.wait_for("(local time, from the RTC)", 10, date_mark)
            shown = slave.text()[date_mark:].split("(local")[0].split()[-2:]
            when = datetime.strptime(" ".join(shown), "%Y-%m-%d %H:%M:%S")
            wanted = (datetime.now(timezone.utc) + timedelta(seconds=SKEW + 2 * 3600)).replace(tzinfo=None)
            check(abs((when - wanted).total_seconds()) < 90, f"`date` shows the master's time plus 2 h ({when}, wanted about {wanted:%H:%M:%S})")
            master.wait_for("delay requests answered.", 40, master_mark)

            master_mark = master.send("run ptp master ip=10.7.0.1/24 secs=14")
            master.wait_for("ptp master: NIC 1 10.7.0.1", 30, master_mark)
            slave_mark = slave.send("run ptp ip=10.7.0.2/24 settime secs=10")
            slave.wait_for("ptp: settime: the master has no real time", 40, slave_mark)
            check("clock and RTC set" not in slave.text()[slave_mark:], "a master without NTP time does not set the slave's clock")
            slave.wait_for("ptp: jitter", 30, slave_mark)
            master.wait_for("delay requests answered.", 30, master_mark)
        finally:
            stop.set()
            server.join()
            master.stop()
            slave.stop()
    print("PASS: ptp ntp")


if __name__ == "__main__":
    main()
