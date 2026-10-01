#!/usr/bin/env python3
"""End-to-end test (QEMU) for MSI on NICs.

QEMU's `e1000e` model advertises an MSI capability, so it exercises the real path:
`nic::try_enable_msi` programs the capability, forces an interrupt on the NIC and
waits for the message to arrive (the self-test), after which the NIC runs on an
MSI vector instead of its legacy IRQ line. This checks that:

  1. the NIC ends up on an MSI vector and the self-test message was counted;
  2. further frames keep arriving as MSI messages (`nic list` and `irqstat`
     counts rise with traffic);
  3. with `no-msi` in the `settings` file the same NIC keeps its legacy IRQ.

Run via `make test-msi`."""

import sys
import tempfile
import time
import re
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "tapes"))
from test_hello_rs_e2e import McastSocket  # noqa: E402
from test_idle_cpu_e2e import monitor_key  # noqa: E402
from test_scripts_settings_e2e import Guest, build_image, expect, expect_absent  # noqa: E402

GROUP, PORT = "230.42.42.98", 4298
MAC = bytes.fromhex("525400123471")
CLIENT = bytes.fromhex("02deadbeef81")
ETHERTYPE = bytes((0x88, 0xB6))
DISCOVER_REQUEST = 0x02


def boot(tmp: Path, name: str, files: dict) -> tuple[Guest, str]:
    image = tmp / f"{name}.img"
    build_image(image, files)
    monitor = str(tmp / f"{name}.mon")
    guest = Guest(image, [
        "-monitor", f"unix:{monitor},server,nowait",
        "-netdev", f"socket,id=n0,mcast={GROUP}:{PORT}",
        "-device", f"e1000e,netdev=n0,mac={MAC.hex(':')}",
    ])
    guest.read_until(b"Waiting for a remote terminal client to connect;", 30)
    monitor_key(monitor, "f12")  # leave the term net wait for the local prompt
    guest.read_until(b"fos> ", 15)
    return guest, monitor


def notifications(text: str) -> int:
    match = re.search(r"NIC (?:MSI vector 0x[0-9a-f]+|IRQ\d+): (\d+) notification", text)
    return int(match.group(1)) if match else -1


def main() -> None:
    with tempfile.TemporaryDirectory() as name:
        tmp = Path(name)

        guest, _ = boot(tmp, "msi", {})
        try:
            out = guest.run("nic list")
            expect(out, "Intel e1000e", "NIC MSI vector 0x")
            expect_absent(out, "NIC IRQ")
            first = notifications(out)
            if first < 1:
                raise SystemExit(f"the MSI self-test message was not counted:\n{out}")
            print("PASS: the e1000e comes up on an MSI vector after its self-test.")

            sender = McastSocket(GROUP, PORT)
            for _ in range(20):
                frame = bytes((0xFF,) * 6) + CLIENT + ETHERTYPE + bytes([DISCOVER_REQUEST])
                sender.send(frame.ljust(60, b"\0"))
                time.sleep(0.05)
            time.sleep(1)
            out = guest.run("nic list")
            later = notifications(out)
            if later <= first:
                raise SystemExit(f"traffic raised no MSI messages ({first} -> {later}):\n{out}")
            irq = guest.run("irqstat")
            expect(irq, "MSI vector 0x50:", "message(s) received")
            print(f"PASS: frames keep arriving as MSI messages ({first} -> {later} notifications).")
        finally:
            guest.terminate()

        guest, _ = boot(tmp, "nomsi", {"settings": b"no-msi\n"})
        try:
            out = guest.run("nic list")
            expect(out, "Intel e1000e", "NIC IRQ")
            expect_absent(out, "MSI vector")
            print("PASS: `no-msi` in the settings file keeps the NIC on its legacy IRQ.")
        finally:
            guest.terminate()
    print("PASS: NIC MSI works, is self-tested, and can be switched off.")


if __name__ == "__main__":
    main()
