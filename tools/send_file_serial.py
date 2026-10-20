#!/usr/bin/env python3
"""Sends one file to a Fosfor OS machine over its console serial line (the aarch64 shell's `recv`; framing as in
kernel/src/kup_serial.rs and tools/send_kernel_serial.py).

    tools/send_file_serial.py /dev/ttyUSB3 build/fosfor-aarch64/tapes/clock clock --fast 921600

The shell must be at its prompt. The tool types `recv NAME`, waits for `KUP serial: ready`, sends the file in 2 KiB frames and
reports the machine's verdict. NAME may be `diskN/NAME`. Send the stamped copy (`build/fosfor-aarch64/tapes/NAME`) for a tape.
Stdlib only."""
import argparse
import os
import select
import sys
import termios
import zlib
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import send_kernel_serial as kup  # noqa: E402


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("port")
    parser.add_argument("file")
    parser.add_argument("name", help="the file's name on the machine (NAME or diskN/NAME)")
    parser.add_argument("--baud", type=int, default=115200)
    parser.add_argument("--fast", type=int, default=0, help="raise both ends to this speed for the transfer")
    arguments = parser.parse_args()
    data = Path(arguments.file).read_bytes()
    fd = os.open(arguments.port, os.O_RDWR | os.O_NOCTTY)
    attributes = termios.tcgetattr(fd)
    attributes[0] = attributes[1] = attributes[3] = 0
    attributes[2] = termios.CS8 | termios.CREAD | termios.CLOCAL

    def set_speed(baud: int) -> None:
        attributes[4] = attributes[5] = getattr(termios, f"B{baud}")
        termios.tcsetattr(fd, termios.TCSADRAIN, attributes)

    set_speed(arguments.baud)

    def read(timeout: float) -> bytes:
        ready, _, _ = select.select([fd], [], [], timeout)
        return os.read(fd, 4096) if ready else b""

    def write(chunk: bytes) -> None:
        while chunk:
            chunk = chunk[os.write(fd, chunk) :]

    try:
        kup.push(read, write, data, baud=arguments.fast, set_speed=set_speed, shell_command=b"recv " + arguments.name.encode())
    finally:
        set_speed(arguments.baud)
    print(f"the machine verified {len(data)} bytes (crc {zlib.crc32(data):08x}) and wrote them as {arguments.name}.")


if __name__ == "__main__":
    sys.exit(main())
