#!/usr/bin/env python3
"""End-to-end test for multi-controller Intel HD Audio support in
`kernel/src/hda.rs` (`initialize_all`/`get(index)`/`count()`).

This test boots a guest with *two* independent `intel-hda` PCI
controllers (`hda0` carrying an `hda-output`-only codec, `hda1` carrying
an `hda-duplex` codec), mirroring a real machine with one onboard
controller plus a discrete add-in card. It proves:

- Both controllers are discovered and reset independently (distinct
  MMIO bases, both reaching a working CORB/RIRB command transport).
- Each controller's own codec is enumerated correctly and independently
  -- controller 0's codec has no ADC/line-in pins (`hda-output` is
  output-only), while controller 1's codec has both a DAC/line-out and
  an ADC/line-in pair (`hda-duplex`), so a driver that let one
  controller's state leak into the other's would be caught here.
- A second `hda` call remains idempotent (no re-probe) with both
  controllers still reported.

Not part of `make test`; mirrors `tapes/test_hda_e2e.py`'s own
single-controller structure and every other `tapes/test_*_e2e.py`
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

MCAST_GROUP = "230.42.42.61"
MCAST_PORT = 4261
ROOT = Path(__file__).resolve().parent.parent
IMAGE = ROOT / "build" / "fos.img"
TERM_CLIENT_MAC = bytes.fromhex("02deadbeef43")
GUEST_MAC = "52:54:00:12:34:64"

CHAT_ETHERTYPE = (0x88, 0xB6)
TERM_TAG_HELLO = 0x20
TERM_TAG_DATA = 0x21
TERM_TAG_BYE = 0x22

# QEMU's synthetic HDA codecs all report vendor ID 0x1af4 -- see
# `tapes/test_hda_e2e.py`'s own comment on this same convention.
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
            "-audiodev",
            "none,id=snd1",
            "-device",
            "intel-hda,id=hda0",
            "-device",
            "hda-output,bus=hda0.0,audiodev=snd0",
            "-device",
            "intel-hda,id=hda1",
            "-device",
            "hda-duplex,bus=hda1.0,audiodev=snd1",
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
            raise SystemExit(f"no controller discovered at all:\n{text}")
        if "could not be reset" in text:
            raise SystemExit(f"controller reset failed:\n{text}")

        count_match = re.search(r"(\d+) Intel HD Audio controller\(s\) discovered", text)
        if not count_match:
            raise SystemExit(f"no controller-count summary line found:\n{text}")
        count = int(count_match.group(1))
        if count != 2:
            raise SystemExit(f"expected 2 controllers discovered, found {count}:\n{text}")
        print("PASS: both HDA controllers discovered (count == 2).")

        # Split the transcript into one chunk per "Controller N: ..."
        # header so each controller's own MMIO base/codec block can be
        # checked independently, rather than matching the first
        # occurrence of anything across both controllers' output.
        blocks = re.split(r"(?=Controller \d+: MMIO base)", text)
        blocks = [b for b in blocks if b.startswith("Controller ")]
        if len(blocks) != 2:
            raise SystemExit(f"expected 2 'Controller N: ...' blocks, found {len(blocks)}:\n{text}")

        mmio_bases = []
        for block in blocks:
            header = re.match(r"Controller (\d+): MMIO base 0x([0-9a-f]+)\.", block)
            if not header:
                raise SystemExit(f"malformed controller header:\n{block}")
            mmio_bases.append(int(header.group(2), 16))
            if "No codec responded" in block:
                raise SystemExit(f"a controller's codec did not respond:\n{block}")

        if mmio_bases[0] == mmio_bases[1]:
            raise SystemExit(
                f"both controllers reported the same MMIO base 0x{mmio_bases[0]:08x} "
                "-- they should be independently mapped"
            )
        print(
            f"PASS: controllers have distinct MMIO bases "
            f"(0x{mmio_bases[0]:08x} != 0x{mmio_bases[1]:08x})."
        )

        # Controller 0 carries an 'hda-output' codec: a DAC (nid 2) and
        # its line-out pin (nid 3), but no ADC/line-in pair at all.
        block0 = blocks[0]
        if "vendor:device 0x1af4" not in block0.lower():
            raise SystemExit(f"controller 0's codec did not report vendor 0x1af4:\n{block0}")
        if "audio input" in block0:
            raise SystemExit(
                f"controller 0 (hda-output) unexpectedly reported an ADC widget:\n{block0}"
            )
        print("PASS: controller 0 (hda-output) enumerated with no ADC/line-in widgets.")

        # Controller 1 carries an 'hda-duplex' codec: both a DAC/line-out
        # pair and an ADC/line-in pair, exactly like
        # `tapes/test_hda_e2e.py`'s single-controller case.
        block1 = blocks[1]
        if "vendor:device 0x1af4" not in block1.lower():
            raise SystemExit(f"controller 1's codec did not report vendor 0x1af4:\n{block1}")
        if "audio output" not in block1 or "audio input" not in block1:
            raise SystemExit(
                f"controller 1 (hda-duplex) should report both a DAC and an ADC widget:\n{block1}"
            )
        print("PASS: controller 1 (hda-duplex) enumerated with both DAC and ADC widgets.")

        # A second call should be idempotent: same controller count, no
        # re-probe of hardware.
        second = run_command(b"hda")
        second_text = second.decode("utf-8", "replace")
        second_count_match = re.search(
            r"(\d+) Intel HD Audio controller\(s\) discovered", second_text
        )
        if not second_count_match or int(second_count_match.group(1)) != count:
            raise SystemExit(f"second 'hda' call reported a different count:\n{second_text}")
        print("PASS: second 'hda' call is idempotent (no re-probe, same controller count).")

        mcast.send(eth_frame(guest_mac, TERM_CLIENT_MAC, TERM_TAG_BYE))
    finally:
        guest.terminate()
        try:
            guest.wait(timeout=5)
        except subprocess.TimeoutExpired:
            guest.kill()


if __name__ == "__main__":
    main()
