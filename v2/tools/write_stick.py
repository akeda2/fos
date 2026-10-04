#!/usr/bin/env python3
"""Writes a v2 image to a USB stick (what `dd` does, with the guard rails).

    sudo python3 v2/tools/write_stick.py /dev/sdX [--image build/v2/v2.img]

`dd if=build/v2/v2.img of=/dev/sdX bs=4M conv=fsync status=progress` does exactly the same
copy; this script only refuses the mistakes that cost people their disks: it will not touch
anything that is mounted, not a whole disk, not removable, smaller than the image, or
larger than 256 GiB (a stick, not a system drive), and it shows what it found and asks you
to type the device name back. It never picks a device itself. Destructive by design: the
stick's old contents are gone.

The image is a complete GPT disk (EFI System Partition with BOOTX64.EFI, the fos payload with
the two kernel slots, and the FOSFS data partition). Its backup GPT sits at the end of the
*image* (96 MiB in), not at the end of a bigger stick: UEFI firmware reads the primary
table and does not mind; Linux tools warn, and `sgdisk -e DEVICE` moves it if you care."""

import argparse
import os
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import build_image  # noqa: E402


def sysfs(device: str, name: str) -> str:
    try:
        return Path(f"/sys/block/{device}/{name}").read_text().strip()
    except OSError:
        return ""


def mounted_partitions(device: str) -> list[str]:
    found = []
    for line in Path("/proc/mounts").read_text().splitlines():
        source = line.split()[0]
        if source.startswith(f"/dev/{device}"):
            found.append(source)
    return found


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("device", help="the whole-disk device node of the stick, e.g. /dev/sdb")
    parser.add_argument("--image", default="build/v2/v2.img", type=Path)
    parser.add_argument("--keep-guids", action="store_true",
                        help="write the image's GUIDs as they are instead of fresh random ones (two sticks written "
                             "that way cannot be told apart by the kernel's boot-disk discovery)")
    arguments = parser.parse_args()

    device = Path(arguments.device)
    name = device.name
    if not arguments.image.exists():
        return fail(f"{arguments.image} does not exist; run `make v2-image` first")
    if not device.exists() or not Path(f"/sys/block/{name}").exists():
        return fail(f"{device} is not a whole-disk block device (give /dev/sdX, not a partition like /dev/sdX1)")
    size = int(sysfs(name, "size") or 0) * 512
    image_size = arguments.image.stat().st_size
    if sysfs(name, "removable") != "1":
        return fail(f"{device} is not flagged removable by the kernel; refusing (a USB stick should be). Use dd yourself if you are sure.")
    if size < image_size:
        return fail(f"{device} holds {size >> 20} MiB, the image is {image_size >> 20} MiB")
    if size > 256 << 30:
        return fail(f"{device} is {size >> 30} GiB: too big to be a stick; refusing")
    mounted = mounted_partitions(name)
    if mounted:
        return fail(f"{', '.join(mounted)} is mounted; unmount it first")
    if os.geteuid() != 0:
        return fail("run as root (sudo): writing a block device needs it")

    vendor, model = sysfs(name, "device/vendor"), sysfs(name, "device/model")
    print(f"Device : {device}  {vendor} {model}  {size / 1e9:.1f} GB, removable")
    print(f"Image  : {arguments.image}  {image_size >> 20} MiB")
    print(f"This ERASES everything on {device}. Type the device name ({name}) to go on: ", end="", flush=True)
    if sys.stdin.readline().strip() != name:
        return fail("not confirmed; nothing written")

    data = bytearray(arguments.image.read_bytes())
    if not arguments.keep_guids:
        # Every stick gets its own GUIDs: the kernel finds the disk it booted from by the payload
        # partition's, and two sticks from one image would otherwise be indistinguishable.
        guid = build_image.randomize_guids(data)
        print(f"Payload partition GUID for this stick: {guid}")
    copied = 0
    with open(device, "wb", buffering=0) as target:
        for at in range(0, len(data), 4 << 20):
            chunk = data[at:at + (4 << 20)]
            target.write(chunk)
            copied += len(chunk)
            print(f"\r{copied >> 20} / {image_size >> 20} MiB", end="", flush=True)
        os.fsync(target.fileno())
    print(f"\nWritten and synced. Safe to remove {device} once `sync` returns.")
    return 0


def fail(message: str) -> int:
    print(f"write_stick: {message}", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())
