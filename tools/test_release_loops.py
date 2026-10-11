#!/usr/bin/env python3
"""Blocking commands end when the `term net` client escapes or disconnects, whichever NIC they listen on.

`nic sniff` and `tape load net` run until stopped. Over a `term net` session the stop has to come from the session itself: the
client's Escape, or its disconnect (it may be gone for good: a closed terminal, a dropped cable). In the legacy kernel a loop that
polled a different NIC than the session's could never see either and the machine had to be walked to. Checks, with the session on
NIC 1 and a second NIC on another segment: `nic sniff 2` (a NIC the session is not on) and `nic sniff 1` (the session's own) and
`tape load net` each end on a lone Escape byte, and on a disconnect, after which the machine accepts a new session. Ported from the
legacy test_release_loops_e2e. Run with `make test-release-loops`."""
import os
import select
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from testslot import port  # noqa: E402
from test_boot import (BROADCAST, CODE_CANDIDATES, IMAGE, TAG_BYE, TAG_DATA, TAG_DISCOVER_REPLY, TAG_DISCOVER_REQUEST,  # noqa: E402
                       TAG_HELLO, VARS_CANDIDATES, Wire, check, clean, find, MCAST_GROUP, MCAST_PORT)

OTHER_GROUP, OTHER_PORT = "230.42.42.91", port(4291)


def main():
    code, variables = find("FOS_OVMF_CODE", CODE_CANDIDATES), find("FOS_OVMF_VARS", VARS_CANDIDATES)
    with tempfile.TemporaryDirectory() as tmp:
        image = Path(tmp) / "image.img"
        shutil.copy(IMAGE, image)
        vars_copy = Path(tmp) / "vars.fd"
        shutil.copy(variables, vars_copy)
        command = ["qemu-system-x86_64", "-machine", "q35", "-m", "512", "-display", "none", "-serial", "stdio", "-no-reboot",
                   "-drive", f"if=pflash,format=raw,readonly=on,file={code}", "-drive", f"if=pflash,format=raw,file={vars_copy}",
                   "-drive", f"format=raw,file={image}",
                   "-netdev", f"socket,id=net0,mcast={MCAST_GROUP}:{MCAST_PORT}", "-device", "e1000,netdev=net0,mac=52:54:00:12:34:7a",
                   "-netdev", f"socket,id=net1,mcast={OTHER_GROUP}:{OTHER_PORT}", "-device", "e1000,netdev=net1,mac=52:54:00:12:34:7b"]
        wire = Wire()
        process = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        raw = bytearray()
        deadline = time.monotonic() + 240

        def pump(seconds):
            end = time.monotonic() + seconds
            while time.monotonic() < end:
                ready, _, _ = select.select([process.stdout], [], [], 0.1)
                if ready:
                    raw.extend(os.read(process.stdout.fileno(), 4096))

        def serial_wait(marker, after, timeout=20):
            end = time.monotonic() + timeout
            while marker not in clean(bytes(raw))[after:]:
                if time.monotonic() > min(end, deadline) or process.poll() is not None:
                    return False
                pump(0.2)
            return True

        try:
            while "M3 complete" not in clean(bytes(raw)):
                if time.monotonic() > deadline or process.poll() is not None:
                    raise SystemExit(f"FAIL: the guest never booted\n{clean(bytes(raw))[-2500:]}")
                pump(0.2)
            wire.send(BROADCAST, TAG_DISCOVER_REQUEST, b"host")
            guest = None
            for _ in range(30):
                for source, tag, _ in wire.frames(0.2):
                    if tag == TAG_DISCOVER_REPLY:
                        guest = source
                if guest:
                    break
            wire.send(guest, TAG_HELLO)
            wire.until("fos> ", 10)

            def start(text, marker):
                wire.send(guest, TAG_DATA, text.encode() + b"\r")
                wire.until(marker, 15)
                time.sleep(0.5)

            for text, marker, label, end_marker in (
                    ("nic sniff 2", "Sniffing NIC 2", "`nic sniff` on a NIC the session is not on", "Stopped:"),
                    ("nic sniff 1", "Sniffing NIC 1", "`nic sniff` on the session's own NIC", "Stopped:"),
                    ("tape load net", "Waiting for a tape over the network", "`tape load net`", "gave up")):
                # A lone Escape byte from the client ends it, and the session carries on.
                start(text, marker)
                wire.send(guest, TAG_DATA, b"\x1b")
                back = wire.until("fos> ", 15)
                check("fos> " in back, f"{label} ends on the client's Escape", back)
                check("ticks at 100 Hz" in (wire.send(guest, TAG_DATA, b"uptime\r") or wire.until("ticks at 100 Hz", 10)), f"and the session still works afterwards", "")

                # The client disconnects while it runs: the machine must not stay stuck in it.
                start(text, marker)
                mark = len(clean(bytes(raw)))
                wire.send(guest, TAG_BYE)
                ended = serial_wait(end_marker, mark, 20)   # the command's own end message (the prompt is printed by the disconnect itself)
                check(ended, f"{label} ends when the client disconnects", clean(bytes(raw))[mark:][-1500:])
                # And a new session is accepted and answered.
                wire.send(guest, TAG_HELLO)
                wire.until("fos> ", 15)
                wire.send(guest, TAG_DATA, b"uptime\r")
                try:
                    answered = wire.until("ticks at 100 Hz", 10)
                except SystemExit:
                    pump(1.0)
                    raise SystemExit("FAIL: no answer from a new session after the disconnect; serial output:\n" + clean(bytes(raw))[-1800:])
                check("ticks at 100 Hz" in answered, "a new session is accepted and answered afterwards", "")
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
    print("release loops: all checks passed")


if __name__ == "__main__":
    main()
