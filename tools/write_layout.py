#!/usr/bin/env python3
"""Generate the assembly constants shared by the image builder and loaders."""

from argparse import ArgumentParser
from pathlib import Path

SECTOR_SIZE = 512
PAYLOAD_START_LBA = 2048
STAGE2_SECTORS = 32
KERNEL_LOAD_ADDRESS = 0x10000
KERNEL_RESERVED_END = 0x20000
IMAGE_SECTORS = 8192
DATA_START_LBA = 4096
DATA_SECTORS = 4096


def sector_count(path: Path) -> int:
    size = path.stat().st_size
    return (size + SECTOR_SIZE - 1) // SECTOR_SIZE


def main() -> None:
    parser = ArgumentParser()
    parser.add_argument("--kernel", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    kernel_sectors = sector_count(args.kernel)
    if not 0 < kernel_sectors <= 127:
        raise SystemExit("kernel must occupy between 1 and 127 sectors")
    kernel_end = KERNEL_LOAD_ADDRESS + kernel_sectors * SECTOR_SIZE
    if kernel_end > KERNEL_RESERVED_END:
        raise SystemExit("kernel exceeds the reserved low-memory load region")
    content = (
        f".set STAGE2_LBA, {PAYLOAD_START_LBA}\n"
        f".set STAGE2_SECTORS, {STAGE2_SECTORS}\n"
        f".set KERNEL_LBA, {PAYLOAD_START_LBA + STAGE2_SECTORS}\n"
        f".set KERNEL_SECTORS, {kernel_sectors}\n"
        f".set KERNEL_END, {KERNEL_RESERVED_END}\n"
        f".set IMAGE_SECTORS, {IMAGE_SECTORS}\n"
        f".set DATA_START_LBA, {DATA_START_LBA}\n"
        f".set DATA_SECTORS, {DATA_SECTORS}\n"
    )
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(content, encoding="ascii")


if __name__ == "__main__":
    main()
