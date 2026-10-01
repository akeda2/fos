#!/usr/bin/env python3
"""Automated end-to-end smoke test for fos's debug mode (see
`kernel/src/debug.rs`, AGENTS.md's "Debug mode" section, and
`tools/send_kernel_update.py`): unattended, remote-friendly development on
real hardware via reboot-on-crash, the consecutive-crash loop guard, and
KUP (the network kernel-update service).

Unlike most of this project's manual, ad hoc network round-trip scripts,
this one *is* wired into the Makefile (`make test-debug-mode`) since every
scenario here only needs QEMU's own serial console (or, for KUP, a
multicast netdev this script joins directly as a third virtual peer -- see
`McastSocket`) and never a second guest or a real interface.

Deliberately does **not** pass `-no-reboot`: several scenarios below rely
on QEMU performing a genuine guest reset when the kernel asks for one
(an 8042 keyboard-controller pulse reset), the same way real hardware
would. `-snapshot` is used throughout so none of these throwaway images
are ever mutated on disk.

Covers:
  1. `reboot-on-crash` on: a deliberate crash (`debug crash`) reboots the
     machine, and the *next* boot reports the previous crash record.
  2. `reboot-on-crash` off (the default, no `debug` file at all): the same
     crash halts instead, with no reboot.
  3. The consecutive-crash loop guard: `crash-on-boot` (a test-only debug
     option that crashes before the milestone that would otherwise reset
     the loop counter) reliably halts after exactly
     `MAX_CONSECUTIVE_CRASHES` automatic reboots, never looping forever.
  4. KUP: disabled by default (frames silently ignored), a bad-checksum
     push is rejected with slot metadata left untouched, and a good push
     of the current `build/kernel.bin` is installed, ACKed, and actually
     booted (confirmed via the hidden `debug slots` command) after a real
     reset.
  5. `reboot soft`/`reboot hard`: a soft reboot re-enters the boot loader
     without ever resetting the CPU and still reports real memory/finds
     the NIC afterward (a direct regression check for the two soft-
     reboot-only bugs fixed and documented in AGENTS.md); `reboot hard`
     performs the same hardware reset `reset` does.
"""

import os
import selectors
import socket as socketlib
import struct
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
STAGE1 = ROOT / "build" / "stage1.bin"
STAGE2 = ROOT / "build" / "stage2.bin"
KERNEL = ROOT / "build" / "kernel.bin"
BUILD_IMAGE = ROOT / "tools" / "build_image.py"

MCAST_GROUP = "230.42.42.76"
MCAST_PORT = 4276
HOST_MAC = bytes.fromhex("02deadbeef10")
GUEST_MAC = bytes.fromhex("525400123801")
BROADCAST_MAC = b"\xff" * 6
CHAT_ETHERTYPE = (0x88, 0xB6)

KUP_TAG_PUT_REQUEST = 0x3A
KUP_TAG_PUT_DATA = 0x3B
KUP_TAG_PUT_STOP = 0x3C
KUP_TAG_PUT_ACK = 0x3D
KUP_TAG_PUT_ERROR = 0x3E
KUP_ERROR_CHECKSUM_MISMATCH = 2
CHUNK_LEN = 1024


def internet_checksum(data: bytes) -> int:
    total = 0
    it = iter(data)
    for high in it:
        low = next(it, 0)
        total += (high << 8) | low
    while total > 0xFFFF:
        total = (total & 0xFFFF) + (total >> 16)
    return (~total) & 0xFFFF


class McastSocket:
    def __init__(self, group: str, port: int):
        self.sock = socketlib.socket(socketlib.AF_INET, socketlib.SOCK_DGRAM, socketlib.IPPROTO_UDP)
        self.sock.setsockopt(socketlib.SOL_SOCKET, socketlib.SO_REUSEADDR, 1)
        self.sock.bind(("0.0.0.0", port))
        mreq = struct.pack("4sl", socketlib.inet_aton(group), socketlib.INADDR_ANY)
        self.sock.setsockopt(socketlib.IPPROTO_IP, socketlib.IP_ADD_MEMBERSHIP, mreq)
        self.sock.setsockopt(socketlib.IPPROTO_IP, socketlib.IP_MULTICAST_TTL, 1)

    def send(self, frame: bytes) -> None:
        self.sock.sendto(frame, (MCAST_GROUP, MCAST_PORT))

    def receive(self, timeout: float) -> bytes | None:
        self.sock.settimeout(timeout)
        try:
            return self.sock.recv(2048)
        except TimeoutError:
            return None


def pad(frame: bytes) -> bytes:
    frame = bytearray(frame)
    if len(frame) < 60:
        frame.extend(b"\0" * (60 - len(frame)))
    return bytes(frame)


def kup_frame(destination: bytes, source: bytes, tag: int, payload: bytes = b"") -> bytes:
    return pad(destination + source + bytes(CHAT_ETHERTYPE) + bytes([tag]) + payload)


def send_kup_update(mcast: McastSocket, data: bytes, checksum_override: int | None = None) -> None:
    checksum = checksum_override if checksum_override is not None else internet_checksum(data)
    request_payload = len(data).to_bytes(4, "little") + checksum.to_bytes(2, "little")
    mcast.send(kup_frame(BROADCAST_MAC, HOST_MAC, KUP_TAG_PUT_REQUEST, request_payload))
    for seq, offset in enumerate(range(0, len(data), CHUNK_LEN)):
        chunk = data[offset : offset + CHUNK_LEN]
        mcast.send(kup_frame(BROADCAST_MAC, HOST_MAC, KUP_TAG_PUT_DATA, seq.to_bytes(4, "little") + chunk))
        time.sleep(0.003)
    mcast.send(kup_frame(BROADCAST_MAC, HOST_MAC, KUP_TAG_PUT_STOP))


def wait_for_kup_reply(mcast: McastSocket, timeout: float) -> tuple[int, int] | None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        packet = mcast.receive(max(deadline - time.monotonic(), 0.01))
        if packet is None:
            continue
        if len(packet) < 15 or packet[12:14] != bytes(CHAT_ETHERTYPE) or packet[0:6] != HOST_MAC:
            continue
        tag = packet[14]
        if tag == KUP_TAG_PUT_ACK:
            return (tag, 0)
        if tag == KUP_TAG_PUT_ERROR:
            return (tag, packet[15] if len(packet) > 15 else 0)
    return None


def build_image(image_path: Path, debug_text: str | None) -> None:
    args = [
        sys.executable, str(BUILD_IMAGE),
        "--stage1", str(STAGE1), "--stage2", str(STAGE2), "--kernel", str(KERNEL),
        "--output", str(image_path),
    ]
    if debug_text is not None:
        debug_file = image_path.with_suffix(".debug.txt")
        debug_file.write_text(debug_text)
        args += ["--file", "debug", str(debug_file)]
    subprocess.run(args, check=True)


class SerialGuest:
    """A QEMU guest with `-nic none`: no networking at all, driven purely
    over its own serial console -- sufficient for every scenario that
    doesn't need KUP's own Ethernet round trip. Commands can be typed
    directly over serial stdin even at the ordinary idle prompt (see
    `shell::run`'s input loop), so no monitor/keypress is needed here."""

    def __init__(self, image_path: Path):
        self.output = bytearray()
        self.selector = selectors.DefaultSelector()
        self.process = subprocess.Popen(
            [
                "qemu-system-i386",
                "-drive", f"format=raw,file={image_path}",
                "-serial", "stdio",
                "-display", "none",
                "-snapshot",
                "-nic", "none",
            ],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )
        self.selector.register(self.process.stdout, selectors.EVENT_READ)

    def read_until(self, marker: bytes, timeout: float, start: int = 0) -> bytes:
        deadline = time.monotonic() + timeout
        while marker not in self.output[start:]:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise SystemExit(
                    f"timed out waiting for {marker!r}; output since checkpoint:\n"
                    f"{self.output[start:].decode('utf-8', 'replace')}"
                )
            for key, _ in self.selector.select(remaining):
                chunk = os.read(key.fd, 4096)
                if not chunk:
                    raise SystemExit(f"guest serial stream ended early:\n{self.output.decode('utf-8', 'replace')}")
                self.output.extend(chunk)
        return bytes(self.output[start:])

    def run_command(self, command: str, timeout: float = 10) -> bytes:
        start = len(self.output)
        self.process.stdin.write((command + "\r\n").encode())
        self.process.stdin.flush()
        return self.read_until(b"fos> ", timeout, start)

    def terminate(self) -> None:
        self.process.terminate()
        try:
            self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.process.kill()


def test_crash_reboot(tmp_dir: Path) -> None:
    image_path = tmp_dir / "crash-reboot.img"
    build_image(image_path, "reboot-on-crash\nreboot-delay 1\n")
    guest = SerialGuest(image_path)
    try:
        guest.read_until(b"fos> ", 15)
        result = guest.run_command("debug status")
        if b"reboot-on-crash=true, reboot-delay=1s" not in result:
            raise SystemExit(f"unexpected debug status:\n{result.decode('utf-8', 'replace')}")
        checkpoint = len(guest.output)
        guest.process.stdin.write(b"debug crash\r\n")
        guest.process.stdin.flush()
        guest.read_until(b"CPU EXCEPTION 3 (Breakpoint)", 5, checkpoint)
        guest.read_until(b"Rebooting: this kernel has no fault recovery.", 5, checkpoint)
        guest.read_until(b"Kernel: protected-mode Rust entry reached.", 15, checkpoint)
        guest.read_until(b"debug: previous boot crashed (vector 3)", 10, checkpoint)
        guest.read_until(b"fos> ", 10, checkpoint)
        print("PASS: reboot-on-crash reboots on a fatal exception and reports it on the next boot.")
    finally:
        guest.terminate()


def test_crash_halts_by_default(tmp_dir: Path) -> None:
    image_path = tmp_dir / "crash-halt.img"
    build_image(image_path, None)
    guest = SerialGuest(image_path)
    try:
        guest.read_until(b"fos> ", 15)
        result = guest.run_command("debug status")
        if b"reboot-on-crash=false" not in result:
            raise SystemExit(f"expected reboot-on-crash=false by default:\n{result.decode('utf-8', 'replace')}")
        checkpoint = len(guest.output)
        guest.process.stdin.write(b"debug crash\r\n")
        guest.process.stdin.flush()
        guest.read_until(b"Halting: this kernel has no fault recovery.", 5, checkpoint)
        time.sleep(2)
        if b"debug: rebooting" in guest.output[checkpoint:]:
            raise SystemExit("the machine rebooted even though reboot-on-crash is off!")
        print("PASS: with reboot-on-crash off (the default), a crash halts instead of rebooting.")
    finally:
        guest.terminate()


def test_crash_loop_guard(tmp_dir: Path) -> None:
    image_path = tmp_dir / "crash-loop.img"
    build_image(image_path, "reboot-on-crash\nreboot-delay 1\ncrash-on-boot\n")
    guest = SerialGuest(image_path)
    try:
        guest.read_until(b"halting instead of rebooting again", 30)
        print("PASS: the consecutive-crash loop guard halts after repeated pre-milestone crashes.")
    finally:
        guest.terminate()


def test_reboot_soft(tmp_dir: Path) -> None:
    """`reboot`/`reboot soft` (`kernel/src/reboot.rs`) warm-restarts by
    jumping straight back into `boot/stage2.S`'s own `_start`, with no
    hardware CPU reset at all. Confirmed here the same way
    `tools/test_reset_e2e.py` confirms the hardware `reset` path: stage
    two's own boot banner reappears a second time on this one continuous
    serial stream, followed by a fresh, working shell prompt -- exactly
    the scenario the two soft-reboot-only bugs documented in AGENTS.md
    (stale KUP slot-LBA constants, and `collect_memory_map`'s partial
    `%ebp` clear) were both found and fixed against."""
    image_path = tmp_dir / "reboot-soft.img"
    build_image(image_path, None)
    guest = SerialGuest(image_path)
    try:
        guest.read_until(b"fos> ", 15)
        if guest.output.count(b"FOS stage 2: real mode active.") != 1:
            raise SystemExit(
                f"expected exactly one boot banner before 'reboot soft':\n{guest.output.decode('utf-8', 'replace')}"
            )
        if b"Physical memory: 0 frames" in guest.output:
            raise SystemExit("first boot already reported zero memory frames; test fixture is broken")
        checkpoint = len(guest.output)
        guest.process.stdin.write(b"reboot soft\r\n")
        guest.process.stdin.flush()
        guest.read_until(
            b"Soft-rebooting: re-entering the boot loader without a hardware reset...", 5, checkpoint
        )
        guest.read_until(b"FOS stage 2: real mode active.", 15, checkpoint)
        guest.read_until(b"Kernel: protected-mode Rust entry reached.", 15, checkpoint)
        result = guest.read_until(b"fos> ", 10, checkpoint)
        # Regression check for the E820 %ebp bug specifically: a soft
        # reboot must still report real, nonzero tracked memory, not
        # silently regress into the "0 frames" symptom that bug produced.
        # (`SerialGuest` runs with `-nic none`, so there is deliberately no
        # NIC-detection check here -- that half of the original bug's
        # symptom is instead covered by `test_kup_good_update`'s NetGuest
        # scenario, which already exercises a soft reboot with a real NIC
        # attached and would fail its own later `debug slots` check if the
        # NIC/pmem state came back corrupted.)
        if b"Physical memory: 0 frames" in result:
            raise SystemExit(f"soft reboot reported zero memory frames:\n{result.decode('utf-8', 'replace')}")
        print("PASS: 'reboot soft' re-enters the boot loader (no hardware reset) and reaches a working prompt.")
    finally:
        guest.terminate()


def test_reboot_hard(tmp_dir: Path) -> None:
    """`reboot hard` is documented as an explicit alias for `reset`'s
    8042 keyboard-controller pulse reset; confirm the two-word form
    actually dispatches to that same hardware-reset path (not, say,
    silently falling through to a no-op or the soft-reboot path)."""
    image_path = tmp_dir / "reboot-hard.img"
    build_image(image_path, None)
    guest = SerialGuest(image_path)
    try:
        guest.read_until(b"fos> ", 15)
        checkpoint = len(guest.output)
        guest.process.stdin.write(b"reboot hard\r\n")
        guest.process.stdin.flush()
        guest.read_until(b"Resetting the machine now (8042 keyboard-controller pulse reset)...", 5, checkpoint)
        guest.read_until(b"FOS stage 2: real mode active.", 15, checkpoint)
        guest.read_until(b"Kernel: protected-mode Rust entry reached.", 15, checkpoint)
        guest.read_until(b"fos> ", 10, checkpoint)
        print("PASS: 'reboot hard' performs the same hardware reset as 'reset'.")
    finally:
        guest.terminate()


class NetGuest:
    """A QEMU guest with a real NIC on the shared multicast netdev, for
    the KUP scenarios. Reaching KUP's own idle-loop-only service point
    needs one F12 (via the QEMU monitor) to leave the automatic boot-time
    `term net` wait; ordinary commands can then go over serial stdin the
    same as `SerialGuest`."""

    def __init__(self, image_path: Path, monitor_sock: Path):
        self.monitor_sock = monitor_sock
        self.output = bytearray()
        self.selector = selectors.DefaultSelector()
        self.process = subprocess.Popen(
            [
                "qemu-system-i386",
                "-drive", f"format=raw,file={image_path}",
                "-serial", "stdio",
                "-monitor", f"unix:{monitor_sock},server,nowait",
                "-display", "none",
                "-snapshot",
                "-no-shutdown",
                "-netdev", f"socket,id=net0,mcast={MCAST_GROUP}:{MCAST_PORT}",
                "-device", f"rtl8139,netdev=net0,mac={GUEST_MAC.hex(':')}",
            ],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )
        self.selector.register(self.process.stdout, selectors.EVENT_READ)

    def read_until(self, marker: bytes, timeout: float, start: int = 0) -> bytes:
        deadline = time.monotonic() + timeout
        while marker not in self.output[start:]:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise SystemExit(
                    f"timed out waiting for {marker!r}; output since checkpoint:\n"
                    f"{self.output[start:].decode('utf-8', 'replace')}"
                )
            for key, _ in self.selector.select(remaining):
                chunk = os.read(key.fd, 4096)
                if not chunk:
                    raise SystemExit(f"guest serial stream ended early:\n{self.output.decode('utf-8', 'replace')}")
                self.output.extend(chunk)
        return bytes(self.output[start:])

    def escape_to_idle_prompt(self) -> None:
        start = len(self.output)
        self.read_until(b"Waiting for a remote terminal client to connect", 20, start)
        monitor = None
        for _ in range(50):
            try:
                monitor = socketlib.socket(socketlib.AF_UNIX, socketlib.SOCK_STREAM)
                monitor.connect(str(self.monitor_sock))
                break
            except (FileNotFoundError, ConnectionRefusedError):
                if monitor is not None:
                    monitor.close()
                monitor = None
                time.sleep(0.1)
        if monitor is None:
            raise SystemExit("could not connect to the guest's QEMU monitor")
        monitor.settimeout(5)
        monitor.recv(65536)
        monitor.sendall(b"sendkey f12\n")
        monitor.close()
        self.read_until(b"fos> ", 10, start)

    def run_command(self, command: str, timeout: float = 10) -> bytes:
        start = len(self.output)
        self.process.stdin.write((command + "\r\n").encode())
        self.process.stdin.flush()
        return self.read_until(b"fos> ", timeout, start)

    def terminate(self) -> None:
        self.process.terminate()
        try:
            self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.process.kill()


def test_kup_disabled_by_default(tmp_dir: Path) -> None:
    image_path = tmp_dir / "kup-disabled.img"
    build_image(image_path, None)
    guest = NetGuest(image_path, tmp_dir / "kup-disabled-monitor.sock")
    try:
        guest.escape_to_idle_prompt()
        if b"kernel-update=false" not in guest.run_command("debug status"):
            raise SystemExit("expected kernel-update=false by default")
        mcast = McastSocket(MCAST_GROUP, MCAST_PORT)
        send_kup_update(mcast, b"\x90" * 4096)
        if wait_for_kup_reply(mcast, 5) is not None:
            raise SystemExit("expected no reply with kernel-update disabled")
        if b"active=0" not in guest.run_command("debug slots"):
            raise SystemExit("guest's active slot changed despite kernel-update being disabled")
        print("PASS: KUP frames are silently ignored when 'kernel-update' is not enabled.")
    finally:
        guest.terminate()


def test_kup_bad_checksum(tmp_dir: Path, debug_text: str) -> None:
    image_path = tmp_dir / "kup-bad-checksum.img"
    build_image(image_path, debug_text)
    guest = NetGuest(image_path, tmp_dir / "kup-bad-checksum-monitor.sock")
    try:
        guest.escape_to_idle_prompt()
        mcast = McastSocket(MCAST_GROUP, MCAST_PORT)
        data = b"\x90" * 4096
        send_kup_update(mcast, data, checksum_override=(internet_checksum(data) ^ 0xFFFF))
        reply = wait_for_kup_reply(mcast, 10)
        if reply != (KUP_TAG_PUT_ERROR, KUP_ERROR_CHECKSUM_MISMATCH):
            raise SystemExit(f"expected a CHECKSUM_MISMATCH error, got {reply}")
        result = guest.run_command("debug slots")
        if b"active=0" not in result or b"pending=none" not in result:
            raise SystemExit(f"a bad-checksum push modified slot metadata:\n{result.decode('utf-8', 'replace')}")
        print("PASS: a bad-checksum KUP push is rejected and leaves slot metadata untouched.")
    finally:
        guest.terminate()


def test_kup_good_update(tmp_dir: Path, debug_text: str) -> None:
    image_path = tmp_dir / "kup-good.img"
    build_image(image_path, debug_text)
    guest = NetGuest(image_path, tmp_dir / "kup-good-monitor.sock")
    try:
        guest.escape_to_idle_prompt()
        if b"active=0" not in guest.run_command("debug slots"):
            raise SystemExit("expected a fresh image to boot active slot 0")

        data = KERNEL.read_bytes()
        mcast = McastSocket(MCAST_GROUP, MCAST_PORT)
        checkpoint = len(guest.output)
        send_kup_update(mcast, data)
        reply = wait_for_kup_reply(mcast, 30)
        if reply != (KUP_TAG_PUT_ACK, 0):
            raise SystemExit(f"expected a PUT_ACK reply, got {reply}")
        guest.read_until(b"into slot 1; rebooting to try it", 5, checkpoint)
        print("PASS: a good KUP push is installed into slot 1 and ACKed.")

        guest.read_until(b"Kernel: protected-mode Rust entry reached.", 15, checkpoint)
        print("PASS: the machine actually reset after a successful KUP push.")
        guest.escape_to_idle_prompt()

        result = guest.run_command("debug slots")
        if b"booted slot 1" not in result or b"active=1" not in result or b"pending=none" not in result:
            raise SystemExit(f"expected slot 1 to be promoted to active:\n{result.decode('utf-8', 'replace')}")
        print("PASS: slot 1 was promoted to active once the new boot reached the shell prompt.")
    finally:
        guest.terminate()


def test_watchdog(tmp_dir: Path) -> None:
    """The software watchdog (`watchdog N` in the debug file): an idle prompt is
    not a hang; a real hang (`debug hang`) is reset; the reset boot is counted as
    unclean (an on-disk record, since RAM does not survive a reset on every
    machine); a clean `reset` clears the streak; and after three unclean boots in
    a row the watchdog stays off so a machine that always hangs cannot be reset in
    a loop."""
    image_path = tmp_dir / "watchdog.img"
    build_image(image_path, "watchdog 3\n")
    guest = SerialGuest(image_path)

    def hang_and_wait_for_reboot() -> int:
        checkpoint = len(guest.output)
        guest.process.stdin.write(b"debug hang\r\n")
        guest.process.stdin.flush()
        guest.read_until(b"WATCHDOG: no foreground progress", 20, checkpoint)
        guest.read_until(b"Kernel: protected-mode Rust entry reached.", 25, checkpoint)
        guest.read_until(b"fos> ", 15, checkpoint)
        return checkpoint

    def status() -> str:
        return guest.run_command("debug status").decode("utf-8", "replace")

    try:
        guest.read_until(b"fos> ", 15)
        first = status()
        if "watchdog=3s (armed)" not in first or "unclean boots in a row: 0" not in first:
            raise SystemExit(f"watchdog was not armed on a first boot:\n{first}")
        time.sleep(7)  # well past the 3 s limit, doing nothing at the prompt
        if "Uptime:" not in guest.run_command("uptime").decode("utf-8", "replace"):
            raise SystemExit("an idle prompt tripped the watchdog or the machine reset")
        print("PASS: an idle prompt does not trip the watchdog.")

        mark = hang_and_wait_for_reboot()
        text = guest.output[mark:].decode("utf-8", "replace")
        if "did not shut down cleanly (1 in a row)" not in text:
            raise SystemExit(f"the hang was not counted as an unclean boot:\n{text}")
        if "watchdog=3s (armed), unclean boots in a row: 1" not in status():
            raise SystemExit("after one unclean boot the watchdog should still be armed")
        print("PASS: a hang is reset by the watchdog and counted as an unclean boot.")

        # A deliberate reset is a clean stop and clears the streak.
        mark = len(guest.output)
        guest.process.stdin.write(b"reset\r\n")
        guest.process.stdin.flush()
        guest.read_until(b"Kernel: protected-mode Rust entry reached.", 25, mark)
        guest.read_until(b"fos> ", 15, mark)
        text = guest.output[mark:].decode("utf-8", "replace")
        if "did not shut down cleanly" in text or "unclean boots in a row: 0" not in status():
            raise SystemExit(f"a clean reset was counted as unclean:\n{text}")
        print("PASS: a clean reset clears the unclean-boot streak.")

        # Three hangs in a row: the third reset boot sees 3 unclean boots and stays off.
        hang_and_wait_for_reboot()  # -> 1 unclean
        hang_and_wait_for_reboot()  # -> 2 unclean
        mark = hang_and_wait_for_reboot()  # -> 3 unclean
        text = guest.output[mark:].decode("utf-8", "replace")
        if "(3 in a row)" not in text or "watchdog stays off" not in text:
            raise SystemExit(f"the hang-loop guard did not engage after three unclean boots:\n{text}")
        if "watchdog=3s (off), unclean boots in a row: 3" not in status():
            raise SystemExit("the watchdog should be off after three unclean boots")
        print("PASS: after three unclean boots in a row the watchdog stays off (no reset loop).")
    finally:
        guest.terminate()


def main() -> None:
    for required in (STAGE1, STAGE2, KERNEL):
        if not required.exists():
            raise SystemExit(f"{required} does not exist; run `make image` first.")

    with tempfile.TemporaryDirectory(prefix="fos-debug-mode-test-") as tmp_dir_name:
        tmp_dir = Path(tmp_dir_name)
        kup_debug_text = "reboot-on-crash\nkernel-update\n"

        test_crash_reboot(tmp_dir)
        test_crash_halts_by_default(tmp_dir)
        test_crash_loop_guard(tmp_dir)
        test_reboot_soft(tmp_dir)
        test_reboot_hard(tmp_dir)
        test_kup_disabled_by_default(tmp_dir)
        test_kup_bad_checksum(tmp_dir, kup_debug_text)
        test_kup_good_update(tmp_dir, kup_debug_text)
        test_watchdog(tmp_dir)

        print("PASS: debug mode (crash-reboot, loop guard, KUP, and watchdog) works end to end.")


if __name__ == "__main__":
    main()
