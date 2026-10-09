#!/usr/bin/env python3
"""Tapes on the v2 kernel (milestone M5): ELF64 position-independent programs run by the
kernel through the versioned TapeApi table.

Over the term net wire (the same protocol v1 uses): `run hello ARGS` (startup arguments,
`alloc`/`free`, the microsecond clock), `run calc ...` one-shot and interactive (`read_key`
with the term session as the input source, a lone Escape byte as the abort), and a tape
sent over Ethernet with the v1 `tape load net` protocol, then `tape run`, `tape save`
and `run` of the saved copy."""

import os
import select
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_boot import (BROADCAST, CODE_CANDIDATES, IMAGE, ROOT, TAG_DATA, TAG_DISCOVER_REPLY, TAG_DISCOVER_REQUEST,  # noqa: E402
                       TAG_HELLO, VARS_CANDIDATES, Wire, check, clean, find, MCAST_GROUP, MCAST_PORT)

TAPE_START, TAPE_DATA, TAPE_STOP = 0x10, 0x11, 0x12
HELLO = ROOT / "build/cargo-target/x86_64-fos-tape/release/hello"


def main():
    code, variables = find("FOS_OVMF_CODE", CODE_CANDIDATES), find("FOS_OVMF_VARS", VARS_CANDIDATES)
    with tempfile.TemporaryDirectory() as tmp:
        image = Path(tmp) / "image.img"
        shutil.copy(IMAGE, image)
        vars_copy = Path(tmp) / "vars.fd"
        shutil.copy(variables, vars_copy)
        command = ["qemu-system-x86_64", "-machine", "q35", "-m", "512", "-display", "none", "-serial", "stdio", "-no-reboot",
                   "-drive", f"if=pflash,format=raw,readonly=on,file={code}", "-drive", f"if=pflash,format=raw,file={vars_copy}",
                   "-drive", f"format=raw,file={image}",
                   "-netdev", f"socket,id=net0,mcast={MCAST_GROUP}:{MCAST_PORT}", "-device", "e1000,netdev=net0,mac=52:54:00:12:34:7a",
                   # NICs 2 and 3 share a QEMU hub: a cable between them, for memtest's network soak.
                   "-netdev", "hubport,id=cable2,hubid=7", "-device", "e1000,netdev=cable2,mac=52:54:00:12:34:7c",
                   "-netdev", "hubport,id=cable3,hubid=7", "-device", "e1000,netdev=cable3,mac=52:54:00:12:34:7d"]
        wire = Wire()
        process = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        raw = bytearray()
        deadline = time.monotonic() + 150

        def pump(marker):
            while marker not in clean(bytes(raw)):
                if time.monotonic() > deadline or process.poll() is not None:
                    raise SystemExit(f"FAIL: never saw {marker!r}\n{clean(bytes(raw))[-3500:]}")
                ready, _, _ = select.select([process.stdout], [], [], 0.2)
                if ready:
                    raw.extend(os.read(process.stdout.fileno(), 4096))

        try:
            pump("M3 complete")
            wire.send(BROADCAST, TAG_DISCOVER_REQUEST, b"host")
            guest = None
            for _ in range(30):
                for source, tag, _ in wire.frames(0.2):
                    if tag == TAG_DISCOVER_REPLY:
                        guest = source
                if guest:
                    break
            wire.send(guest, TAG_HELLO)
            wire.until("fos> ", 10)

            def run(line, marker="fos> ", timeout=30):
                wire.send(guest, TAG_DATA, line.encode() + b"\n")
                return wire.until(marker, timeout)

            hello = run("run hello a b  c")
            check("hello from a v2 tape (API version 3" in hello, "a tape loads from FOSFS (ELF64 PIE, relocated) and runs", hello)
            check("arguments: [a b  c]" in hello, "startup arguments arrive through the table", hello)
            check("primes up to 100000: 9592" in hello, "it computes correctly with memory from the kernel's alloc()", hello)
            check("fos> " in hello, "control returns to the shell after the tape", hello)

            check("= 35" in run("run calc 3 4 + 5 *"), "calc evaluates its arguments", "")
            check("division by zero" in run("run calc 1 0 /"), "calc reports errors instead of faulting", "")
            check("= -9223372036854775807" in run("run calc -9223372036854775807 1 *"), "64-bit arithmetic", "")

            # Interactive: the term session is the tape's keyboard; a lone Escape byte stops it.
            wire.send(guest, TAG_DATA, b"run calc\n")
            wire.until("> ", 15)
            wire.send(guest, TAG_DATA, b"6 7 *\n")
            check("= 42" in wire.until("= 42", 15), "an interactive tape reads keys from the term net session (read_key)", "")
            wire.send(guest, TAG_DATA, b"\x1b")
            check("fos> " in wire.until("fos> ", 15), "a lone Escape byte aborts the tape and the prompt returns", "")
            # A short frame is padded to 60 bytes with zeros; they must not reach the tape as keys. audio-net's peer picker
            # closes on "any other key", so pressing P over term net used to flash the picker and return to the panel.
            wire.send(guest, TAG_DATA, b"run audio-net\n")
            wire.until("audio-net: settings", 20)
            wire.send(guest, TAG_DATA, b"p")
            opened = wire.until("machines on the network", 10).split("machines on the network")[-1]
            later = opened + "".join(payload.decode("utf-8", "replace") for _, tag, payload in wire.frames(2.5) if tag == TAG_DATA)
            check("audio-net: settings" not in later, "the peer picker stays open after P (the zero padding of a one-byte term net frame is not delivered as keys)", later)
            wire.send(guest, TAG_DATA, b"x")
            closed = "".join(payload.decode("utf-8", "replace") for _, tag, payload in wire.frames(2.5) if tag == TAG_DATA)
            check("audio-net: settings" in closed, "and any other key closes it and brings the settings panel back", closed)
            wire.send(guest, TAG_DATA, b"q")
            wire.until("fos> ", 15)

            # A tape over the network: the v1 `tape load net` protocol.
            blob = HELLO.read_bytes()
            wire.send(guest, TAG_DATA, b"tape load net\n")
            wire.until("Waiting for a tape over the network", 15)
            time.sleep(0.3)
            wire.send(guest, TAPE_START, len(blob).to_bytes(4, "little"))
            time.sleep(0.01)
            for sequence in range(0, (len(blob) + 1023) // 1024):
                wire.send(guest, TAPE_DATA, sequence.to_bytes(4, "little") + blob[sequence * 1024:(sequence + 1) * 1024])
                time.sleep(0.004)
            wire.send(guest, TAPE_STOP)
            loaded = wire.until("fos> ", 20)
            check(f"Tape loaded: {len(blob)} bytes from network" in loaded, "a tape sent over Ethernet is received, verified by length and relocated", loaded)
            ran = run("tape run from the network")
            check("arguments: [from the network]" in ran and "primes up to 100000: 9592" in ran, "and `tape run` runs it", ran)
            check("Saved" in run("tape save sent"), "`tape save` installs it on FOSFS", "")
            check("arguments: [again]" in run("run sent again"), "the saved copy loads and runs like any other file", "")
            check("Unknown command" not in run("uptime"), "the shell is healthy afterwards", "")

            # memtest: the ported engine over RAM claimed from the kernel's allocator.
            clean_run = run("run memtest size=4 passes=1", timeout=120)
            check("claimed 4 MiB" in clean_run and "memtest: PASS" in clean_run, "memtest claims RAM with alloc() and a clean pass reports PASS", clean_run)
            injected = run("run memtest size=4 passes=1 tests=addr inject=100", timeout=120)
            check("memtest: FAIL" in injected and "mismatching word" in injected and "bad bits: 5(" in injected, "memtest detects a deliberately flipped bit and names it", injected)
            soak = run("run memtest soak for=9 status=3 size=4 disk=soakwire disk.every=3 disk.kb=8", timeout=150)
            check("memtest: PASS" in soak and "disk checks:" in soak and "disk checks: 0" not in soak, "soak mode runs (churn, clock, kernel health, a disk write/read-back check)", soak)
            net_soak = run("run memtest soak for=8 status=4 size=4 net=2,3 net.fps=2000", timeout=150)
            check("memtest: PASS" in net_soak and "net NIC 2 -> NIC 3: sent" in net_soak and "net NIC 3 -> NIC 2: sent" in net_soak and "received 0 " not in net_soak,
                  "network soak: checksummed frames of every size cross the cable between two NICs in both directions, none lost or corrupt", net_soak)
            bad_nic = run("run memtest soak for=3 net=2,9", timeout=60)
            check("names a NIC this machine does not have" in bad_nic, "net= with a NIC that does not exist is refused up front", bad_nic)
            check("soakwire" not in run("ls"), "and removes its test file afterwards", "")
            wire.send(guest, TAG_DATA, b"shutdown\n")
            for _ in range(100):
                if process.poll() is not None:
                    break
                time.sleep(0.2)
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
    print("tapes: all checks passed")


if __name__ == "__main__":
    main()
