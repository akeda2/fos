#!/usr/bin/env python3
"""The per-machine configuration files on the boot volume (milestone M4):
`hostname`, `settings`, `debug` and the `start` script are read from FOSFS and honoured.

Images are built with `build_image.py --file NAME=PATH`, so no real file is touched.
Boot 1: hostname + start script: discovery answers with the new name, the script's
commands run at boot. Boot 2: `deny-term-net`: discovery still answers but a HELLO gets
no session. Boot 3: `reboot-on-crash` + `crash-on-boot`: the machine faults, reports,
resets itself through UEFI and does it again (QEMU without -no-reboot)."""

import os
import select
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_boot import (BROADCAST, CODE_CANDIDATES, ROOT, TAG_DATA, TAG_DISCOVER_REPLY, TAG_DISCOVER_REQUEST, TAG_HELLO,  # noqa: E402
                       VARS_CANDIDATES, Wire, check, clean, find, MCAST_GROUP, MCAST_PORT, LOADER_BANNER)

LOADER = ROOT / "build/cargo-target/x86_64-unknown-uefi/release/fos-loader.efi"
KERNEL = ROOT / "build/cargo-target/x86_64-fos-kernel/release/fos-v2-kernel"


def run(files, wait_for, extra_after=None, no_reboot=True, seconds=90, times=1):
    """Builds an image with `files`, boots it, waits for `wait_for` in the serial text, calls
    extra_after(wire, text) if given, and returns the serial text."""
    code, variables = find("FOS_OVMF_CODE", CODE_CANDIDATES), find("FOS_OVMF_VARS", VARS_CANDIDATES)
    with tempfile.TemporaryDirectory() as tmp:
        arguments = []
        for name, content in files.items():
            path = Path(tmp) / f"file-{name}"
            path.write_text(content)
            arguments += ["--file", f"{name}={path}"]
        image = Path(tmp) / "image.img"
        subprocess.run([sys.executable, str(ROOT / "tools/build_image.py"), "--loader", str(LOADER), "--kernel", str(KERNEL),
                        "--output", str(image), *arguments], check=True, capture_output=True)
        vars_copy = Path(tmp) / "vars.fd"
        shutil.copy(variables, vars_copy)
        command = ["qemu-system-x86_64", "-machine", "q35", "-m", "512", "-display", "none", "-serial", "stdio",
                   "-drive", f"if=pflash,format=raw,readonly=on,file={code}", "-drive", f"if=pflash,format=raw,file={vars_copy}",
                   "-drive", f"format=raw,file={image}",
                   "-netdev", f"socket,id=net0,mcast={MCAST_GROUP}:{MCAST_PORT}", "-device", "e1000,netdev=net0,mac=52:54:00:12:34:79"]
        if no_reboot:
            command.append("-no-reboot")
        wire = Wire()
        process = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        raw = bytearray()
        deadline = time.monotonic() + seconds
        try:
            while clean(bytes(raw)).count(wait_for) < times:
                if time.monotonic() > deadline or process.poll() is not None:
                    raise SystemExit(f"FAIL: never saw {wait_for!r}\n{clean(bytes(raw))[-3000:]}")
                ready, _, _ = select.select([process.stdout], [], [], 0.2)
                if ready:
                    raw.extend(os.read(process.stdout.fileno(), 4096))
            if extra_after:
                extra_after(wire, clean(bytes(raw)))
            time.sleep(0.5)
            while select.select([process.stdout], [], [], 0.2)[0]:
                raw.extend(os.read(process.stdout.fileno(), 4096))
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
        return clean(bytes(raw))


def discover(wire):
    wire.send(BROADCAST, TAG_DISCOVER_REQUEST, b"host")
    for _ in range(30):
        for source, tag, payload in wire.frames(0.2):
            if tag == TAG_DISCOVER_REPLY:
                return source, payload.rstrip(b"\0")
    return None, None


def main():
    replies = {}

    def named(wire, text):
        replies["lab"] = discover(wire)[1]

    text = run({"hostname": "lab\n", "start": "# a comment\nuptime\nmemory\n"}, "M3 complete", named)
    check("hostname file: this machine announces itself as lab1" in text, "the hostname file is read", text)
    check("start file: running 2 command(s)." in text and "start> uptime" in text and "ticks at 100 Hz" in text and "Managed:" in text, "the start script runs its commands at boot", text)
    check(replies["lab"] == b"lab1", f"discovery answers with the configured name ({replies['lab']})", text)

    outcome = {}

    def denied(wire, text):
        outcome["reply"] = discover(wire)[1]
        guest = discover(wire)[0]
        wire.send(guest, TAG_HELLO)
        seen = [tag for _, tag, _ in [f for _ in range(10) for f in wire.frames(0.2)] if tag == TAG_DATA]
        outcome["data"] = seen

    text = run({"settings": "deny-term-net\n"}, "M3 complete", denied)
    check("deny-term-net" in text and outcome["reply"] == b"fos2", "deny-term-net: discovery still answers", text)
    check(not outcome["data"] and "Remote terminal connected" not in text, "deny-term-net: a HELLO gets no session", text)

    text = run({"debug": "reboot-on-crash\nreboot-delay 1\ncrash-on-boot\n"}, "resetting in 1 s", no_reboot=False, times=2)
    check("CPU EXCEPTION: #UD invalid opcode" in text, "crash-on-boot raises a real exception at boot", text)
    check(text.count(LOADER_BANNER) >= 2, "reboot-on-crash resets the machine, and it boots (and crashes) again", text)
    print("v2 config: all checks passed")


if __name__ == "__main__":
    main()
