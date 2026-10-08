#!/usr/bin/env python3
"""The RTL8139 driver (`kernel/src/rtl8139.rs`) in a v2 guest whose only NIC is QEMU's `-device rtl8139`.

Proved: the card is found at boot with its MAC and link up, its legacy-INTx interrupt path passes the timer self-test, and
frames flow both ways: `nic discover` against an e1000 guest in each direction (the RTL8139 sends and receives), a promiscuous
`nic sniff` of injected frames, and NETFS round trips with the host's `foscopy.py` of 37 KB and 300 KB (the latter wraps the
32 KiB receive ring many times and runs the four transmit slots round robin), compared byte for byte, plus a file fetched from
the e1000 guest. Run with `make test-v2-rtl8139`."""

import random
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_audio_net import GROUP, PORT, Guest, fosfs_add  # noqa: E402
from test_boot import CODE_CANDIDATES, IMAGE, VARS_CANDIDATES, check, find  # noqa: E402
from test_nic import arp_request, inject  # noqa: E402
from test_netfs import foscopy  # noqa: E402

RTL_MAC = "52:54:00:12:39:01"
E1000_MAC = "52:54:00:12:39:02"


class RtlGuest(Guest):
    """A guest on the multicast segment through an RTL8139 instead of the e1000 every other test uses."""

    def __init__(self, tmp, name, mac, files=None):
        code, variables = find("FOS_OVMF_CODE", CODE_CANDIDATES), find("FOS_OVMF_VARS", VARS_CANDIDATES)
        vars_copy = Path(tmp) / f"{name}-vars.fd"
        shutil.copy(variables, vars_copy)
        disk = Path(tmp) / f"{name}.img"
        shutil.copy(IMAGE, disk)
        if files:
            fosfs_add(disk, files)
        command = ["qemu-system-x86_64", "-machine", "q35", "-m", "512", "-display", "none", "-serial", "stdio", "-no-reboot",
                   "-drive", f"if=pflash,format=raw,readonly=on,file={code}", "-drive", f"if=pflash,format=raw,file={vars_copy}",
                   "-drive", f"format=raw,file={disk}",
                   "-netdev", f"socket,id=net0,mcast={GROUP}:{PORT}", "-device", f"rtl8139,netdev=net0,mac={mac}"]
        self.process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        self.raw = bytearray()
        self.name = name


def main():
    with tempfile.TemporaryDirectory() as tmp:
        work = Path(tmp)
        medium = bytes((index * 7 + index // 251) % 256 for index in range(37_000))
        large = random.Random(11).randbytes(300_000)
        (work / "medium.bin").write_bytes(medium)
        (work / "large.bin").write_bytes(large)
        r = RtlGuest(tmp, "rtl-r", RTL_MAC, files={"hostname": "realtek", "rtl.txt": "file on the rtl8139 machine\n"})
        b = Guest(tmp, "rtl-b", E1000_MAC, files={"hostname": "bravo", "bravo.txt": "file on bravo\n"})
        try:
            for guest in (r, b):
                guest.wait_for("M3 complete")
                guest.wait_for("fos> ")
            boot = r.text()
            check("rtl8139 at" in boot and RTL_MAC in boot and "link up" in boot, "the RTL8139 is found with its MAC and link up", boot[-1500:])
            check("initialisation failed" not in boot, "and initialises", boot[-1500:])
            check("interrupts: Intx" in boot and "self-test interrupt arrived" in boot, "its legacy INTx path passes the timer self-test", boot[-1500:])

            mark = r.send("nic")
            r.pump(1.0)
            text = r.text()[mark:]
            check("rtl8139" in text and "link up" in text and "interrupts: INTx pin" in text and "0 bad frames" in text, "nic lists it with its driver line", text[-700:])
            mark = r.send("nic regs")
            r.pump(1.0)
            check("RTL8139 I/O" in r.text()[mark:] and "RCR" in r.text()[mark:], "nic regs dumps its registers", r.text()[mark:][-500:])

            # Both directions against an e1000 guest.
            mark = r.send("nic discover")
            r.wait_for("fos> ", 30, mark)
            check(E1000_MAC in r.text()[mark:] and "bravo1" in r.text()[mark:], "the RTL8139 sends a probe and hears the reply", r.text()[mark:][-500:])
            mark = b.send("nic discover")
            b.wait_for("fos> ", 30, mark)
            check(RTL_MAC in b.text()[mark:] and "realtek1" in b.text()[mark:], "and answers the other machine's probe by name", b.text()[mark:][-500:])

            # Promiscuous mode: frames for someone else's address.
            mark = r.send("nic sniff")
            r.wait_for("Sniffing NIC 1", 30, mark)
            r.pump(0.5)
            inject(bytes.fromhex("525400ddeeff") + arp_request(bytes.fromhex("525400aabbcc"), bytes([10, 1, 2, 4]), bytes([10, 1, 2, 3]))[6:])  # unicast to a third machine
            r.pump(1.5)
            r.process.stdin.write(b"\x1b")
            r.process.stdin.flush()
            r.wait_for("Stopped:", 15, mark)
            check("ARP who has 10.1.2.3? tell 10.1.2.4" in r.text()[mark:], "promiscuous nic sniff shows a frame not addressed to it", r.text()[mark:][-600:])
            r.wait_for("fos> ", 10, mark)

            # NETFS with the host, and with the other guest.
            result = foscopy(["--mac", RTL_MAC, "medium.bin", ":disk1/medium.bin"], work)
            check(result.returncode == 0, "a 37,000-byte file is stored through the RTL8139", result.stderr[-600:])
            result = foscopy(["--mac", RTL_MAC, ":disk1/medium.bin", "medium.back"], work)
            check(result.returncode == 0 and (work / "medium.back").read_bytes() == medium, "and read back byte for byte", result.stderr[-600:])
            result = foscopy(["--mac", RTL_MAC, "large.bin", ":disk1/large.bin"], work)
            check(result.returncode == 0, "300,000 bytes of random data are stored (the receive ring wraps many times)", result.stderr[-600:])
            result = foscopy(["--mac", RTL_MAC, ":disk1/large.bin", "large.back"], work)
            check(result.returncode == 0 and (work / "large.back").read_bytes() == large, "and read back byte for byte (the four transmit slots round robin)", result.stderr[-600:])

            mark = r.send("copy bravo1/bravo.txt disk1")
            r.wait_for("fos> ", 30, mark)
            check("Copied bravo.txt" in r.text()[mark:], "copy HOST/NAME fetches a file from the e1000 guest", r.text()[mark:][-400:])
            mark = b.send("copy disk1 bravo.txt realtek1 bravo-copy.txt")
            b.wait_for("fos> ", 30, mark)
            check("Copied bravo.txt to realtek1" in b.text()[mark:], "and the e1000 guest stores a file on the RTL8139 machine", b.text()[mark:][-400:])
            mark = r.send("cat bravo-copy.txt")
            r.pump(1.0)
            check("file on bravo" in r.text()[mark:], "which has it", r.text()[mark:][-300:])

            mark = r.send("nic")
            r.pump(1.0)
            text = r.text()[mark:]
            check("0 bad frames" in text and "0 recoveries" in text, "after all that traffic the driver reports no bad frames and no recoveries", text[-700:])
        finally:
            for guest in (r, b):
                guest.stop()
    print("v2 rtl8139: all checks passed")


if __name__ == "__main__":
    main()
