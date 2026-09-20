#!/usr/bin/env python3
"""End-to-end test for the core line-oriented Rust editor tape.

The test drives the tape through one remote ``term net`` session, edits a
file that was created through the normal shell, saves it through the tape's
FOSFS callbacks, and reads it back through ``cat``. It deliberately uses the
default-sized 128 MiB QEMU configuration, proving small files work without
the optional allocator-backed memory above 128 MiB.
"""

import selectors
import socket as socketlib
import struct
import subprocess
import time
import os
from pathlib import Path

MCAST_GROUP = "230.42.42.54"
MCAST_PORT = 4254
IMAGE = Path(__file__).resolve().parent.parent / "build" / "fos.img"
TERM_CLIENT_MAC = bytes.fromhex("02deadbeef31")
CHAT_ETHERTYPE = bytes((0x88, 0xB6))
TERM_TAG_HELLO = 0x20
TERM_TAG_DATA = 0x21


class McastSocket:
    def __init__(self):
        self.sock = socketlib.socket(
            socketlib.AF_INET, socketlib.SOCK_DGRAM, socketlib.IPPROTO_UDP
        )
        self.sock.setsockopt(socketlib.SOL_SOCKET, socketlib.SO_REUSEADDR, 1)
        self.sock.bind(("0.0.0.0", MCAST_PORT))
        membership = struct.pack(
            "4sl", socketlib.inet_aton(MCAST_GROUP), socketlib.INADDR_ANY
        )
        self.sock.setsockopt(socketlib.IPPROTO_IP, socketlib.IP_ADD_MEMBERSHIP, membership)
        self.sock.setsockopt(socketlib.IPPROTO_IP, socketlib.IP_MULTICAST_TTL, 1)

    def send(self, frame):
        self.sock.sendto(frame, (MCAST_GROUP, MCAST_PORT))

    def close(self):
        self.sock.close()


def main():
    if not IMAGE.exists():
        raise SystemExit(f"{IMAGE} does not exist; run `make image` first.")

    qemu = subprocess.Popen(
        [
            "qemu-system-i386",
            "-m",
            "128",
            "-drive",
            f"format=raw,file={IMAGE}",
            "-serial",
            "stdio",
            "-monitor",
            "none",
            "-no-reboot",
            "-no-shutdown",
            "-netdev",
            f"socket,id=net0,mcast={MCAST_GROUP}:{MCAST_PORT}",
            "-device",
            "rtl8139,netdev=net0",
            "-display",
            "none",
        ],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )
    output = bytearray()
    selector = selectors.DefaultSelector()
    selector.register(qemu.stdout, selectors.EVENT_READ)
    mcast = McastSocket()

    def read_until(marker, timeout=15):
        deadline = time.monotonic() + timeout
        while marker not in output:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise SystemExit(
                    f"timed out waiting for {marker!r}; output so far:\n"
                    f"{output.decode('utf-8', 'replace')}"
                )
            for key, _ in selector.select(remaining):
                chunk = os.read(key.fd, 4096)
                if not chunk:
                    raise SystemExit(
                        f"QEMU serial stream ended early:\n"
                        f"{output.decode('utf-8', 'replace')}"
                    )
                output.extend(chunk)

    def frame(tag, payload=b""):
        ethernet = b"\xff" * 6 + TERM_CLIENT_MAC + CHAT_ETHERTYPE
        result = bytearray(ethernet + bytes((tag,)) + payload)
        result.extend(b"\0" * max(0, 60 - len(result)))
        return bytes(result)

    def command(text, marker=b"fos> ", timeout=15):
        del output[:]
        if not text.endswith("\n"):
            text += "\n"
        mcast.send(frame(TERM_TAG_DATA, text.encode("ascii")))
        read_until(marker, timeout)

    try:
        read_until(b"Waiting for a remote terminal client to connect", 30)
        mcast.send(frame(TERM_TAG_HELLO))
        read_until(b"Remote terminal connected", 10)
        read_until(b"fos> ", 5)

        command("write TEST.TXT alpha beta\n")
        if b"File written." not in output:
            raise SystemExit(f"failed to seed TEST.TXT:\n{output.decode('utf-8', 'replace')}")

        command("tape load disk ED.BIN")
        if b"Loaded " not in output or b" byte(s) from 'ED.BIN'" not in output:
            raise SystemExit(f"failed to load ED.BIN:\n{output.decode('utf-8', 'replace')}")

        del output[:]
        mcast.send(frame(TERM_TAG_DATA, b"tape run ed TEST.TXT\n"))
        read_until(b"ed> ", 15)

        command("p\n", marker=b"ed> ")
        if b"1: alpha beta" not in output:
            raise SystemExit(f"editor did not print the seed file:\n{output.decode('utf-8', 'replace')}")

        command("i1\n", marker=b". ")
        command("first\n", marker=b". ")
        command(".\n", marker=b"ed> ")
        command("s2/replaced\n", marker=b"ed> ")
        command("a\n", marker=b". ")
        command("last\n", marker=b". ")
        command(".\n", marker=b"ed> ")
        command("d1\n", marker=b"ed> ")
        command("x\n", marker=b"fos> ")
        if b"written TEST.TXT" not in output:
            raise SystemExit(f"editor did not report a successful save:\n{output.decode('utf-8', 'replace')}")

        command("cat TEST.TXT\n")
        text = output.decode("utf-8", "replace")
        if "replaced\nlast\n" not in text:
            raise SystemExit(f"saved file has unexpected contents:\n{text}")
        print("PASS: editor loaded, edited, saved, and persisted TEST.TXT.")
    finally:
        mcast.close()
        qemu.terminate()
        try:
            qemu.wait(timeout=5)
        except subprocess.TimeoutExpired:
            qemu.kill()
            qemu.wait()


if __name__ == "__main__":
    main()
