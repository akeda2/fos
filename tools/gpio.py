#!/usr/bin/env python3
"""Reads and drives the GPIO pins of a Raspberry Pi running fos, from the PC (the host-side twin of the shell's `gpio @HOST`):

    gpio.py --iface IF --mac HOST [list|read N|set N 0|1|mode N in|out 0|1|alt0..alt5|pull N none|up|down]

HOST is a name (`pi1`: the hostname file plus the NIC number), a MAC address or a peer number, as foscopy.py takes it.
Every request is signed with the `synckey` in the repository root (or `--key FILE`; the Pi must have the same one) after a
challenge from the Pi, so a recorded frame cannot be replayed; the Pi answers only at its shell prompt (not while a tape
runs), and changes pins only when its `settings` file says `gpio-remote`. Wire format: kernel/src/gpio_remote.rs. Needs
CAP_NET_RAW (the `python3-netraw` copy). Real pins can only be checked on the Pi; `make test-gpio` runs this against a
simulated bank."""
import argparse
import hashlib
import hmac
import struct
import time
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import foscopy  # noqa: E402
from tape_manifest import KEY_FILE, read_key  # noqa: E402

TAG_REQUEST, TAG_REPLY = 0x41, 0x42
OP_STATE, OP_SET, OP_FUNCTION, OP_PULL = 1, 2, 3, 4
STATUS_TEXT = {
    0: "done",
    1: "the request was not understood",
    2: "that machine has no GPIO pins",
    3: "writes are not enabled there (`gpio-remote` in its settings file)",
    4: "that pin (or one of them) is not allowed or not an output",
}
PINS = 58
NONCE_LEN, MAC_LEN = 16, 16
FUNCTIONS = ["in", "out", "alt0", "alt1", "alt2", "alt3", "alt4", "alt5"]
PULLS = ["none", "up", "down"]
DATA_LEN = {OP_STATE: 24 + 2 * PINS, OP_SET: 8, OP_FUNCTION: 0, OP_PULL: 0}


class GpioError(Exception):
    pass


def mac_of(label: bytes, first: int, nonce: bytes, body: bytes, key: bytes) -> bytes:
    return hmac.new(key, label + bytes([first]) + nonce + body, hashlib.sha256).digest()[:MAC_LEN]


def wait_for(link, peer: bytes, timeout: float) -> bytes | None:
    """The payload (after the tag) of the next GPIO reply from `peer`, or None."""
    deadline = time.monotonic() + timeout
    while (left := deadline - time.monotonic()) > 0:
        packet = link.receive(min(left, 0.2))
        body = packet and foscopy.chat_body_from(packet, peer)
        if body and body[0] == TAG_REPLY:
            return bytes(body[1:])
    return None


def call(link, peer: bytes, key: bytes, op: int, args: bytes = b"", timeout: float = 2.0) -> tuple[int, bytes]:
    """One signed request: (status, data). GpioError when the machine does not answer or its answer is not ours."""
    link.send(foscopy.frame(peer, link.source, TAG_REQUEST, b"\0"))
    challenge = wait_for(link, peer, timeout)
    if challenge is None:
        raise GpioError("no answer (not a fos machine at its prompt, no synckey there, or another one)")
    if challenge[0] == 2:
        raise GpioError(STATUS_TEXT[2])
    if challenge[0] != 0 or len(challenge) < 1 + NONCE_LEN:
        raise GpioError("the answer is not a GPIO challenge")
    nonce = challenge[1:1 + NONCE_LEN]
    link.send(foscopy.frame(peer, link.source, TAG_REQUEST, bytes([op]) + nonce + args + mac_of(b"fosgpio1", op, nonce, args, key)))
    answer = wait_for(link, peer, timeout)
    if answer is None:
        raise GpioError("no answer to the request (the key does not match, or the challenge expired)")
    status = answer[0]
    size = DATA_LEN[op] if status == 0 or (op == OP_SET and status == 4) else 0
    if len(answer) < 1 + NONCE_LEN + size + MAC_LEN or answer[1:1 + NONCE_LEN] != nonce:
        raise GpioError("the answer does not belong to this request")
    data = answer[1 + NONCE_LEN:1 + NONCE_LEN + size]
    expected = mac_of(b"fosgpio1r", status, nonce, data, key)
    if not hmac.compare_digest(expected, answer[1 + NONCE_LEN + size:1 + NONCE_LEN + size + MAC_LEN]):
        raise GpioError("the answer is not signed with our key")
    return status, data


def decode_state(data: bytes) -> dict:
    levels, allowed, writable = struct.unpack_from("<QQQ", data)
    return {"levels": levels, "allowed": allowed, "writable": writable, "function": list(data[24:24 + PINS]),
            "pull": list(data[24 + PINS:24 + 2 * PINS])}


def state(link, peer: bytes, key: bytes) -> dict:
    status, data = call(link, peer, key, OP_STATE)
    if status != 0:
        raise GpioError(STATUS_TEXT.get(status, f"status {status}"))
    return decode_state(data)


def describe(snapshot: dict, pin: int) -> str:
    level = "high" if snapshot["levels"] >> pin & 1 else "low"
    pull = PULLS[snapshot["pull"][pin]] if snapshot["pull"][pin] < len(PULLS) else "?"
    where = "" if snapshot["allowed"] >> pin & 1 else "  (read only)"
    return f"GPIO{pin}: {FUNCTIONS[snapshot['function'][pin] & 7]}, {level}, pull {pull}{where}"


def pin_number(text: str) -> int:
    if not text.isdigit() or int(text) >= PINS:
        raise GpioError(f"'{text}' is not a GPIO number (0-{PINS - 1})")
    return int(text)


def level_of(text: str) -> int:
    if text in ("0", "low", "off"):
        return 0
    if text in ("1", "high", "on"):
        return 1
    raise GpioError(f"'{text}' is not a level (0 or 1)")


def run(link, peer: bytes, key: bytes, words: list[str]) -> None:
    words = words or ["list"]
    verb, rest = words[0], words[1:]
    if verb == "list" and len(rest) <= 1:
        snapshot = state(link, peer, key)
        shown = range(PINS) if rest == ["all"] else (pin for pin in range(PINS) if snapshot["allowed"] >> pin & 1)
        for pin in shown:
            print(describe(snapshot, pin))
    elif verb == "read" and len(rest) == 1:
        print(describe(state(link, peer, key), pin_number(rest[0])))
    elif verb == "set" and len(rest) == 2:
        pin = pin_number(rest[0])
        status, data = call(link, peer, key, OP_SET, struct.pack("<QQ", 1 << pin, level_of(rest[1]) << pin))
        report(status)
    elif verb == "mode" and len(rest) in (2, 3) and rest[1] in FUNCTIONS:
        function = FUNCTIONS.index(rest[1])
        if function == 1 and len(rest) != 3:
            raise GpioError("an output needs a starting level: mode N out 0|1")
        level = level_of(rest[2]) if function == 1 else 0
        status, _ = call(link, peer, key, OP_FUNCTION, bytes([pin_number(rest[0]), function, level]))
        report(status)
    elif verb == "pull" and len(rest) == 2 and rest[1] in PULLS:
        status, _ = call(link, peer, key, OP_PULL, bytes([pin_number(rest[0]), PULLS.index(rest[1])]))
        report(status)
    else:
        raise GpioError(__doc__.splitlines()[2].strip())


def report(status: int) -> None:
    print(STATUS_TEXT.get(status, f"status {status}"))
    if status != 0:
        raise SystemExit(1)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--iface")
    parser.add_argument("--mac", required=True, help="host name, MAC address or peer number, as foscopy.py takes it")
    parser.add_argument("--key", type=Path, default=KEY_FILE)
    parser.add_argument("--mcast", metavar="GROUP:PORT", help="UDP multicast seam instead of a raw socket (tests)")
    parser.add_argument("--source-mac", default="02:00:00:f0:5c:02")
    parser.add_argument("--discover-timeout", type=float, default=2.0)
    parser.add_argument("words", nargs="*", help="list [all] | read N | set N 0|1 | mode N in|out L|altN | pull N none|up|down")
    args = parser.parse_args()
    key = read_key(args.key)
    if key is None:
        print(f"gpio.py: no usable key in {args.key} (one line of text, the same as the Pi's `synckey`)", file=sys.stderr)
        return 2
    try:
        if args.mcast:
            link = foscopy.McastTransport(args.mcast, foscopy.parse_mac(args.source_mac))
        else:
            link = foscopy.RawTransport(args.iface or foscopy.default_interface())
        peer = foscopy.resolve_host(link, "", args.mac, args.discover_timeout)
        run(link, peer, key, args.words)
    except (GpioError, foscopy.Fatal) as error:
        print(f"gpio.py: {error}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
