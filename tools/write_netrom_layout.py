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

# `boot/netrom.S` copies the whole embedded kernel with a single real-mode
# `rep movsb`, whose count register (CX) is 16 bits -- so, unlike the
# disk-boot path's 127-sector (~65 KiB) cap enforced across whole sectors,
# this is a hard per-byte ceiling.
MAX_KERNEL_BYTES = 65535


def main() -> None:
    parser = ArgumentParser()
    parser.add_argument("--kernel", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    kernel_size = args.kernel.stat().st_size
    if not 0 < kernel_size <= MAX_KERNEL_BYTES:
        raise SystemExit(
            f"kernel must be between 1 and {MAX_KERNEL_BYTES} bytes to be "
            "embedded in a NIC option ROM (it is copied with a single "
            "16-bit-counted `rep movsb`); use `make image` instead if it "
            "has grown past this"
        )
    content = f".set NETROM_KERNEL_SIZE, {kernel_size}\n"
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(content, encoding="ascii")


if __name__ == "__main__":
    main()
