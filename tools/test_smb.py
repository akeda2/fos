#!/usr/bin/env python3
"""The `fosplay` tape reading from a real SMB share (guest access) through QEMU's user-mode network: `fosplay ls` lists a
directory, `fosplay cat` reads a file to the end; the size and the FNV-1a checksum are compared with what `smbclient`
fetches (the checksum only when smbclient is installed). The server is `FOS_SMB_HOST` (default 192.168.0.94, the maintainer's
Samba), share `FOS_SMB_SHARE` (media), file `FOS_SMB_FILE`; the test passes with a SKIP line when the host's port 445 is not
reachable. Run with `make test-smb`."""
import os
import shutil
import socket
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_boot import check  # noqa: E402
from test_usb_audio import boot_with_commands  # noqa: E402

HOST = os.environ.get("FOS_SMB_HOST", "192.168.0.94")
SHARE = os.environ.get("FOS_SMB_SHARE", "media")
FILE = os.environ.get("FOS_SMB_FILE", "musik/01_-_three_pieces_for_lute_allemande_0gei.flac")


def fnv1a(data):
    value = 0x811C9DC5
    for byte in data:
        value = ((value ^ byte) * 0x01000193) & 0xFFFFFFFF
    return value


def main():
    try:
        socket.create_connection((HOST, 445), 2).close()
    except OSError:
        print(f"SKIP: no SMB server at {HOST}:445")
        return
    expected = None
    if shutil.which("smbclient"):
        with tempfile.TemporaryDirectory() as tmp:
            local = Path(tmp) / "file"
            subprocess.run(["smbclient", "-N", f"//{HOST}/{SHARE}", "-c", f"get {FILE} {local}"], check=True, capture_output=True)
            data = local.read_bytes()
            expected = (len(data), fnv1a(data))
    directory = FILE.rsplit("/", 1)[0] if "/" in FILE else ""
    output = boot_with_commands(["-netdev", "user,id=n0", "-device", "e1000,netdev=n0"],
                                [f"run fosplay ls {HOST} {SHARE} {directory}", f"run fosplay cat {HOST} {SHARE} {FILE}"])
    print(output[-1800:])
    name = FILE.rsplit("/", 1)[-1]
    check(name in output and "entries." in output, "ls lists the directory and shows the file", output[-1800:])
    check("fosplay: cat OK." in output, "cat reads the whole file", output[-1800:])
    if expected:
        size, digest = expected
        check(f"read {size} bytes" in output and f"fnv1a {digest:08x}" in output,
              f"size {size} and checksum {digest:08x} match smbclient's copy", output[-1800:])
    print("PASS: smb")


if __name__ == "__main__":
    main()
