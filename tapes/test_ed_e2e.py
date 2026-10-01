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
import tempfile
import time
import os
from pathlib import Path

MCAST_GROUP = "230.42.42.54"
MCAST_PORT = 4254
IMAGE = Path(
    os.environ.get(
        "FOS_TEST_IMAGE",
        str(Path(__file__).resolve().parent.parent / "build" / "fos.img"),
    )
)
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

    disk2_dir = tempfile.TemporaryDirectory(prefix="fos-ed-disk2-")
    # A blank 4 MiB secondary disk, matching tools/qemu_smoke.py's own
    # --second-disk setup: `format disk2` handles the no-partition-table
    # starting state, and 4 MiB comfortably fits FOSFS3's fixed directory
    # area plus a couple of small test files.
    disk2_path = os.path.join(disk2_dir.name, "disk2.img")
    with open(disk2_path, "wb") as disk2_file:
        disk2_file.truncate(4 * 1024 * 1024)

    qemu = subprocess.Popen(
        [
            "qemu-system-i386",
            "-m",
            "128",
            "-drive",
            f"format=raw,file={IMAGE}",
            "-drive",
            f"format=raw,file={disk2_path}",
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

        command("write test.txt alpha beta\n")
        if b"File written." not in output:
            raise SystemExit(f"failed to seed test.txt:\n{output.decode('utf-8', 'replace')}")

        command("tape load disk ed.bin")
        if b"Loaded " not in output or b" byte(s) from 'ed.bin'" not in output:
            raise SystemExit(f"failed to load ed.bin:\n{output.decode('utf-8', 'replace')}")

        del output[:]
        mcast.send(frame(TERM_TAG_DATA, b"tape run ed test.txt\n"))
        read_until(b"ed> ", 15)

        command("p\n", marker=b"ed> ")
        if b"1: alpha beta" not in output:
            raise SystemExit(f"editor did not print the seed file:\n{output.decode('utf-8', 'replace')}")

        command("i1\n", marker=b". ")
        command("first\n", marker=b". ")
        command(".\n", marker=b"ed> ")
        command("s2/replaced\n", marker=b"ed> ")
        # cLINE/OLD/NEW changes text inside a line; eLINE pre-fills the old
        # line so Backspace trims its end and typing appends.
        command("c2/place/PLACE\n", marker=b"ed> ")
        if b"2: rePLACEd" not in output:
            raise SystemExit(f"c did not change text inside the line:\n{output.decode('utf-8', 'replace')}")
        command("c2/nothere/x\n", marker=b"ed> ")
        if b"text not found" not in output:
            raise SystemExit(f"c did not report a missing OLD text:\n{output.decode('utf-8', 'replace')}")
        command("c2/PLACE/place\n", marker=b"ed> ")
        command("e2\n", marker=b"edit: replaced")
        command("\x08\x08ed\n", marker=b"ed> ")
        command("p\n", marker=b"ed> ")
        if b"2: replaced" not in output:
            raise SystemExit(f"e did not keep the edited line:\n{output.decode('utf-8', 'replace')}")
        # Cursor keys (ESC [ ... sequences) reach the tape as input over term
        # net instead of aborting it. Home + Delete + a typed letter turn "replaced"
        # into "Replaced"; Right, Backspace and End + a letter then give
        # "Rplacedd"; finally Home, Delete, "re" and End + Backspace restore it.
        command("e2\n", marker=b"edit: replaced")
        command("\x1b[H\x1b[3~R\n", marker=b"ed> ")
        command("p\n", marker=b"ed> ")
        if b"2: Replaced" not in output:
            raise SystemExit(f"cursor editing (Home, Delete) failed:\n{output.decode('utf-8', 'replace')}")
        command("e2\n", marker=b"edit: Replaced")
        command("\x1b[H\x1b[C\x1b[C\x08\x1b[Fd\n", marker=b"ed> ")
        command("p\n", marker=b"ed> ")
        if b"2: Rplacedd" not in output:
            raise SystemExit(f"cursor editing (Right, Backspace, End) failed:\n{output.decode('utf-8', 'replace')}")
        command("e2\n", marker=b"edit: Rplacedd")
        command("\x1b[H\x1b[3~re\x1b[F\x08\n", marker=b"ed> ")
        command("p\n", marker=b"ed> ")
        if b"2: replaced\r\n" not in output and b"2: replaced\n" not in output:
            raise SystemExit(f"cursor editing (insert, End, Backspace) failed:\n{output.decode('utf-8', 'replace')}")
        command("a\n", marker=b". ")
        command("last\n", marker=b". ")
        command(".\n", marker=b"ed> ")
        command("d1\n", marker=b"ed> ")
        command("x\n", marker=b"fos> ")
        if b"written test.txt" not in output:
            raise SystemExit(f"editor did not report a successful save:\n{output.decode('utf-8', 'replace')}")

        command("cat test.txt\n")
        text = output.decode("utf-8", "replace")
        if "replaced\nlast\n" not in text:
            raise SystemExit(f"saved file has unexpected contents:\n{text}")

        command("tape run ed router.cfg", marker=b"ed> ")
        del output[:]
        mcast.send(frame(TERM_TAG_DATA, b"m\n"))
        read_until(b"-- more --", 10)
        if b"1: # fos router configuration example" not in output:
            raise SystemExit(f"paged print did not show the first page:\n{output.decode('utf-8', 'replace')}")
        mcast.send(frame(TERM_TAG_DATA, b"q"))
        read_until(b"ed> ", 10)
        command("q\n")

        # `tape run ed diskN/NAME` reads/writes a file on an attached
        # secondary disk instead of only ever the boot disk, exactly like
        # `tape run @diskN/NAME` and `tape load diskN NAME` (see
        # kernel/src/shell.rs's `tape_fs_reference`). disk2 starts as a
        # blank image with no FOSFS structure at all, so it must be
        # formatted first, the same as `tools/qemu_smoke.py`'s own
        # --second-disk scenario.
        command("format disk2\n")
        if b"FOSFS formatted." not in output:
            raise SystemExit(f"failed to format disk2:\n{output.decode('utf-8', 'replace')}")

        command("write disk2 test2.txt one two\n")
        if b"File written." not in output:
            raise SystemExit(f"failed to seed disk2 test2.txt:\n{output.decode('utf-8', 'replace')}")

        del output[:]
        mcast.send(frame(TERM_TAG_DATA, b"tape run ed disk2/test2.txt\n"))
        read_until(b"ed> ", 15)

        command("p\n", marker=b"ed> ")
        if b"1: one two" not in output:
            raise SystemExit(f"editor did not print disk2's seed file:\n{output.decode('utf-8', 'replace')}")

        command("a\n", marker=b". ")
        command("added\n", marker=b". ")
        command(".\n", marker=b"ed> ")
        command("x\n", marker=b"fos> ")
        if b"written disk2/test2.txt" not in output:
            raise SystemExit(f"editor did not report a successful disk2 save:\n{output.decode('utf-8', 'replace')}")

        command("cat disk2 test2.txt\n")
        text = output.decode("utf-8", "replace")
        if "one two\nadded\n" not in text:
            raise SystemExit(f"disk2 saved file has unexpected contents:\n{text}")

        print("PASS: editor loaded, edited, saved, and persisted test.txt (disk1 and disk2).")
    finally:
        mcast.close()
        qemu.terminate()
        try:
            qemu.wait(timeout=5)
        except subprocess.TimeoutExpired:
            qemu.kill()
            qemu.wait()
        disk2_dir.cleanup()


if __name__ == "__main__":
    main()
