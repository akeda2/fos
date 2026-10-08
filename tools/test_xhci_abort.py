#!/usr/bin/env python3
"""xHCI command abort: a command that never completes is aborted and the command ring carries on behind it.

`usb abort-test` (a hidden hook) queues a command the controller never accepts (wrong cycle bit), lets it time out, runs the
abort sequence of xHCI 4.6.1.2 (CRCR.CA, wait for CRR to clear, move the dequeue pointer past the dead TRB) and then
executes two real No-Op commands. Before the abort existed, a command that timed out on real hardware (an Address Device
behind a SuperSpeed hub) left the controller unable to do anything else. QEMU cannot make a command hang, so this checks the
recovery path's pointer arithmetic, not a busy controller. Run with `make test-xhci-abort`."""
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_boot import check  # noqa: E402
from test_keyboard import boot  # noqa: E402


def session(send, wait_for):
    pass


def main():
    import subprocess  # noqa: F401
    from test_usb_replug import Machine  # noqa: E402
    import tempfile, shutil
    from test_boot import IMAGE
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        boot_image = tmp / "boot.img"
        shutil.copy(IMAGE, boot_image)
        stick = tmp / "stick.img"
        stick.write_bytes(bytes(8 << 20))
        machine = Machine(tmp, boot_image, stick, "qemu-xhci")
        try:
            machine.wait_for("fos> ", 90)
            output = machine.run("usb abort-test", 60)
            check("command ring was aborted and restarted" in output, "a timed-out command aborts and restarts the command ring", output)
            check("passed (the ring carries on)" in output, "and the ring then runs ordinary commands", output)
            disks = machine.run("disks")
            check("disk2" in disks, "the stick behind the controller still works afterwards", disks)
            listing = machine.run("ls disk2")
            check("disk2" in listing or "empty" in listing or "FOSFS" in listing, "and can be read", listing)
        finally:
            machine.close()
    print("xhci abort: all checks passed")


if __name__ == "__main__":
    main()
