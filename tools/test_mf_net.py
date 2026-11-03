#!/usr/bin/env python3
"""The server kernel's network console and KUP (docs/big-kernel.md, slice 6), through the same wire formats the embedded
kernel speaks (the harness is tools/test_debug_mode.py's Machine, pointed at the kernel-mf ELF):

 1. Discovery: the guest answers a probe and announces itself as `fos-<last three MAC bytes>`.
 2. A `term net` session: HELLO, the `fos> ` prompt, commands, output going back over the wire.
 3. A bad-checksum KUP is rejected and leaves the slot metadata alone.
 4. KUP with the same kernel: ACK, reset, the loader boots slot 1, the kernel confirms and promotes it.
 5. Without `kernel-update` the KUP frames are not answered; `deny-term-net` closes the terminal.
QEMU only: real NIC DMA/interrupts, a real reset and the loader's fallback on silicon are for hardware."""

import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import test_debug_mode as base  # noqa: E402
from test_boot import BROADCAST, TAG_DISCOVER_REPLY, TAG_DISCOVER_REQUEST, TAG_HELLO, ROOT, check  # noqa: E402

base.KERNEL = ROOT / "build/cargo-target/x86_64-fos-kernel/release/fosfor-kernel-mf"
READY = "entering the service loop"


def check_screen(machine, path):
    """The framebuffer console: a screendump shows the boot narration (text rows, and the prompt's row at the bottom of it)."""
    import socket
    time.sleep(0.5)
    monitor = socket.socket(socket.AF_UNIX)
    monitor.connect(str(machine.monitor))
    monitor.settimeout(5)
    monitor.recv(4096)
    monitor.send(f"screendump {path}\n".encode())
    for _ in range(50):
        machine.pump(0.1)
        if path.exists() and path.stat().st_size > 1000:
            break
    monitor.close()
    data = path.read_bytes()
    width, height = (int(v) for v in data.split(b"\n", 2)[1].split())
    pixels = data[len(f"P6\n{width} {height}\n255\n"):]
    lit = [any(pixels[y * width * 3:(y + 1) * width * 3]) for y in range(height)]
    check(sum(lit) > 100, f"the framebuffer console draws the boot narration ({sum(lit)} lit pixel rows of {height})", "")
    check("Framebuffer console:" in machine.text() and "characters" in machine.text(), "and says so on the serial console", machine.text()[-1500:])
    check("Framebuffer: write-combining (PAT)" in machine.text(), "mapped write-combining through the PAT", "\n".join(l for l in machine.text().splitlines() if "ramebuffer" in l))


def red_pixels(machine, path):
    """Screendump again and count pure ANSI red (0xaa0000) pixels."""
    import socket
    path.unlink(missing_ok=True)
    monitor = socket.socket(socket.AF_UNIX)
    monitor.connect(str(machine.monitor))
    monitor.settimeout(5)
    monitor.recv(4096)
    monitor.send(f"screendump {path}\n".encode())
    for _ in range(50):
        machine.pump(0.1)
        if path.exists() and path.stat().st_size > 1000:
            break
    monitor.close()
    data = path.read_bytes()
    width, height = (int(v) for v in data.split(b"\n", 2)[1].split())
    pixels = data[len(f"P6\n{width} {height}\n255\n"):]
    return pixels.count(bytes([0xaa, 0, 0]))


def main():
    kernel = base.KERNEL.read_bytes()
    with tempfile.TemporaryDirectory() as tmp:
        machine = base.Machine(tmp, {"debug": "kernel-update\n"})
        machine.monitor = Path(tmp) / "monitor"
        machine.extra = ["-vga", "std"]
        machine.start()
        try:
            machine.wait_for(READY)
            check_screen(machine, Path(tmp) / "screen.ppm")
            check("debug: kernel-update" in machine.text(), "the debug file is read from the boot volume", machine.text()[-1500:])
            check("interrupts: Intx" in machine.text(), "the e1000 came up and proved its interrupt", machine.text()[-1500:])

            names = []
            for _ in range(40):
                machine.wire.send(BROADCAST, TAG_DISCOVER_REQUEST, b"host")
                names = [payload for _, tag, payload in machine.wire.frames(0.25) if tag == TAG_DISCOVER_REPLY]
                if names:
                    break
            check(any(name.rstrip(b"\0") == b"fos-12347b" for name in names), f"discovery: announced as fos-<last 3 MAC bytes> ({names})", "")

            machine.connect()
            out = machine.run("echo hello from the wire")
            check("hello from the wire" in out, "a terminal session: HELLO, prompt, a command and its output", out)
            machine.run("debug colors")
            check(red_pixels(machine, Path(tmp) / "ansi.ppm") > 20, "ANSI colour: SGR 31 draws red text on the framebuffer", "")
            out = machine.run("cpus")
            check("CPU(s)" in out, "the shell answers over the network", out)
            before = machine.run("slots")
            check("booted slot 0, active 0, pending None" in before, "a fresh image boots slot 0", before)

            result = machine.push(kernel, checksum=0x1234)
            check(result == ("error", 2), f"KUP with a bad checksum is rejected ({result})", "")
            after = machine.run("slots")
            check("pending None" in after, "and leaves the slot metadata untouched", after)

            mark = len(machine.text())
            result = machine.push(kernel)
            check(result == "ack", f"KUP with the real kernel is verified, written and acknowledged ({result})", machine.text()[-1500:])
            third = machine.wait_for(READY, 90, mark)
            check("booted from slot 1" in third, "after the reset the loader boots the new kernel from slot 1", third)
            check("Kernel update confirmed: slot 1" in third, "and the kernel confirms it, promoting it", third)
            machine.connect()
            out = machine.run("slots")
            check("booted slot 1, active 1, pending None" in out, "slot 1 is now the active slot, nothing pending", out)
        finally:
            machine.stop()

    # `irq-cpu 1`: the NIC's interrupt (self-test, then every frame) is delivered to CPU 1 and the terminal still works.
    with tempfile.TemporaryDirectory() as tmp:
        machine = base.Machine(tmp, {"settings": "irq-cpu 1\n"})
        machine.extra = ["-smp", "4"]
        machine.start()
        try:
            machine.wait_for(READY)
            text = machine.text()
            check("irq-cpu 1" in text, "settings: irq-cpu 1 is read", text[-1500:])
            check("interrupts: Intx" in text or "interrupts: Msi" in text, "the NIC proved its interrupt path with the target on CPU 1", text[-1500:])
            machine.connect()
            out = machine.run("echo routed")
            check("routed" in out, "the network terminal works with interrupts on CPU 1", out)
            out = machine.run("cpus")
            check("4 CPU(s)" in out, "on a 4 CPU machine", out)
        finally:
            machine.stop()

    # No kernel-update: KUP frames are not answered; deny-term-net: no terminal.
    with tempfile.TemporaryDirectory() as tmp:
        machine = base.Machine(tmp, {"settings": "deny-term-net\n"})
        machine.start()
        try:
            machine.wait_for(READY)
            machine.wire.send(BROADCAST, TAG_DISCOVER_REQUEST, b"host")
            guest = next((source for source, tag, _ in machine.wire.frames(2) if tag == TAG_DISCOVER_REPLY), None)
            check(guest is not None, "a machine with deny-term-net still answers discovery", "")
            machine.wire.send(guest, base.KUP_REQUEST, (4).to_bytes(4, "little") + b"\0\0")
            replies = [tag for _, tag, _ in machine.wire.frames(1.5) if tag in (base.KUP_ACK, base.KUP_ERROR)]
            check(not replies, "KUP is not answered without kernel-update", str(replies))
            machine.wire.send(guest, TAG_HELLO)
            time.sleep(1)
            machine.pump(0.5)
            check("Remote terminal connected" not in machine.text(), "deny-term-net refuses the terminal", machine.text()[-800:])
        finally:
            machine.stop()
    print("test-mf-net: all checks passed")


if __name__ == "__main__":
    main()
