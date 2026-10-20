#!/usr/bin/env python3
"""Pushes a kernel to a Fosfor OS machine over its console serial line (KUP over serial; protocol in kernel/src/kup_serial.rs).

    tools/send_kernel_serial.py /dev/ttyUSB3 build/cargo-target/aarch64-unknown-none-softfloat/release/fosfor-kernel

The shell must be at its prompt. The tool types `kup serial`, waits for `KUP serial: ready`, sends the image in 2 KiB frames
(each answered before the next), and reports the machine's verdict. The kernel is written to the inactive A/B slot and tried
once; the loader falls back by itself unless it reaches its milestone. Stdlib only."""
import argparse
import os
import select
import struct
import sys
import time
import zlib

MAGIC = b"\xf0\x5b"
CHUNK = 2048


def frame(kind: bytes, sequence: int, payload: bytes = b"") -> bytes:
    body = kind + struct.pack("<HH", sequence, len(payload)) + payload
    return MAGIC + body + struct.pack("<I", zlib.crc32(body))


class Reader:
    """Pulls frames out of a byte stream that also carries console text."""

    def __init__(self, read):
        self.read = read  # read(timeout) -> bytes
        self.buffer = b""
        self.text = b""

    def pump(self, timeout: float) -> None:
        self.buffer += self.read(timeout)

    def next(self, timeout: float):
        """The next good frame as (kind, sequence, payload), or None on timeout."""
        deadline = time.time() + timeout
        while True:
            at = self.buffer.find(MAGIC)
            if at >= 0 and len(self.buffer) >= at + 2 + 5:
                length = struct.unpack_from("<H", self.buffer, at + 5)[0]
                end = at + 2 + 5 + length + 4
                if len(self.buffer) >= end:
                    body = self.buffer[at + 2 : end - 4]
                    if zlib.crc32(body) == struct.unpack_from("<I", self.buffer, end - 4)[0]:
                        self.text += self.buffer[:at]
                        self.buffer = self.buffer[end:]
                        return body[:1], struct.unpack_from("<H", body, 1)[0], body[5:]
                    self.buffer = self.buffer[at + 1 :]  # not a frame after all: look further on
                    continue
            elif at < 0 and len(self.buffer) > 1:
                self.text += self.buffer[:-1]
                self.buffer = self.buffer[-1:]
            remaining = deadline - time.time()
            if remaining <= 0:
                return None
            self.pump(min(remaining, 0.2))


def push(read, write, image: bytes, log=print, command: bool = True, baud: int = 0, set_speed=None) -> int:
    """Runs the whole update over `read(timeout) -> bytes` / `write(bytes)`; returns the slot the kernel was written to.
    With `baud` the machine is asked to raise its UART speed (aarch64 shell) and `set_speed(baud)` switches this end."""
    reader = Reader(read)
    if command:
        write(b"\r")
        time.sleep(0.2)
        reader.pump(0.3)
        reader.buffer = b""
        write(b"kup serial %d\r" % baud if baud else b"kup serial\r")
    deadline = time.time() + 15
    while b"KUP serial: ready" not in reader.buffer + reader.text:
        if time.time() > deadline:
            raise SystemExit("no `KUP serial: ready` from the machine (is the shell at its prompt?)")
        reader.pump(0.2)
    reader.text = b""
    reader.buffer = reader.buffer.split(b"KUP serial: ready", 1)[-1]
    if baud and set_speed:
        time.sleep(0.02)
        set_speed(baud)
        reader.buffer = b""

    def exchange(kind: bytes, sequence: int, payload: bytes = b"", timeout: float = 1.0, attempts: int = 8):
        for _ in range(attempts):
            write(frame(kind, sequence, payload))
            while True:
                answer = reader.next(timeout)
                if answer is None:
                    break
                if answer[0] == b"A" and answer[1] == sequence:
                    return answer
                if answer[0] == b"F":
                    raise SystemExit("the machine refused: " + answer[2].decode(errors="replace"))
                if answer[0] == b"N":
                    break
        raise SystemExit(f"no answer to frame {sequence} after {attempts} tries")

    log(f"sending {len(image)} bytes (crc {zlib.crc32(image):08x})")
    exchange(b"S", 0, struct.pack("<II", len(image), zlib.crc32(image)))
    started = time.time()
    for number, offset in enumerate(range(0, len(image), CHUNK), start=1):
        exchange(b"D", number, image[offset : offset + CHUNK])
        if number % 64 == 0:
            log(f"  {min(offset + CHUNK, len(image))}/{len(image)} bytes")
    log(f"sent in {time.time() - started:.1f} s; waiting for the machine to verify and write it")
    write(frame(b"E", 0))
    end = time.time() + 120
    while time.time() < end:
        answer = reader.next(1.0)
        if answer is None:
            continue
        if answer[0] == b"K":
            return answer[1]
        if answer[0] == b"F":
            raise SystemExit("the machine refused: " + answer[2].decode(errors="replace"))
    raise SystemExit("no verdict from the machine")


def main() -> None:
    import termios

    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("port")
    parser.add_argument("kernel")
    parser.add_argument("--baud", type=int, default=115200)
    parser.add_argument("--fast", type=int, default=0, help="raise both ends to this speed for the transfer (aarch64 shell)")
    arguments = parser.parse_args()
    image = open(arguments.kernel, "rb").read()
    fd = os.open(arguments.port, os.O_RDWR | os.O_NOCTTY)
    attributes = termios.tcgetattr(fd)
    attributes[0] = attributes[1] = attributes[3] = 0
    attributes[2] = termios.CS8 | termios.CREAD | termios.CLOCAL

    def set_speed(baud: int) -> None:
        speed = getattr(termios, f"B{baud}")
        attributes[4] = attributes[5] = speed
        termios.tcsetattr(fd, termios.TCSADRAIN, attributes)

    set_speed(arguments.baud)

    def read(timeout: float) -> bytes:
        ready, _, _ = select.select([fd], [], [], timeout)
        return os.read(fd, 4096) if ready else b""

    def write(data: bytes) -> None:
        while data:
            data = data[os.write(fd, data) :]

    try:
        slot = push(read, write, image, baud=arguments.fast, set_speed=set_speed)
    finally:
        set_speed(arguments.baud)
    print(f"the machine verified the kernel and wrote it to slot {slot}; it restarts and tries it once.")


if __name__ == "__main__":
    sys.exit(main())
