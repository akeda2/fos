#!/usr/bin/env python3
"""Checks which tapes a fos machine holds are byte-identical to the build: `verify_tapes.py --iface IF --mac N [NAME...]`.

Each tape is fetched with `foscopy.py` (NETFS GET, checksummed on the wire) and compared by SHA-256 with
`build/fosfor/tapes/NAME`; the `.fos_build` stamp (see `stamp_tape.py`) of both is printed, so "same build" is a fact and not
a guess from file sizes. Without NAMEs every stamped tape in the build directory is checked. Read-only on the machine; exit
status 1 when any tape differs or is missing. Needs CAP_NET_RAW like foscopy (run it with the `python3-netraw` copy).
"""
import argparse
import hashlib
import os
import struct
import subprocess
import sys
import tempfile
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
TAPES = os.path.join(ROOT, "build", "fosfor", "tapes")


def stamp(data):
    """(build time, ABI version) from the ELF section `.fos_build`, or None."""
    try:
        shoff, shentsize, shnum, shstrndx = struct.unpack_from("<Q10xHHH", data, 0x28)[0], *struct.unpack_from("<HHH", data, 0x3A)
        sections = [struct.unpack_from("<IIQQQQ", data, shoff + i * shentsize) for i in range(shnum)]
        names = sections[shstrndx]
        for name, _kind, _flags, _addr, offset, size in sections:
            start = names[4] + name
            if data[start:data.index(b"\0", start)] == b".fos_build" and size >= 16:
                seconds, abi = struct.unpack_from("<QI", data, offset)
                return seconds, abi
    except (struct.error, ValueError, IndexError):
        pass
    return None


def describe(data):
    found = stamp(data)
    if not found:
        return "no stamp"
    return f"{time.strftime('%Y-%m-%d %H:%M:%S', time.localtime(found[0]))} abi {found[1]}"


def pull(iface, mac, name, dest):
    """Fetches `:disk1/NAME` with foscopy into `dest`; (True, "") or (False, the reason)."""
    run = subprocess.run([sys.executable, os.path.join(ROOT, "tools", "foscopy.py"), "--iface", iface, "--mac", mac, f":disk1/{name}", dest],
                         capture_output=True, text=True)
    if run.returncode != 0 or not os.path.exists(dest):
        return False, ((run.stdout + run.stderr).strip().splitlines()[-1:] or ["no answer"])[0]
    return True, ""


def compare(iface, mac, name, built_path, scratch):
    """("same" | "differs" | "missing", reason, the machine's bytes or None) for one tape against its build."""
    local = os.path.join(scratch, name)
    ok, reason = pull(iface, mac, name, local)
    if not ok:
        return "missing", reason, None
    held = open(local, "rb").read()
    same = hashlib.sha256(held).digest() == hashlib.sha256(open(built_path, "rb").read()).digest()
    return ("same" if same else "differs"), "", held


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--iface", required=True)
    parser.add_argument("--mac", required=True, help="host name, MAC address or peer number, as foscopy.py takes it")
    parser.add_argument("names", nargs="*")
    args = parser.parse_args()
    names = args.names or sorted(n for n in os.listdir(TAPES) if stamp(open(os.path.join(TAPES, n), "rb").read()))
    bad = 0
    with tempfile.TemporaryDirectory() as scratch:
        for name in names:
            path = os.path.join(TAPES, name)
            state, reason, held = compare(args.iface, args.mac, name, path, scratch)
            if held is None:
                print(f"{name:<16} MISSING   {reason}")
            else:
                print(f"{name:<16} {state.upper() if state != 'same' else 'same':<9} machine {describe(held)}; build {describe(open(path, 'rb').read())}")
            bad += state != "same"
    print(f"{len(names) - bad} of {len(names)} identical to the build.")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
