#!/usr/bin/env python3
"""Generate the assembly constants shared by the image builder and loaders.

This is the single place that turns "how big is the kernel binary right
now" into the disk-layout constants (`layout.inc`) that both `boot/stage1.S`
and `boot/stage2.S` `.include`. Keeping this generated, rather than hand
edited in the assembly sources, means the loader's LBA/sector-count
constants can never silently drift out of sync with the actual kernel size
or `tools/build_image.py`'s partition layout.
"""

from argparse import ArgumentParser
from pathlib import Path

SECTOR_SIZE = 512
# LBA of the raw payload partition (stage two + kernel); see build_image.py.
PAYLOAD_START_LBA = 2048
# Stage two is always allocated this many 512-byte sectors, regardless of
# its actual assembled size, leaving headroom to grow without relayout.
STAGE2_SECTORS = 32
# Physical address stage two loads the kernel to, and the end of the
# reserved low-memory region the kernel itself must fit inside (see
# `kernel/linker.ld`).
KERNEL_LOAD_ADDRESS = 0x10000
KERNEL_RESERVED_END = 0x50000
IMAGE_SECTORS = 32768
# LBA and size of the dedicated FOSFS data partition, kept separate from the
# boot payload partition so formatting FOSFS can never touch loader code.
DATA_START_LBA = 4096
# Leave the final 33 sectors for the backup GPT header and entry array.
DATA_SECTORS = IMAGE_SECTORS - DATA_START_LBA - 33


def sector_count(path: Path) -> int:
    """Rounds a file's byte size up to whole 512-byte sectors."""
    size = path.stat().st_size
    return (size + SECTOR_SIZE - 1) // SECTOR_SIZE


def main() -> None:
    parser = ArgumentParser()
    parser.add_argument("--kernel", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    kernel_sectors = sector_count(args.kernel)
    max_kernel_sectors = (KERNEL_RESERVED_END - KERNEL_LOAD_ADDRESS) // SECTOR_SIZE
    if not 0 < kernel_sectors <= max_kernel_sectors:
        raise SystemExit(f"kernel must occupy between 1 and {max_kernel_sectors} sectors")
    kernel_end = KERNEL_LOAD_ADDRESS + kernel_sectors * SECTOR_SIZE
    if kernel_end > KERNEL_RESERVED_END:
        raise SystemExit("kernel exceeds the reserved low-memory load region")
    # Emitted as GNU `as` `.set` directives so boot/stage1.S and
    # boot/stage2.S can `.include` this file directly as assembly source.
    content = (
        f".set STAGE2_LBA, {PAYLOAD_START_LBA}\n"
        f".set STAGE2_SECTORS, {STAGE2_SECTORS}\n"
        f".set KERNEL_LBA, {PAYLOAD_START_LBA + STAGE2_SECTORS}\n"
        f".set KERNEL_SECTORS, {kernel_sectors}\n"
        f".set KERNEL_END, {KERNEL_RESERVED_END}\n"
        f".set IMAGE_SECTORS_LO, {IMAGE_SECTORS & 0xffffffff}\n"
        f".set IMAGE_SECTORS_HI, {IMAGE_SECTORS >> 32}\n"
        f".set DATA_START_LBA_LO, {DATA_START_LBA & 0xffffffff}\n"
        f".set DATA_START_LBA_HI, {DATA_START_LBA >> 32}\n"
        f".set DATA_SECTORS_LO, {DATA_SECTORS & 0xffffffff}\n"
        f".set DATA_SECTORS_HI, {DATA_SECTORS >> 32}\n"
    )
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(content, encoding="ascii")


if __name__ == "__main__":
    main()
