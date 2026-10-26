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
from test_audio_net import Guest  # noqa: E402
from test_boot import check  # noqa: E402
from test_loopback import ask, float_wav  # noqa: E402
from test_record import fosfs_files  # noqa: E402
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
    # The host's name, when this machine resolves it to the same address: QEMU's DNS relays to ours.
    name = os.environ.get("FOS_SMB_NAME", "")
    if not name:
        try:
            name = socket.gethostbyaddr(HOST)[0]
            name = name if socket.gethostbyname(name) == HOST else ""
        except OSError:
            name = ""
    commands = [f"run fosplay ls {HOST} {SHARE} {directory}", f"run fosplay cat {HOST} {SHARE} {FILE}"]
    if name:
        commands.append(f"run fosplay ls {name} {SHARE} {directory}")
    output = boot_with_commands(["-netdev", "user,id=n0", "-device", "e1000,netdev=n0"], commands)
    print(output[-1800:])
    if name:
        octets = ", ".join(HOST.split("."))
        check(f"-> {name} (Ipv4Address([{octets}]))" in output and output.count("entries.") >= 2,
              f"the host name {name} resolves through DNS and lists the directory", output[-1200:])
    name = FILE.rsplit("/", 1)[-1]
    check(name in output and "entries." in output, "ls lists the directory and shows the file", output[-1800:])
    check("fosplay: cat OK." in output, "cat reads the whole file", output[-1800:])
    if expected:
        size, digest = expected
        check(f"read {size} bytes" in output and f"fnv1a {digest:08x}" in output,
              f"size {size} and checksum {digest:08x} match smbclient's copy", output[-1800:])
    play_directory(directory)
    print("PASS: smb")


def play_directory(directory):
    """`fosplay play HOST SHARE DIR` streams the directory's tracks into loop0 (the NIC is a second, user-mode one) while
    `record` captures the first seconds: the playlist is found, sorted and numbered, and what is recorded is sound."""
    with tempfile.TemporaryDirectory() as tmp:
        guest = Guest(tmp, "smbplay", "52:54:00:12:36:03", extra=["-netdev", "user,id=n1", "-device", "e1000,netdev=n1"])
        try:
            guest.wait_for("M3 complete")
            guest.wait_for("fos> ")
            ask(guest, "nic use 2", "Using NIC 2")
            guest.send("run record device=loop0 smb 8 &0")
            guest.pump(2)
            guest.send(f"run fosplay device=loop0 gfx=off play //{HOST}/{SHARE}/{directory} &2")
            guest.wait_for("Service record ended", 60)
            guest.pump(1)
            text = guest.text()
            check("[1/" in text and "FLAC" in text, "the directory playlist starts its first track", text[-1200:])
            guest.send("kill 2")
            guest.pump(1)
        finally:
            guest.stop()
        blob = fosfs_files(Path(tmp) / "smbplay.img").get("smb.wav")
        check(blob is not None, "smb.wav was written", guest.text()[-800:])
        samples, rate = float_wav(blob)
        loud = [x for x in samples if abs(x) > 0.02]
        check(len(loud) > rate, f"the streamed music was recorded ({len(loud)} loud samples)", guest.text()[-800:])


if __name__ == "__main__":
    main()
