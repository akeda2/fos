#!/usr/bin/env python3
"""Generate the assembly constant `boot/netrom.S` needs for its embedded
kernel copy.

Mirrors `tools/write_layout.py`'s role for the disk-boot path: turns "how
big is the kernel binary right now" into a `.set` constant
(`netrom_layout.inc`) that `boot/netrom.S` `.include`s, so the ROM's
`rep movsb` byte count and BootInfo `kernel_end` field can never silently
drift out of sync with the actual kernel size.
"""

from argparse import ArgumentParser
from pathlib import Path

# `boot/netrom.S` copies the embedded kernel with a 32-bit-address-size
# `rep movsb`. The destination still shares the disk boot path's reserved
# low-memory window, so do not allow a kernel beyond 0x10000..0x60000.
MAX_KERNEL_BYTES = 0x60000 - 0x10000


def main() -> None:
    parser = ArgumentParser()
    parser.add_argument("--kernel", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    kernel_size = args.kernel.stat().st_size
    if not 0 < kernel_size <= MAX_KERNEL_BYTES:
        raise SystemExit("kernel must fit in the reserved 0x10000..0x60000 load region")
    content = f".set NETROM_KERNEL_SIZE, {kernel_size}\n"
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(content, encoding="ascii")


if __name__ == "__main__":
    main()
