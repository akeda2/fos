#!/usr/bin/env python3
"""Boots the server kernel on a tap device so the host's Linux `aoe` module can use the disk it exports
(docs/block-sharing.md, "Testing against Linux"). A manual tool, not a test: the root steps (making the tap device, loading
the module, reading and writing /dev/etherd) are for a person to run, and the guide below prints them.

What it does without root:
  - makes a scratch data disk (random bytes, default 64 MiB) in build/aoe-linux/ and writes its SHA-256 beside it;
  - builds a boot image whose settings export disk 2 read-only as shelf 5 slot 2 (`aoe-shelf 5`, `aoe-export 2`);
  - boots QEMU (KVM unless --no-kvm) with that disk and an e1000 on the tap device, the guest shell on this terminal
    (Ctrl-A X quits QEMU).
The data disk is a file the script made: no host block device is ever handed to the guest.

    make run-mf-aoe-tap            (or: python3 tools/aoe_linux.py [--tap fosaoe0] [--size-mib 64] [--fresh] [--no-kvm])
Needs LOADER_EFI, KERNEL_MF_ELF, BIOS_STAGE1 and BIOS_STAGE2 (the Makefile target sets them)."""
import argparse
import hashlib
import os
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CODE = next(p for p in ["/usr/share/OVMF/OVMF_CODE_4M.fd", "/usr/share/OVMF/OVMF_CODE.fd",
                        "/usr/share/edk2/ovmf/OVMF_CODE.fd"] if Path(p).exists())
GUEST_MAC = "52:54:00:12:34:d5"


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--tap", default="fosaoe0")
    parser.add_argument("--size-mib", type=int, default=64)
    parser.add_argument("--fresh", action="store_true", help="make a new data disk even if one exists")
    parser.add_argument("--no-kvm", action="store_true")
    args = parser.parse_args()

    if not Path(f"/sys/class/net/{args.tap}").exists():
        print(f"There is no network interface {args.tap}. Make it once (root), owned by you so QEMU needs no root:\n")
        print(f"    sudo ip tuntap add dev {args.tap} mode tap user $USER")
        print(f"    sudo ip link set {args.tap} up\n")
        print("then run this again. docs/block-sharing.md has the whole procedure.")
        return 1

    work = ROOT / "build" / "aoe-linux"
    work.mkdir(parents=True, exist_ok=True)
    data = work / "data.img"
    size = args.size_mib << 20
    if args.fresh or not data.exists() or data.stat().st_size != size:
        print(f"Making a {args.size_mib} MiB scratch disk of random bytes: {data}")
        with open(data, "wb") as out:
            for _ in range(args.size_mib):
                out.write(os.urandom(1 << 20))
    digest = hashlib.sha256(data.read_bytes()).hexdigest()
    (work / "data.sha256").write_text(f"{digest}  data.img\n")

    settings = work / "settings"
    settings.write_text("aoe-shelf 5\naoe-export 2\n")
    boot = work / "boot.img"
    subprocess.run([sys.executable, str(ROOT / "tools/build_image.py"), "--loader", os.environ["LOADER_EFI"],
                    "--kernel", os.environ["KERNEL_MF_ELF"], "--output", str(boot), "--bios-stage1", os.environ["BIOS_STAGE1"],
                    "--bios-stage2", os.environ["BIOS_STAGE2"], "--file", f"settings={settings}"], check=True, capture_output=True)

    print(f"""
Disk 2 of the guest is {data}
  {args.size_mib} MiB, {size // 512} sectors, SHA-256 {digest}
It is exported read-only as shelf 5 slot 2: Linux will call it /dev/etherd/e5.2.
In the guest shell below: `aoe` shows the export and its counters; for the write tests
  aoe unexport 2
  aoe export 2 rw confirm
The commands for the host are in docs/block-sharing.md, "Testing against Linux".
Ctrl-A X quits QEMU.
""")
    command = ["qemu-system-x86_64", "-cpu", "max", "-machine", "q35", "-m", "1024", "-smp", "2", "-display", "none",
               "-serial", "mon:stdio", "-no-reboot",
               "-drive", f"if=pflash,format=raw,readonly=on,file={CODE}", "-drive", f"format=raw,file={boot},snapshot=on",
               "-drive", f"format=raw,file={data}",
               "-netdev", f"tap,id=net0,ifname={args.tap},script=no,downscript=no", "-device", f"e1000,netdev=net0,mac={GUEST_MAC}"]
    if not args.no_kvm:
        command.insert(1, "-enable-kvm")
    os.execvp(command[0], command)


if __name__ == "__main__":
    sys.exit(main())
