#!/usr/bin/env python3
"""USB hubs behind the xHCI controller (the v2 hub driver, `usb_hub.rs`).

Three guests on QEMU's `qemu-xhci`, each with the PS/2 controller off so a key can only
arrive through USB:
1. a hub on root port 1 with a keyboard on its port 2 (route string 2): the keyboard is
   found behind the hub, named by its path `1.2`, and typing reaches the shell;
2. a hub with a USB stick on its port 3: the stick becomes a disk (bulk transfers through
   the hub, with a route string and the transaction-translator fields in the slot context);
3. a hub behind a hub (tier two, route string 0x41...) with a keyboard at the end: `1.1.4`.

(QEMU numbers its USB 2 ports after the four USB 3 ones, so `port=1` shows up as root port 5.)
QEMU emulates the hub's ports but not a real transaction translator, so split-transaction
timing stays a real-hardware matter. Run with `make test-hub`."""

import re
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_boot import check  # noqa: E402
from test_keyboard import boot, ps2_session  # noqa: E402


def main():
    xhci = ["-device", "qemu-xhci,id=xhci"]
    text = boot("q35,i8042=off", ps2_session, xhci + ["-device", "usb-hub,bus=xhci.0,port=1", "-device", "usb-kbd,bus=xhci.0,port=1.2"])
    check(re.search(r"USB: port \d+: a hub", text) and "ports, powering them" in text, "a hub on a root port is recognised and its ports powered", text)
    check(re.search(r"USB keyboard .* on port \d+\.2 of xHCI", text), "a keyboard behind the hub is found, with its path (root port.2)", text)
    check("Unknown command: xz" in text, "and typing on it reaches the shell", text)

    with tempfile.TemporaryDirectory() as tmp:
        stick = Path(tmp) / "stick.img"
        stick.write_bytes(bytes(8 << 20))
        text = boot("q35,i8042=off", lambda send, wait_for: None, xhci + [
            "-device", "usb-hub,bus=xhci.0,port=1",
            "-drive", f"if=none,id=stick,format=raw,file={stick}",
            "-device", "usb-storage,bus=xhci.0,port=1.3,drive=stick"])
    check(re.search(r"USB storage .* on port \d+\.3 of xHCI", text), "a USB stick behind the hub becomes a disk", text)

    text = boot("q35,i8042=off", ps2_session, xhci + [
        "-device", "usb-hub,bus=xhci.0,port=1", "-device", "usb-hub,bus=xhci.0,port=1.1",
        "-device", "usb-kbd,bus=xhci.0,port=1.1.4"])
    check(re.search(r"USB keyboard .* on port \d+\.1\.4 of xHCI", text), "a keyboard behind two hubs is found (path root.1.4)", text)
    check("Unknown command: xz" in text, "and typing on it reaches the shell", text)
    print("usb hub: all checks passed")


if __name__ == "__main__":
    main()
