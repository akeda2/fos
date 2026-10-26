#!/usr/bin/env python3
"""The shared TCP client (`tape_ip::tcp::Socket`) end to end: `netcheck HOST tcp=PORT BYTES` on a QEMU user-mode NIC
(DHCP, host at 10.0.2.2) fetches a few MB from a host server that answers "GET N" with N bytes (byte i = i % 251). Checks
the handshake, every byte, the throughput report and a second connection straight afterwards. Run with `make test-tcp`."""
import socket
import sys
import threading
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from testslot import port  # noqa: E402
from test_boot import check  # noqa: E402
from test_usb_audio import boot_with_commands  # noqa: E402

PORT = port(23456)
SIZE = 3_000_000


def serve(listener):
    while True:
        connection, _ = listener.accept()
        with connection:
            line = b""
            while not line.endswith(b"\n"):
                chunk = connection.recv(64)
                if not chunk:
                    break
                line += chunk
            if line.startswith(b"GET "):
                count = int(line[4:])
                pattern = bytes(i % 251 for i in range(251 * 64))
                data = (pattern * (count // len(pattern) + 1))[:count]
                try:
                    connection.sendall(data)
                except OSError:
                    pass


def main():
    listener = socket.socket()
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind(("127.0.0.1", PORT))
    listener.listen(2)
    threading.Thread(target=serve, args=(listener,), daemon=True).start()
    output = boot_with_commands(
        ["-netdev", "user,id=n0", "-device", "e1000,netdev=n0"],
        [f"run netcheck 10.0.2.2 tcp={PORT} {SIZE}", f"run netcheck 10.0.2.2 tcp={PORT} 1000"])
    print(output[-1500:])
    check(f"netcheck: tcp: {SIZE} bytes in" in output and "0 wrong." in output, f"{SIZE} bytes arrived intact", output[-1500:])
    check(output.count("netcheck: tcp: OK.") == 2, "a second connection works straight after the first", output[-1500:])
    print("PASS: tcp")


if __name__ == "__main__":
    main()
