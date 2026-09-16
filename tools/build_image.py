#!/usr/bin/env python3
"""Create a fixed-layout MBR disk image without touching host block devices.

Produces `build/fos.img`: a 4 MiB raw disk with a classic MBR partition
table describing two partitions -- a raw "boot payload" partition (stage
two loader + kernel, active/bootable) and a separate FOSFS data partition,
which this script also preformats with a tiny FOSFS filesystem containing
one `README.TXT`. See the top-level README's "Disk image layout" section
for the full physical layout diagram.
"""

from argparse import ArgumentParser
from pathlib import Path

SECTOR_SIZE = 512
PAYLOAD_START_LBA = 2048
STAGE2_SECTORS = 32
MIN_IMAGE_SECTORS = 8192
DATA_START_LBA = 4096
DATA_SECTORS = 4096
# FOSFS reserves one superblock sector plus this many fixed directory
# sectors (64 entries total, 32 bytes each) before file data begins.
DIRECTORY_SECTORS = 4
DATA_START = 1 + DIRECTORY_SECTORS


def preformat_fosfs(image: bytearray) -> None:
    """Writes a ready-to-use FOSFS filesystem into the data partition.

    Lays down the superblock (magic, version, geometry), a single
    directory entry for `README.TXT`, and that file's contents, so a fresh
    image boots straight into a usable filesystem without requiring a
    `format` first. See `kernel/src/fosfs.rs` for the on-disk layout this
    mirrors.
    """
    offset = DATA_START_LBA * SECTOR_SIZE
    superblock = bytearray(SECTOR_SIZE)
    superblock[:8] = b"FOSFS1\0\0"
    superblock[8:10] = (1).to_bytes(2, "little")
    superblock[10:12] = (32).to_bytes(2, "little")
    superblock[12:16] = (1).to_bytes(4, "little")
    superblock[16:20] = DIRECTORY_SECTORS.to_bytes(4, "little")
    superblock[20:24] = DATA_START.to_bytes(4, "little")
    superblock[24:28] = (DATA_SECTORS - DATA_START).to_bytes(4, "little")
    image[offset:offset + SECTOR_SIZE] = superblock

    readme = b"Welcome to FOSFS.\nUse ls, cat README.TXT, and write NAME TEXT.\n"
    entry = bytearray(32)
    entry[0] = 1
    entry[1] = len(b"README.TXT")
    entry[4:8] = DATA_START.to_bytes(4, "little")
    entry[8:12] = len(readme).to_bytes(4, "little")
    entry[12:16] = (1).to_bytes(4, "little")
    entry[16:16 + len(b"README.TXT")] = b"README.TXT"
    directory_offset = offset + SECTOR_SIZE
    image[directory_offset:directory_offset + 32] = entry
    file_offset = offset + DATA_START * SECTOR_SIZE
    image[file_offset:file_offset + len(readme)] = readme


def pad(data: bytes, sectors: int) -> bytes:
    """Right-pads `data` with zero bytes to fill exactly `sectors` sectors."""
    if len(data) > sectors * SECTOR_SIZE:
        raise ValueError(f"artifact exceeds its {sectors}-sector allocation")
    return data.ljust(sectors * SECTOR_SIZE, b"\0")


def main() -> None:
    parser = ArgumentParser()
    parser.add_argument("--stage1", type=Path, required=True)
    parser.add_argument("--stage2", type=Path, required=True)
    parser.add_argument("--kernel", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    stage1 = args.stage1.read_bytes()
    if len(stage1) != SECTOR_SIZE or stage1[510:] != b"\x55\xaa":
        raise SystemExit("stage 1 must be exactly one boot-signature-bearing sector")
    stage2 = pad(args.stage2.read_bytes(), STAGE2_SECTORS)
    kernel = args.kernel.read_bytes()
    kernel_sectors = (len(kernel) + SECTOR_SIZE - 1) // SECTOR_SIZE
    if not 0 < kernel_sectors <= 127:
        raise SystemExit("kernel must occupy between 1 and 127 sectors")

    image = bytearray(MIN_IMAGE_SECTORS * SECTOR_SIZE)
    image[:SECTOR_SIZE] = stage1
    partition_offset = 446
    payload_sectors = STAGE2_SECTORS + kernel_sectors
    # Two classic 16-byte MBR partition table entries. Byte 0 is the boot
    # flag (0x80 = active/bootable), byte 4 is the partition type (0x02,
    # arbitrarily chosen to mean "fos raw payload"/"fos FOSFS data" -- no
    # real OS needs to recognize these types), bytes 1-3 and 5-7 are
    # legacy CHS start/end fields that BIOS EDD/LBA reads ignore, so they
    # are filled with the conventional "ignore me" sentinel 0xda,0xff,0xff,
    # followed by the LBA start and sector count that this loader actually
    # uses.
    image[partition_offset:partition_offset + 16] = bytes([
        0x80, 0, 2, 0, 0xda, 0xff, 0xff, 0xff,
    ]) + PAYLOAD_START_LBA.to_bytes(4, "little") + payload_sectors.to_bytes(4, "little")
    image[partition_offset + 16:partition_offset + 32] = bytes([
        0, 0, 2, 0, 0xda, 0xff, 0xff, 0xff,
    ]) + DATA_START_LBA.to_bytes(4, "little") + DATA_SECTORS.to_bytes(4, "little")
    image[PAYLOAD_START_LBA * SECTOR_SIZE:(PAYLOAD_START_LBA + STAGE2_SECTORS) * SECTOR_SIZE] = stage2
    kernel_start = (PAYLOAD_START_LBA + STAGE2_SECTORS) * SECTOR_SIZE
    image[kernel_start:kernel_start + len(kernel)] = kernel
    preformat_fosfs(image)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_bytes(image)


if __name__ == "__main__":
    main()

