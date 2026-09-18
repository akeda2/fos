#!/usr/bin/env python3
"""One-off end-to-end test for the `reset` command: boot the image, type
`reset`, and confirm the machine actually reboots -- stage two's own
"FOS stage 2: real mode active." banner (and everything after it) appears
a second time on the same serial stream, with no new QEMU process
involved.

Not part of `make test`: unlike `tools/qemu_smoke.py`'s single boot-and-
command-sequence run (which ends with `halt`, matching the assumption
that the machine never comes back), this needs `-no-reboot` *absent* so
QEMU actually honors the 8042 pulse-reset `reset` now performs (see
`arch::x86::reset` in kernel/src/arch.rs) instead of exiting the guest.
"""

from pathlib import Path
import os
import selectors
import subprocess
import sys
import time

IMAGE = Path(__file__).resolve().parent.parent / "build" / "fos.img"
BOOT_MARKER = b"FOS stage 2: real mode active."
PROMPT = b"fos> "


def main() -> None:
    if not IMAGE.exists():
        raise SystemExit(f"{IMAGE} does not exist; run `make image` first.")

    qemu = subprocess.Popen(
        ["qemu-system-i386", "-drive", f"format=raw,file={IMAGE}",
         "-serial", "stdio", "-monitor", "none", "-display", "none"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )
    assert qemu.stdin is not None and qemu.stdout is not None
    output = bytearray()
    selector = selectors.DefaultSelector()
    selector.register(qemu.stdout, selectors.EVENT_READ)

    def read_until(marker: bytes, timeout: float) -> None:
        deadline = time.monotonic() + timeout
        while marker not in output:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise SystemExit(f"timed out waiting for {marker!r}; output so far:\n{output.decode('utf-8', 'replace')}")
            for key, _ in selector.select(remaining):
                chunk = os.read(key.fd, 4096)
                if not chunk:
                    raise SystemExit(f"QEMU serial stream ended early:\n{output.decode('utf-8', 'replace')}")
                output.extend(chunk)

    try:
        # First boot.
        read_until(BOOT_MARKER, 15)
        read_until(PROMPT, 15)
        boot_count_markers = output.count(BOOT_MARKER)
        assert boot_count_markers == 1, f"expected exactly one boot banner before 'reset', saw {boot_count_markers}"
        del output[:]

        qemu.stdin.write(b"reset\r")
        qemu.stdin.flush()

        # Second boot, after the 8042 pulse-reset fires.
        read_until(BOOT_MARKER, 15)
        read_until(PROMPT, 15)
        print(output.decode("utf-8", "replace"))
        print("PASS: 'reset' rebooted the machine (stage two's banner reappeared).", file=sys.stderr)
    finally:
        qemu.terminate()
        try:
            qemu.wait(timeout=5)
        except subprocess.TimeoutExpired:
            qemu.kill()


if __name__ == "__main__":
    main()
