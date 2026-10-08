#!/usr/bin/env python3
"""Builds the BIOS-path spike disk image (docs/shared-codebase.md, milestone B1).

A raw disk: stage one in sector 0 (with a protective-MBR partition entry at 446), stage two at LBA 64
(the BIOS boot gap of a GPT disk), the test kernel at LBA 1024. Stage two's header (offset 0x10:
kernel LBA u64, kernel sector count u32, destination u32) is patched here. Standard library only.
"""
import argparse
import struct
import sys
from pathlib import Path

SECTOR = 512
STAGE2_LBA, STAGE2_SECTORS = 64, 16        # keep equal to boot/bios/stage1.S
KERNEL_LBA = 1024
KERNEL_DEST = 0x100000
IMAGE_SECTORS = 8192                       # 4 MiB

parser = argparse.ArgumentParser()
parser.add_argument("--stage1", required=True, type=Path)
parser.add_argument("--stage2", required=True, type=Path)
parser.add_argument("--kernel", required=True, type=Path)
parser.add_argument("--output", required=True, type=Path)
arguments = parser.parse_args()

stage1 = bytearray(arguments.stage1.read_bytes())
stage2 = bytearray(arguments.stage2.read_bytes())
kernel = arguments.kernel.read_bytes()

if len(stage1) != SECTOR or stage1[510:512] != b"\x55\xaa":
    sys.exit(f"stage one must be exactly 512 bytes ending in 0x55aa (is {len(stage1)})")
if any(stage1[446:510]):
    sys.exit("stage one's code or data runs into the partition table (bytes 446-509)")
if len(stage2) > STAGE2_SECTORS * SECTOR:
    sys.exit(f"stage two is {len(stage2)} bytes; stage one loads only {STAGE2_SECTORS * SECTOR}")
kernel_sectors = (len(kernel) + SECTOR - 1) // SECTOR
if KERNEL_LBA + kernel_sectors > IMAGE_SECTORS:
    sys.exit("kernel does not fit the image")
header = struct.pack("<QII", KERNEL_LBA, kernel_sectors, KERNEL_DEST)
before = len(stage2)
stage2[0x10:0x10 + len(header)] = header      # same length on both sides: a mismatch would shift everything after it
assert len(stage2) == before, "patching stage two's header changed its length"

# A protective MBR entry (type 0xee) over the whole disk, as a GPT disk has: the BIOS still boots the code in sector 0.
stage1[446:462] = struct.pack("<BBBBBBBBII", 0, 0, 2, 0, 0xEE, 0xFF, 0xFF, 0xFF, 1, IMAGE_SECTORS - 1)

image = bytearray(IMAGE_SECTORS * SECTOR)
image[0:SECTOR] = stage1
image[STAGE2_LBA * SECTOR:STAGE2_LBA * SECTOR + len(stage2)] = stage2
image[KERNEL_LBA * SECTOR:KERNEL_LBA * SECTOR + len(kernel)] = kernel
arguments.output.parent.mkdir(parents=True, exist_ok=True)
arguments.output.write_bytes(image)
print(f"wrote {arguments.output}: stage 2 {len(stage2)} bytes at LBA {STAGE2_LBA}, kernel {len(kernel)} bytes at LBA {KERNEL_LBA}")
