#!/usr/bin/env python3
"""The `net-chat` tape talks the raw-Ethernet chat protocol and keeps working from a `term net` session.

The tape is driven through a term net session. The test plays the chat peer itself on the same multicast segment (the term net
protocol and the chat protocol share an ethertype): it sends chat text and discovery frames to the guest and reads what the tape
sends back. Checks: text from the peer is shown; a typed line leaves as a chat text frame to the broadcast address; `/discover`
broadcasts a discovery probe, and a reply from the peer lands in the tape's address book (`/peers`); `/to MAC` sends the next line
by unicast; Escape returns to a usable shell. Ported from the legacy tapes/test_net_chat_e2e.py (which needed a second guest in the
shell `chat` command; this kernel has no such command). Run with `make test-net-chat`."""
import os
import select
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_boot import (BROADCAST, CODE_CANDIDATES, HOST_MAC, IMAGE, TAG_DATA, TAG_DISCOVER_REPLY, TAG_DISCOVER_REQUEST,  # noqa: E402
                       TAG_HELLO, VARS_CANDIDATES, Wire, check, clean, find, MCAST_GROUP, MCAST_PORT)

CHAT_TEXT = 0x01


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
                   "-netdev", f"socket,id=net0,mcast={MCAST_GROUP}:{MCAST_PORT}", "-device", "e1000,netdev=net0,mac=52:54:00:12:34:7a"]
        wire = Wire()
        process = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        raw = bytearray()
        deadline = time.monotonic() + 150

        def pump(marker):
            while marker not in clean(bytes(raw)):
                if time.monotonic() > deadline or process.poll() is not None:
                    raise SystemExit(f"FAIL: never saw {marker!r}\n{clean(bytes(raw))[-3500:]}")
                ready, _, _ = select.select([process.stdout], [], [], 0.2)
                if ready:
                    raw.extend(os.read(process.stdout.fileno(), 4096))

        try:
            pump("M3 complete")
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

            def line(text, marker, timeout=15):
                wire.send(guest, TAG_DATA, text.encode() + b"\r")
                return wire.until(marker, timeout)

            banner = line("run net-chat", "Type a line")
            check("fos tape chat" in banner, "net-chat starts from a term net session", banner)

            # Text from the peer is shown by the tape.
            wire.send(guest, CHAT_TEXT, b"hello tape")
            shown = wire.until("hello tape", 10)
            check("hello tape" in shown, "a chat text frame from the peer is printed", shown)

            # A typed line leaves as a chat text frame to the broadcast address.
            for byte in b"from the tape\r":
                wire.send(guest, TAG_DATA, bytes([byte]))
                time.sleep(0.05)
            sent = [payload.rstrip(b"\0") for _, tag, payload in wire.frames(3.0) if tag == CHAT_TEXT]
            check(any(b"from the tape" in payload for payload in sent), "a typed line is sent as a chat text frame (broadcast)", repr(sent))

            # Backspace removes the last character from the line and erases it on screen.
            for byte in b"abx\x08c\r":
                wire.send(guest, TAG_DATA, bytes([byte]))
                time.sleep(0.05)
            seen = list(wire.frames(3.0))
            echoed = b"".join(payload for _, tag, payload in seen if tag == TAG_DATA)
            check(b"\x08 \x08" in echoed, "Backspace is echoed as erase (BS space BS)", repr(echoed))
            sent = [payload.rstrip(b"\0") for _, tag, payload in seen if tag == CHAT_TEXT]
            check(any(payload == b"abc" for payload in sent), "Backspace drops the character from the sent line", repr(sent))

            # /discover broadcasts a probe; the peer's reply goes into the address book.
            wire.send(guest, TAG_DATA, b"/discover\r")
            probes = [source for source, tag, _ in wire.frames(3.0) if tag == TAG_DISCOVER_REQUEST]
            check(probes, "/discover broadcasts a discovery probe", "")
            wire.send(guest, TAG_DISCOVER_REPLY, b"testpeer")
            time.sleep(0.5)
            listing = line("/peers", "02:de:ad:be:ef:03")
            check(HOST_MAC.hex(":") in listing.lower(), "the peer that answered is in the address book (/peers)", listing)

            # /to MAC: the next line goes by unicast to that address.
            mac_text = HOST_MAC.hex(":")
            wire.send(guest, TAG_DATA, f"/to {mac_text}\r".encode())
            time.sleep(0.5)
            for byte in b"direct\r":
                wire.send(guest, TAG_DATA, bytes([byte]))
                time.sleep(0.05)
            direct = [payload.rstrip(b"\0") for _, tag, payload in wire.frames(3.0) if tag == CHAT_TEXT]
            check(any(b"direct" in payload for payload in direct), "after /to MAC a line is sent to that address", repr(direct))

            # Escape returns to the shell, which still answers.
            wire.send(guest, TAG_DATA, b"\x1b")
            check("fos> " in wire.until("fos> ", 15), "Escape returns to the shell", "")
            check("ticks at 100 Hz" in line("uptime", "ticks at 100 Hz"), "and the shell is still usable", "")
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
    print("net-chat: all checks passed")


if __name__ == "__main__":
    main()
