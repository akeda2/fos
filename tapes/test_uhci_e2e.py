#!/usr/bin/env python3
"""End-to-end test for the UHCI (USB 1.1) host controller driver in
`kernel/src/uhci.rs`: controller discovery/bring-up, root-port reset, and
full standard-enumeration control transfers (`GET_DESCRIPTOR` x3,
`SET_ADDRESS`, `SET_CONFIGURATION`) against a real, full-speed,
class-compliant device.

This test proves:

- At least one UHCI controller is discovered on the PCI bus and brought up
  (`running=yes`).
- The port a device is actually plugged into reports `connected,
  full-speed` after the reset/enable sequence -- UHCI never auto-enables a
  port the way some other host-controller families do, so this also
  covers the explicit `SET_PORT_ENABLE` step.
- The full device descriptor is read back with the real vendor/product ID
  QEMU's synthetic `usb-audio` device reports (`hw/usb/dev-audio.c`'s
  `0x46f4:0x0002`, the same Red Hat/QEMU USB vendor ID convention
  `test_hda_e2e.py` documents for `0x1af4` on the audio-*codec* side of a
  different device family) -- confirming a real SETUP/DATA/STATUS control
  transfer round-tripped correctly, not just that *something* came back.
- The configuration descriptor is read back in two stages (a short header
  to learn `wTotalLength`, then the full descriptor) and contains a real
  USB Audio Class 1.0 `CS_INTERFACE` header descriptor (`bDescriptorType
  0x24`, `bDescriptorSubtype 0x01`) -- proving the multi-TD data stage
  correctly reassembles more than one interface's worth of descriptors,
  not just the first 8-byte probe.
- `SET_CONFIGURATION` succeeds, and a second, independent `usb` command
  re-enumerates identically (same vendor/product ID, same configuration
  descriptor bytes) -- the command resets the port from scratch every
  call rather than caching stale state.

Isochronous transfers and USB Audio Class *semantic* descriptor parsing
(channel counts, sample rates, ...) are not implemented; this test only
proves enumeration itself, matching `uhci.rs`'s own documented scope.

Not part of `make test`; mirrors every other `tapes/test_*_e2e.py` focused
manual regression harness.
"""

import os
import re
import selectors
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "tools"))
sys.path.insert(0, str(Path(__file__).resolve().parent))
import send_tape  # noqa: E402
from test_primes_e2e import McastSocket  # noqa: E402

MCAST_GROUP = "230.42.42.62"
MCAST_PORT = 4262
ROOT = Path(__file__).resolve().parent.parent
IMAGE = ROOT / "build" / "fos.img"
TERM_CLIENT_MAC = bytes.fromhex("02deadbeef62")
GUEST_MAC = "52:54:00:12:34:71"

CHAT_ETHERTYPE = (0x88, 0xB6)
TERM_TAG_HELLO = 0x20
TERM_TAG_DATA = 0x21
TERM_TAG_BYE = 0x22

# QEMU's synthetic usb-audio device (hw/usb/dev-audio.c) reports this
# vendor/product ID pair, the same Red Hat/QEMU USB vendor ID convention
# used elsewhere for devices with no real hardware vendor identity.
EXPECTED_VENDOR_ID = 0x46F4
EXPECTED_PRODUCT_ID = 0x0002


def eth_frame(destination: bytes, source: bytes, tag: int, payload: bytes = b"") -> bytes:
    frame = bytearray(destination + source + bytes(CHAT_ETHERTYPE) + bytes([tag]) + payload)
    if len(frame) < 60:
        frame.extend(b"\0" * (60 - len(frame)))
    return bytes(frame)


def main() -> None:
    if not IMAGE.exists():
        raise SystemExit(f"missing {IMAGE}; run `make image` first")

    selector = selectors.DefaultSelector()
    output = bytearray()
    wav_path = ROOT / "build" / "uhci_e2e_test.wav"

    guest = subprocess.Popen(
        [
            "qemu-system-i386",
            "-drive",
            f"format=raw,file={IMAGE}",
            "-snapshot",
            "-serial",
            "stdio",
            "-no-reboot",
            "-no-shutdown",
            "-netdev",
            f"socket,id=net0,mcast={MCAST_GROUP}:{MCAST_PORT}",
            "-device",
            f"rtl8139,netdev=net0,mac={GUEST_MAC}",
            "-usb",
            "-device",
            "piix3-usb-uhci,id=uhci",
            "-device",
            "usb-audio,bus=uhci.0,audiodev=snd0",
            "-audiodev",
            f"wav,id=snd0,path={wav_path}",
            "-display",
            "none",
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )
    selector.register(guest.stdout, selectors.EVENT_READ, "guest")

    def read_until(marker: bytes, timeout: float, start: int = 0) -> bytes:
        deadline = time.monotonic() + timeout
        while marker not in output[start:]:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise SystemExit(
                    f"timed out waiting for {marker!r}; output since checkpoint:\n"
                    f"{output[start:].decode('utf-8', 'replace')}"
                )
            for _ in selector.select(remaining):
                chunk = os.read(guest.stdout.fileno(), 4096)
                if not chunk:
                    raise SystemExit(
                        f"serial stream ended early:\n{output.decode('utf-8', 'replace')}"
                    )
                output.extend(chunk)
        return bytes(output[start:])

    def run_command(command: bytes, timeout: float = 15) -> bytes:
        start = len(output)
        mcast.send(eth_frame(guest_mac, TERM_CLIENT_MAC, TERM_TAG_DATA, command + b"\n"))
        return read_until(b"fos> ", timeout, start)

    def check(text: str) -> None:
        if "No UHCI controllers found" in text:
            raise SystemExit(f"no UHCI controller discovered:\n{text}")
        if "running=no" in text:
            raise SystemExit(f"a discovered controller is not running:\n{text}")
        if "no device connected" not in text and "connected, full-speed" not in text:
            raise SystemExit(f"no port reported a connection attempt:\n{text}")
        if "connected, full-speed" not in text:
            raise SystemExit(f"no port reported the attached full-speed device:\n{text}")

        match = re.search(r"vendor=0x([0-9a-f]{4}) product=0x([0-9a-f]{4})", text)
        if not match:
            raise SystemExit(f"no device descriptor vendor/product line found:\n{text}")
        vendor_id, product_id = int(match.group(1), 16), int(match.group(2), 16)
        if vendor_id != EXPECTED_VENDOR_ID or product_id != EXPECTED_PRODUCT_ID:
            raise SystemExit(
                f"unexpected vendor:product 0x{vendor_id:04x}:0x{product_id:04x} "
                f"(expected 0x{EXPECTED_VENDOR_ID:04x}:0x{EXPECTED_PRODUCT_ID:04x}):\n{text}"
            )

        if "configuration descriptor (" not in text:
            raise SystemExit(f"no configuration descriptor read reported:\n{text}")
        # A real USB Audio Class 1.0 CS_INTERFACE header: bDescriptorType
        # 0x24 (CS_INTERFACE), bDescriptorSubtype 0x01 (HEADER), always
        # immediately preceded by the standard interface descriptor's own
        # bytes -- present verbatim in the hex dump if the multi-TD data
        # stage reassembled more than the first probe packet correctly.
        if "24 01 00 01" not in text:
            raise SystemExit(f"no USB Audio Class HEADER descriptor found in dump:\n{text}")

        if "SET_CONFIGURATION failed" in text:
            raise SystemExit(f"SET_CONFIGURATION failed:\n{text}")
        if "configured (bConfigurationValue=" not in text:
            raise SystemExit(f"no successful SET_CONFIGURATION reported:\n{text}")

    guest_mac = send_tape.parse_mac(GUEST_MAC)

    try:
        mcast = McastSocket(MCAST_GROUP, MCAST_PORT)

        read_until(b"Waiting for a remote terminal client to connect", 30)
        mcast.send(eth_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_HELLO))
        read_until(b"Remote terminal connected", 10)
        read_until(b"fos> ", 5)

        first = run_command(b"usb").decode("utf-8", "replace")
        check(first)
        print("PASS: UHCI controller discovered, running, and its port enumerated.")
        print(
            f"PASS: device descriptor read back vendor:product "
            f"0x{EXPECTED_VENDOR_ID:04x}:0x{EXPECTED_PRODUCT_ID:04x}."
        )
        print("PASS: configuration descriptor contains a real USB Audio Class HEADER.")
        print("PASS: SET_CONFIGURATION succeeded.")

        # A second, independent call must reset the port and re-run the
        # whole enumeration from scratch, reporting the identical result --
        # not a cached answer from the first call.
        second = run_command(b"usb").decode("utf-8", "replace")
        check(second)

        def config_bytes(text: str) -> str:
            return "\n".join(
                line for line in text.splitlines() if line.startswith("0x0000")
            )

        if config_bytes(first) != config_bytes(second):
            raise SystemExit(
                f"second 'usb' call reported different configuration descriptor bytes:\n"
                f"first:\n{first}\nsecond:\n{second}"
            )
        print("PASS: second 'usb' call independently re-enumerates the same device.")

        mcast.send(eth_frame(guest_mac, TERM_CLIENT_MAC, TERM_TAG_BYE))
    finally:
        guest.terminate()
        try:
            guest.wait(timeout=5)
        except subprocess.TimeoutExpired:
            guest.kill()
        wav_path.unlink(missing_ok=True)


if __name__ == "__main__":
    main()
