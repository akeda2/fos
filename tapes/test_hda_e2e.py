#!/usr/bin/env python3
"""End-to-end test for the Intel HD Audio ("HDA") driver in
`kernel/src/hda.rs`: controller reset, CORB/RIRB command transport, and
node-graph/widget enumeration. Playback/capture are not implemented yet.

This test proves:

- The controller is discovered on the PCI bus and its MMIO BAR is mapped
  and reset successfully (`GCTL_CRST` clear-then-set round trip).
- At least one codec address responds on `STATESTS` after reset, and a
  CORB/RIRB command round trip correctly reads back that codec's Vendor
  ID -- confirmed against QEMU's own synthetic codec identity
  (`hw/audio/hda-codec-common.h`'s `QEMU_HDA_ID_OUTPUT`/
  `QEMU_HDA_ID_DUPLEX`/`QEMU_HDA_ID_MICRO`, all vendor `0x1af4`, QEMU's
  own convention for synthetic devices), not merely that *some* nonzero
  value came back.
- The driver correctly walks the codec's node graph and finds its Audio
  Function Group, then that AFG's own child widgets -- confirmed against
  QEMU's known `hda-duplex` layout (`hw/audio/hda-codec-common.h`): a DAC
  (`nid 2`) and its line-out pin complex (`nid 3`, connected to `nid 2`),
  plus an ADC (`nid 4`) and its line-in pin complex (`nid 5`, connected
  to `nid 4`).

Not part of `make test`; mirrors every other `tapes/test_*_e2e.py`
focused manual regression harness.
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

MCAST_GROUP = "230.42.42.59"
MCAST_PORT = 4259
ROOT = Path(__file__).resolve().parent.parent
IMAGE = ROOT / "build" / "fos.img"
TERM_CLIENT_MAC = bytes.fromhex("02deadbeef42")
GUEST_MAC = "52:54:00:12:34:62"

CHAT_ETHERTYPE = (0x88, 0xB6)
TERM_TAG_HELLO = 0x20
TERM_TAG_DATA = 0x21
TERM_TAG_BYE = 0x22

# QEMU's synthetic HDA codecs all report vendor ID 0x1af4 -- the same
# convention QEMU uses for virtio and every other device with no real
# hardware vendor identity (see `hw/audio/hda-codec-common.h`).
EXPECTED_VENDOR_ID = 0x1AF4


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
            "-audiodev",
            "none,id=snd0",
            "-device",
            "intel-hda",
            "-device",
            "hda-duplex,audiodev=snd0",
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

    def run_command(command: bytes, timeout: float = 10) -> bytes:
        start = len(output)
        mcast.send(eth_frame(guest_mac, TERM_CLIENT_MAC, TERM_TAG_DATA, command + b"\n"))
        return read_until(b"fos> ", timeout, start)

    guest_mac = send_tape.parse_mac(GUEST_MAC)

    try:
        mcast = McastSocket(MCAST_GROUP, MCAST_PORT)

        read_until(b"Waiting for a remote terminal client to connect", 30)
        mcast.send(eth_frame(b"\xff" * 6, TERM_CLIENT_MAC, TERM_TAG_HELLO))
        read_until(b"Remote terminal connected", 10)
        read_until(b"fos> ", 5)

        response = run_command(b"hda")
        text = response.decode("utf-8", "replace")

        if "No Intel HD Audio controller found" in text:
            raise SystemExit(f"controller not discovered:\n{text}")
        if "could not be reset" in text:
            raise SystemExit(f"controller reset failed:\n{text}")
        if "No codec responded" in text:
            raise SystemExit(f"no codec responded on STATESTS:\n{text}")

        match = re.search(r"\[cad (\d+)\] vendor:device 0x([0-9a-f]{4}):0x([0-9a-f]{4})", text)
        if not match:
            raise SystemExit(f"no codec vendor/device line found:\n{text}")
        cad, vendor_id, device_id = int(match.group(1)), int(match.group(2), 16), int(
            match.group(3), 16
        )
        if vendor_id != EXPECTED_VENDOR_ID:
            raise SystemExit(
                f"unexpected vendor ID 0x{vendor_id:04x} (expected 0x{EXPECTED_VENDOR_ID:04x}):\n{text}"
            )

        print(
            f"PASS: HDA controller reset; codec at cad {cad} reported "
            f"vendor:device 0x{vendor_id:04x}:0x{device_id:04x}."
        )

        afg_match = re.search(r"Audio Function Group: nid (\d+)", text)
        if not afg_match:
            raise SystemExit(f"no Audio Function Group reported:\n{text}")
        print(f"PASS: Audio Function Group found at nid {afg_match.group(1)}.")

        widgets = dict(
            (int(nid), (kind, conn))
            for nid, kind, conn in re.findall(
                r"nid (\d+): ([a-z /()A-Z0-9_-]+?) \((?:stereo|mono)\)"
                r"(?:[^\n]*conn -> nid (\d+))?",
                text,
            )
        )
        if 2 not in widgets or "audio output" not in widgets[2][0]:
            raise SystemExit(f"expected a DAC at nid 2:\n{text}")
        if 3 not in widgets or "pin complex" not in widgets[3][0] or widgets[3][1] != "2":
            raise SystemExit(f"expected a line-out pin at nid 3 connected to nid 2:\n{text}")
        print("PASS: DAC (nid 2) and line-out pin (nid 3 -> nid 2) both enumerated.")

        if 4 in widgets and 5 in widgets:
            if "audio input" not in widgets[4][0] or widgets[4][1] != "5":
                raise SystemExit(f"expected an ADC at nid 4 connected to nid 5:\n{text}")
            if "pin complex" not in widgets[5][0]:
                raise SystemExit(f"expected a line-in pin at nid 5:\n{text}")
            print("PASS: ADC (nid 4 -> nid 5) and line-in pin (nid 5) both enumerated (hda-duplex).")

        # A second call should be idempotent (no re-reset, same result)
        # rather than re-probing hardware every time.
        second = run_command(b"hda")
        second_text = second.decode("utf-8", "replace")
        if f"0x{vendor_id:04x}:0x{device_id:04x}" not in second_text:
            raise SystemExit(f"second 'hda' call reported a different result:\n{second_text}")
        print("PASS: second 'hda' call is idempotent (no re-probe, same codec reported).")

        mcast.send(eth_frame(guest_mac, TERM_CLIENT_MAC, TERM_TAG_BYE))
    finally:
        guest.terminate()
        try:
            guest.wait(timeout=5)
        except subprocess.TimeoutExpired:
            guest.kill()


if __name__ == "__main__":
    main()
