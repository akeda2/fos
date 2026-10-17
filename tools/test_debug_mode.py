#!/usr/bin/env python3
"""Unattended operation on the v2 kernel (milestone M6): crash records and reboot-on-crash,
the crash-loop guard, the software watchdog, and KUP (network kernel update) with the
loader's A/B fallback.

One persistent disk image per scenario (a throw-away copy), one QEMU that really resets
(no -no-reboot) -- the point is what happens across resets. Scenarios:

 1. `debug crash` with reboot-on-crash: the report, the reset, the next boot's "previous
    crash" record read back from RAM, and the loader noting the unclean stop.
 2. KUP with a bad checksum: rejected, slot metadata untouched.
 3. KUP with the real kernel: ACK, reset, the loader boots slot 1, the kernel confirms and
    promotes it.
 4. KUP with a kernel that crashes before its milestone (the `fault-ud` build), then a
    power cycle: the loader falls back to the known-good slot by itself.
 5. `debug hang` with `watchdog 5`: the watchdog resets the machine and the next boot reports it.
 6. crash-on-boot + reboot-on-crash: five resets, then the crash-loop guard halts.
 7. KUP disabled (no `kernel-update`): the frames are not even answered."""

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
                       VARS_CANDIDATES, Wire, check, clean, find, MCAST_GROUP, MCAST_PORT)

LOADER = ROOT / "build/cargo-target/x86_64-unknown-uefi/release/fosfor-loader.efi"
KERNEL = ROOT / "build/cargo-target/x86_64-fos-kernel/release/fosfor-kernel"
TARGET_DIR = ROOT / "build/cargo-target"
KUP_REQUEST, KUP_DATA, KUP_STOP, KUP_ACK, KUP_ERROR = 0x3a, 0x3b, 0x3c, 0x3d, 0x3e


def internet_checksum(data: bytes) -> int:
    total = 0
    for i in range(0, len(data) - 1, 2):
        total += (data[i] << 8) | data[i + 1]
    if len(data) % 2:
        total += data[-1] << 8
    while total >> 16:
        total = (total & 0xFFFF) + (total >> 16)
    return ~total & 0xFFFF


class Machine:
    """A QEMU+OVMF guest on one persistent image."""

    def __init__(self, tmp, files, binaries=None):
        self.tmp = Path(tmp)
        arguments = []
        for name, path in (binaries or {}).items():
            arguments += ["--file", f"{name}={path}"]
        for name, content in files.items():
            path = self.tmp / f"file-{name}"
            path.write_text(content)
            arguments += ["--file", f"{name}={path}"]
        self.image = self.tmp / "disk.img"
        subprocess.run([sys.executable, str(ROOT / "tools/build_image.py"), "--loader", str(LOADER), "--kernel", str(KERNEL),
                        "--output", str(self.image), *arguments], check=True, capture_output=True)
        self.code = find("FOS_OVMF_CODE", CODE_CANDIDATES)
        self.variables = find("FOS_OVMF_VARS", VARS_CANDIDATES)
        self.wire = Wire()
        self.process = None
        self.raw = bytearray()
        self.guest = None
        self.cpu = None  # a QEMU -cpu model, when a test needs one

    def start(self):
        vars_copy = self.tmp / "vars.fd"
        shutil.copy(self.variables, vars_copy)
        command = ["qemu-system-x86_64", "-machine", "q35", "-m", "512", "-display", "none", "-serial", "stdio",
                   "-drive", f"if=pflash,format=raw,readonly=on,file={self.code}", "-drive", f"if=pflash,format=raw,file={vars_copy}",
                   "-drive", f"format=raw,file={self.image}",
                   "-netdev", f"socket,id=net0,mcast={MCAST_GROUP}:{MCAST_PORT}", "-device", "e1000,netdev=net0,mac=52:54:00:12:34:7b"]
        if self.cpu:
            command += ["-cpu", self.cpu]
        self.process = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        self.raw = bytearray()
        self.guest = None

    def stop(self):
        if self.process:
            self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()

    def text(self):
        return clean(bytes(self.raw))

    def pump(self, seconds=0.2):
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            ready, _, _ = select.select([self.process.stdout], [], [], 0.05)
            if ready:
                self.raw.extend(os.read(self.process.stdout.fileno(), 4096))

    def wait_for(self, marker, timeout=90, since=0, count=1):
        end = time.monotonic() + timeout
        while self.text()[since:].count(marker) < count:
            if time.monotonic() > end or self.process.poll() is not None:
                raise SystemExit(f"FAIL: never saw {marker!r} (x{count})\n{self.text()[-3500:]}")
            self.pump(0.1)
        return self.text()[since:]

    def connect(self):
        """Discovery + HELLO on the wire; returns once the shell prompt appears."""
        self.guest = None
        for _ in range(60):
            self.wire.send(BROADCAST, TAG_DISCOVER_REQUEST, b"host")
            for source, tag, _ in self.wire.frames(0.25):
                if tag == TAG_DISCOVER_REPLY:
                    self.guest = source
            self.pump(0.05)
            if self.guest:
                break
        if not self.guest:
            raise SystemExit("FAIL: no discovery reply\n" + self.text()[-2000:])
        self.wire.send(self.guest, TAG_HELLO)
        self.wire.until("fos> ", 10)

    def run(self, line, timeout=20):
        self.wire.send(self.guest, TAG_DATA, line.encode() + b"\n")
        return self.wire.until("fos> ", timeout)

    def push(self, image: bytes, checksum=None):
        """Sends a kernel image with KUP; returns 'ack', or ('error', code), or None."""
        checksum = internet_checksum(image) if checksum is None else checksum
        self.wire.send(self.guest, KUP_REQUEST, len(image).to_bytes(4, "little") + checksum.to_bytes(2, "little"))
        time.sleep(0.01)
        for sequence in range(0, (len(image) + 1023) // 1024):
            self.wire.send(self.guest, KUP_DATA, sequence.to_bytes(4, "little") + image[sequence * 1024:(sequence + 1) * 1024])
            time.sleep(0.003)
        self.wire.send(self.guest, KUP_STOP)
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            for _, tag, payload in self.wire.frames(0.2):
                if tag == KUP_ACK:
                    return "ack"
                if tag == KUP_ERROR:
                    return ("error", payload[0])
        return None


def build_fault_kernel(feature):
    """The kernel built with one deliberate-fault feature (it crashes before its milestone)."""
    environment = dict(os.environ, CARGO_TARGET_DIR=str(TARGET_DIR))
    command = ["cargo", "+nightly-2026-09-13", "build", "--release", "-Z", "build-std=core,compiler_builtins",
               "-Z", "build-std-features=compiler-builtins-mem", "-Z", "json-target-spec"]
    subprocess.run(command + ["--features", feature], cwd=ROOT / "kernel", env=environment, check=True, capture_output=True)
    data = KERNEL.read_bytes()
    subprocess.run(command, cwd=ROOT / "kernel", env=environment, check=True, capture_output=True)  # restore
    return data


DEBUG = "reboot-on-crash\nreboot-delay 1\nkernel-update\nwatchdog 5\n"


def main():
    good_kernel = KERNEL.read_bytes()
    with tempfile.TemporaryDirectory() as tmp:
        # ---- 1-3 and 5: crash, bad KUP, good KUP, watchdog ----
        machine = Machine(tmp, {"debug": DEBUG})
        machine.start()
        try:
            machine.wait_for("M3 complete")
            machine.connect()
            slots = machine.run("debug slots")
            check("booted slot: 0" in slots and "active slot: 0, pending: None" in slots and "running: 2" in slots, "a fresh image boots slot 0; the milestone marks the boot as up (running 2)", slots)
            check("Watchdog armed: reset after 5 s" in machine.text(), "the watchdog is armed at the milestone", machine.text())

            mark = len(machine.text())
            machine.wire.send(machine.guest, TAG_DATA, b"debug crash\n")
            report = machine.wait_for("CPU EXCEPTION: #UD", 20, mark)
            machine.wait_for("resetting in 1 s", 20, mark)
            second = machine.wait_for("M3 complete", 90, mark)
            check("Previous boot ended without a clean stop (after reaching its milestone)" in second, "the loader notices the unclean stop at the next boot", second)
            check("Previous boot crashed (record found in RAM)" in second and "#UD invalid opcode" in second, "the crash record survives the reset and is reported by the next boot", second)
            machine.connect()

            before = machine.run("debug slots")
            result = machine.push(good_kernel, checksum=0x1234)
            check(result == ("error", 2), f"KUP with a bad checksum is rejected (CHECKSUM_MISMATCH, got {result})", "")
            after = machine.run("debug slots")
            check("pending: None" in after and "active slot: 0" in after, "and leaves the slot metadata untouched", after)

            mark = len(machine.text())
            result = machine.push(good_kernel)
            check(result == "ack", f"KUP with the real kernel is verified, written and acknowledged ({result})", machine.text()[-1500:])
            third = machine.wait_for("M3 complete", 90, mark)
            check("Kernel slot 1:" in third and "booted from slot 1" in third, "after the reset the loader boots the new kernel from slot 1", third)
            check("Kernel update confirmed: slot 1" in third, "and the kernel confirms it at its milestone, promoting it", third)
            machine.connect()
            slots = machine.run("debug slots")
            check("booted slot: 1" in slots and "active slot: 1, pending: None" in slots, "slot 1 is now the active slot, nothing pending", slots)

            mark = len(machine.text())
            machine.wire.send(machine.guest, TAG_DATA, b"debug hang\n")
            hung = machine.wait_for("Watchdog: no progress", 60, mark)
            machine.wait_for("resetting in 1 s", 20, mark)
            woke = machine.wait_for("M3 complete", 90, mark)
            check("watchdog timeout" in woke and "Previous boot crashed (record found in RAM)" in woke, "a hang is caught by the watchdog, which resets the machine and leaves a record", woke)
        finally:
            machine.stop()

        # ---- 4: a pushed kernel that crashes before its milestone; fallback ----
        broken = build_fault_kernel("fault-ud")
        machine = Machine(Path(tmp) / "fallback" if False else tmp, {"debug": DEBUG})
        machine.start()
        try:
            machine.wait_for("M3 complete")
            machine.connect()
            result = machine.push(broken)
            check(result == "ack", "a kernel that will crash is pushed (the loader cannot know)", "")
            mark = len(machine.text())
            machine.wait_for("CPU EXCEPTION: #UD", 90, mark)  # slot 1 boots, faults before its milestone, halts (config not loaded yet)
            machine.stop()
            machine.start()  # the power cycle the operator would do
            again = machine.wait_for("M3 complete", 90)
            check("Previous boot never reached the kernel's milestone" in again, "after the power cycle the loader sees the previous boot never reached its milestone", again)
            check("Kernel slot 0:" in again and "booted from slot 0" in again, "and falls back to the old kernel (slot 0) by itself", again)
            machine.connect()
            slots = machine.run("debug slots")
            check("active slot: 0, pending: None" in slots, "the failed update is forgotten: slot 0 stays active, nothing pending", slots)
        finally:
            machine.stop()

    # ---- 6: the crash-loop guard ----
    with tempfile.TemporaryDirectory() as tmp:
        machine = Machine(tmp, {"debug": "reboot-on-crash\nreboot-delay 1\ncrash-on-boot\n"})
        machine.start()
        try:
            machine.wait_for("crash loop", 150)
            text = machine.text()
            check(text.count("CPU EXCEPTION: #UD") >= 5, f"crash-on-boot resets repeatedly ({text.count('CPU EXCEPTION: #UD')} crashes seen)", text)
            check("5 boots in a row never reached the milestone; not resetting again" in text, "after five the crash-loop guard stops rebooting and halts", text)
        finally:
            machine.stop()

    # ---- 7: KUP disabled ----
    with tempfile.TemporaryDirectory() as tmp:
        machine = Machine(tmp, {})
        machine.start()
        try:
            machine.wait_for("M3 complete")
            machine.connect()
            check(machine.push(good_kernel) is None, "without `kernel-update` the KUP frames are not even answered", "")
        finally:
            machine.stop()
    print("debug mode: all checks passed")


if __name__ == "__main__":
    main()
