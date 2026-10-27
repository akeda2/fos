#!/usr/bin/env python3
"""Asks a fos machine for its signed tape manifest (what `tapesync` uses) and compares it with the build:

    tape_manifest.py --iface IF --mac N [--key FILE] [NAME...]

The machine answers with name, size, `.fos_build` stamp and SHA-256 of every stamped tape on its boot volume, signed
(HMAC-SHA-256 over our random nonce and the manifest) with the `synckey` file both sides hold, so nothing but one small
frame stream crosses the link and a forged or replayed answer is refused (crates/tape-sync, kernel/src/tapesync.rs).
The key is the first line of `synckey` in the repository root (untracked, see .gitignore) or `--key FILE`; the machine
must have the same key. Wire format: crates/tape-sync/src/lib.rs. Needs CAP_NET_RAW (the `python3-netraw` copy)."""
import argparse
import hashlib
import hmac
import os
import struct
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import foscopy  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent
KEY_FILE = ROOT / "synckey"
MAGIC = b"FOSSYNC1"
HEADER_LEN, ENTRY_LEN, MAC_LEN, NONCE_LEN = 16, 64, 32, 16


class ManifestError(Exception):
    pass


def read_key(path: Path = KEY_FILE) -> bytes | None:
    """The first line of the key file, trimmed (as the kernel reads it); None without a usable file."""
    try:
        lines = path.read_text().splitlines()
    except OSError:
        return None
    line = lines[0].strip() if lines else ""
    return line.encode() if line and len(line) <= 128 else None


def decode(data: bytes, key: bytes, nonce: bytes) -> dict[str, dict]:
    """name -> {length, build, abi, arch, sha256 (hex)} from a manifest; ManifestError when it is not valid for `key`."""
    if len(data) < HEADER_LEN + MAC_LEN or data[:8] != MAGIC:
        raise ManifestError("the answer is not a tape manifest")
    count = struct.unpack_from("<H", data, 8)[0]
    body = HEADER_LEN + count * ENTRY_LEN
    if count > 64 or len(data) != body + MAC_LEN:
        raise ManifestError("the answer is not a tape manifest")
    if not hmac.compare_digest(hmac.new(key, nonce + data[:body], hashlib.sha256).digest(), data[body:]):
        raise ManifestError("the manifest is not signed with our key (another synckey there, or a forgery)")
    entries = {}
    for index in range(count):
        at = HEADER_LEN + index * ENTRY_LEN
        name = data[at:at + 16].split(b"\0")[0].decode("ascii", "replace")
        length, build, word = struct.unpack_from("<IQI", data, at + 16)
        entries[name] = {"length": length, "build": build, "abi": word & 0xFFFF, "arch": word >> 16,
                         "sha256": data[at + 32:at + 64].hex()}
    return entries


def fetch(link, peer: bytes, key: bytes, timeout: float = 20.0) -> dict[str, dict]:
    """Requests and verifies the manifest of `peer`; ManifestError (or foscopy.Fatal if it refuses or is silent)."""
    nonce = os.urandom(NONCE_LEN)
    return decode(foscopy.request_stream(link, peer, foscopy.TAG_SYNC_REQUEST, nonce, timeout), key, nonce)


def main() -> int:
    from pathlib import Path
    from verify_tapes import TAPES as TAPES_DIR, describe
    TAPES = Path(TAPES_DIR)
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--iface")
    parser.add_argument("--mac", required=True, help="host name, MAC address or peer number, as foscopy.py takes it")
    parser.add_argument("--key", type=Path, default=KEY_FILE)
    parser.add_argument("--mcast", metavar="GROUP:PORT", help="UDP multicast seam instead of a raw socket (tests)")
    parser.add_argument("--source-mac", default="02:00:00:f0:5c:01")
    parser.add_argument("names", nargs="*", help="tapes to compare (default: every tape in the build)")
    args = parser.parse_args()
    key = read_key(args.key)
    if key is None:
        print(f"no usable key in {args.key} (one line of text, the same as the machine's `synckey`).", file=sys.stderr)
        return 2
    link = foscopy.McastTransport(args.mcast, foscopy.parse_mac(args.source_mac)) if args.mcast \
        else foscopy.RawTransport(args.iface or foscopy.default_interface())
    peer = foscopy.resolve_host(link, args.mac, None, 2.0)
    try:
        manifest = fetch(link, peer, key)
    except (ManifestError, foscopy.Fatal) as problem:
        print(f"manifest: {problem}", file=sys.stderr)
        return 2
    names = args.names or sorted(p.name for p in TAPES.iterdir() if p.is_file() and not p.name.startswith("."))
    bad = 0
    for name in names:
        built = (TAPES / name).read_bytes()
        entry = manifest.get(name)
        if entry is None:
            print(f"{name:<16} MISSING   (not a stamped tape there)")
            bad += 1
            continue
        same = entry["sha256"] == hashlib.sha256(built).hexdigest()
        stamp = time.strftime("%Y-%m-%d %H:%M:%S", time.localtime(entry["build"]))
        print(f"{name:<16} {'same' if same else 'DIFFERS':<9} machine {stamp} abi {entry['abi']}; build {describe(built)}")
        bad += not same
    print(f"{len(names) - bad} of {len(names)} identical to the build.")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
