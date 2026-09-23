#!/usr/bin/env python3
"""End-to-end test of debug mode driven by the *real* host-side tools
(`tools/term_net.py`, `tools/send_kernel_update.py`) over a genuine tap
network interface, instead of the raw-UDP-multicast protocol
reimplementation `tools/test_debug_mode_e2e.py`/`tools/test_kernel_update_e2e.py`
use to avoid needing root. This is the "does this actually work the way
an operator would use it against real hardware" check for debug mode --
those other two scripts already prove the *kernel/protocol* side works;
this one proves the *client tooling* side does too, address resolution,
framing, and all, with nothing standing in for the real thing.

It also demonstrates the "try a new NIC driver as a second interface in
QEMU before ever touching physical hardware" workflow: the guest's second
NIC is a real, QEMU-emulated `ne2k_pci` (NE2000-compatible) card, proving
a from-scratch driver addition can be exercised address-for-address (`nic
list` showing two interfaces, KUP/`term net` still working over the
primary one) without needing real hardware at all. (Intel's I350 has no
QEMU model at all, so this pattern can't cover *that* driver directly --
see AGENTS.md's "Intel I350 support" section -- but it covers any NIC
family QEMU does emulate, which is every other backend this kernel has.)

Requirements this script cannot set up for you, both one-time,
documented in AGENTS.md's "Talking to a real fos machine from a
sandboxed dev environment" section:

1. A persistent tap interface owned by your own user (creating one needs
   root once; using it afterward does not):

       sudo ip tuntap add dev fostap0 mode tap user "$(whoami)"
       sudo ip link set fostap0 up

2. A `python3` copy with `CAP_NET_RAW` (needed for `AF_PACKET` raw
   sockets, regardless of which interface they bind to):

       cp "$(command -v python3)" ~/.local/bin/python3-netraw
       sudo setcap cap_net_raw+eip ~/.local/bin/python3-netraw

Not part of `make test`/`make test-debug-mode`: unlike every other
automated test in this project, this one depends on host machine state
(the tap interface, the capability-tagged Python copy) that cannot be
assumed present, e.g. in a fresh checkout or a CI runner. Run it
directly once both one-time steps above are done:

    python3 tools/test_realnet_e2e.py [--iface fostap0] [--python3-netraw PATH]
"""

import argparse
import os
import selectors
import shutil
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
SEND_KERNEL_UPDATE = ROOT / "tools" / "send_kernel_update.py"
TERM_NET = ROOT / "tools" / "term_net.py"

GUEST_MAC1 = "52:54:00:aa:bb:01"  # primary NIC (rtl8139), on the tap
GUEST_MAC2 = "52:54:00:aa:bb:02"  # secondary NIC (ne2k_pci), QEMU "user" backend


def require(condition: bool, message: str) -> None:
    if not condition:
        raise SystemExit(message)


def build_image(image_path: Path, debug_text: str) -> None:
    debug_file = image_path.with_suffix(".debug.txt")
    debug_file.write_text(debug_text)
    subprocess.run(
        [
            sys.executable, str(BUILD_IMAGE),
            "--stage1", str(STAGE1), "--stage2", str(STAGE2), "--kernel", str(KERNEL),
            "--output", str(image_path),
            "--file", "debug", str(debug_file),
        ],
        check=True,
    )


class TapGuest:
    """A QEMU guest with a real primary NIC bridged to a host tap
    interface (real host connectivity, driven by the actual host tools)
    plus a second NIC on QEMU's own unconnected "user" backend, purely to
    prove a second driver/interface is initialized and listed correctly
    -- this second NIC never needs to exchange a single frame with the
    host for that purpose."""

    def __init__(self, image_path: Path, monitor_sock: Path, tap_iface: str):
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
                "-netdev", f"tap,id=net0,ifname={tap_iface},script=no,downscript=no",
                "-device", f"rtl8139,netdev=net0,mac={GUEST_MAC1}",
                "-netdev", "user,id=net1",
                "-device", f"ne2k_pci,netdev=net1,mac={GUEST_MAC2}",
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

    def read_until_any(self, markers: list[bytes], timeout: float, start: int = 0) -> bytes:
        deadline = time.monotonic() + timeout
        while not any(marker in self.output[start:] for marker in markers):
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise SystemExit(
                    f"timed out waiting for any of {markers!r}; output since checkpoint:\n"
                    f"{self.output[start:].decode('utf-8', 'replace')}"
                )
            for key, _ in self.selector.select(remaining):
                chunk = os.read(key.fd, 4096)
                if not chunk:
                    raise SystemExit(f"guest serial stream ended early:\n{self.output.decode('utf-8', 'replace')}")
                self.output.extend(chunk)
        return bytes(self.output[start:])

    def escape_to_idle_prompt(self) -> None:
        """`term_net` runs automatically at every boot and blocks waiting
        for a `TERM_TAG_HELLO`; KUP/an ordinary idle prompt is only
        reachable via local escape (F12), simulated here through the
        QEMU monitor -- this is the one place this script still needs
        QEMU's own monitor rather than a real host tool, since a serial
        or network client has no equivalent "give up" signal (see
        AGENTS.md's "term net at boot" section).

        `term_net`'s own "waiting for a HELLO" banner differs between the
        very first wait ("Waiting for a remote terminal client to
        connect") and a later re-arm after a remote session disconnects
        ("...waiting for a new one to connect"), so both are accepted
        here."""
        import socket as socketlib

        start = len(self.output)
        self.read_until_any(
            [b"Waiting for a remote terminal client to connect", b"waiting for a new one to connect"],
            20,
            start,
        )
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

    def terminate(self) -> None:
        self.process.terminate()
        try:
            self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.process.kill()


def run_host_tool(python3_netraw: str, script: Path, iface: str, mac: str, args: list[str], timeout: float) -> str:
    """Runs a real host-side tool (needs CAP_NET_RAW for its raw
    AF_PACKET socket) as a subprocess and returns its captured stdout."""
    command = [python3_netraw, str(script), "--iface", iface, "--mac", mac, *args]
    result = subprocess.run(command, capture_output=True, timeout=timeout, text=True)
    if result.returncode != 0:
        raise SystemExit(
            f"{script.name} failed (exit {result.returncode}):\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        )
    return result.stdout


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--iface", default="fostap0", help="Host tap interface name (default: fostap0).")
    parser.add_argument(
        "--python3-netraw",
        default=str(Path.home() / ".local" / "bin" / "python3-netraw"),
        help="Path to a python3 copy with CAP_NET_RAW (see this script's own docstring).",
    )
    args = parser.parse_args()

    require(shutil.which("qemu-system-i386") is not None, "qemu-system-i386 not found on PATH.")
    require(KERNEL.exists(), f"{KERNEL} not found; run `make image` first.")
    require(
        subprocess.run(["ip", "link", "show", args.iface], capture_output=True).returncode == 0,
        f"tap interface {args.iface!r} not found. One-time setup (see this script's docstring):\n"
        f"  sudo ip tuntap add dev {args.iface} mode tap user \"$(whoami)\"\n"
        f"  sudo ip link set {args.iface} up",
    )
    require(
        os.path.exists(args.python3_netraw),
        f"{args.python3_netraw} not found. One-time setup (see this script's docstring):\n"
        f"  cp \"$(command -v python3)\" {args.python3_netraw}\n"
        f"  sudo setcap cap_net_raw+eip {args.python3_netraw}",
    )

    with tempfile.TemporaryDirectory() as tmp:
        tmp_dir = Path(tmp)
        image_path = tmp_dir / "realnet.img"
        build_image(image_path, "kernel-update\n")

        guest = TapGuest(image_path, tmp_dir / "monitor.sock", args.iface)
        try:
            guest.read_until(b"Kernel: protected-mode Rust entry reached.", 15)
            # No F12 escape needed here: `term_net` runs automatically at
            # every boot and already waits for a `TERM_TAG_HELLO`, so the
            # real `term_net.py` can connect directly. Escaping to the
            # idle prompt first would actually make this *slower* and
            # flakier: after a local escape, the shell only restarts
            # `term_net`'s own HELLO wait after TERM_NET_IDLE_TICKS (60s)
            # of local idle time (`kernel/src/shell.rs`'s `run` loop), so
            # a command sent shortly after escaping would see no listener
            # at all until that 60s window elapses.
            guest.read_until(b"Waiting for a remote terminal client to connect", 10)

            # Real `tools/term_net.py`, over the real tap, talking to the
            # real guest: confirm both NICs are up and correctly
            # identified before touching KUP.
            nic_output = run_host_tool(
                args.python3_netraw, TERM_NET, args.iface, GUEST_MAC1, ["nic list"], timeout=20,
            )
            require(b"RTL8139".decode() in nic_output, f"expected RTL8139 in `nic list` output:\n{nic_output}")
            require(
                "NE2000-compatible" in nic_output,
                f"expected the second NIC (NE2000-compatible/ne2k_pci) in `nic list` output:\n{nic_output}",
            )
            print("PASS: real term_net.py sees both NICs (RTL8139 on the tap, NE2000-compatible as nic2).")

            # KUP is only served from the idle prompt loop's ambient
            # discovery poll, never from inside an active `term_net`
            # session (see AGENTS.md's KUP section) -- escape to the
            # local idle prompt before pushing the update.
            guest.escape_to_idle_prompt()

            # Real `tools/send_kernel_update.py`, over the real tap,
            # pushing the actual freshly built kernel.
            checkpoint = len(guest.output)
            kup_output = run_host_tool(
                args.python3_netraw, SEND_KERNEL_UPDATE, args.iface, GUEST_MAC1, [str(KERNEL)], timeout=30,
            )
            require("ACK" in kup_output or "installed" in kup_output.lower(), f"unexpected send_kernel_update.py output:\n{kup_output}")
            print("PASS: real send_kernel_update.py pushed the kernel and received an ACK.")

            guest.read_until(b"rebooting to try it", 10, checkpoint)
            guest.read_until(b"Kernel: protected-mode Rust entry reached.", 20, checkpoint)
            print("PASS: the guest actually reset after the real KUP push.")

            # A fresh boot means `term_net`'s automatic HELLO-wait is
            # already running (see the comment above the first `nic
            # list` check) -- no F12 escape needed here either.
            guest.read_until(b"Waiting for a remote terminal client to connect", 10, checkpoint)
            slots_output = run_host_tool(
                args.python3_netraw, TERM_NET, args.iface, GUEST_MAC1, ["debug slots"], timeout=20,
            )
            require("booted slot 1" in slots_output, f"expected the new kernel to have booted from slot 1:\n{slots_output}")
            require("active=1" in slots_output, f"expected slot 1 promoted to active:\n{slots_output}")
            require("pending=none" in slots_output, f"expected no pending slot after promotion:\n{slots_output}")
            print("PASS: real term_net.py confirms slot 1 was promoted to active after the real KUP push.")

            print(
                "PASS: debug mode works end to end when driven entirely by the real host "
                "tools (term_net.py, send_kernel_update.py) over a genuine tap interface, "
                "alongside a second, QEMU-emulated NIC (ne2k_pci) standing in for a new "
                "driver under development."
            )
        finally:
            guest.terminate()


if __name__ == "__main__":
    main()
