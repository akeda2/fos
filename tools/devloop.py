#!/usr/bin/env python3
"""Drive a real fos machine over Ethernet: push a kernel, wait for it to boot, run
shell commands, and load tapes into a chosen NIC -- without the mistakes that
cost time while bringing up the RTL8168 and 3C905 drivers on real hardware.

Needs CAP_NET_RAW like the tools it wraps; run it with the capability-granted
interpreter described in AGENTS.md ("Talking to a real fos machine from a
sandboxed dev environment"), e.g. `~/.local/bin/python3-netraw tools/devloop.py`.
Everything is done by calling the existing tools (`term_net.py`,
`send_tape.py`, `send_kernel_update.py`) as subprocesses, so it adds no wire
logic of its own.

    devloop.py --iface IF --mac MAC wait
    devloop.py --iface IF --mac MAC run "nic list" "irqstat"
    devloop.py --iface IF --mac MAC push build/kernel.bin
    devloop.py --iface IF --mac MAC tape-into 3 build/tapes/audio-rtp.bin

`--mac` is any NIC of the machine (a full MAC or an address-book number).

`tape-into N FILE` loads FILE into NIC number N (as `nic list` numbers them).
The pitfalls it handles, each of which produced a false result by hand:
  * `tape load net` listens only on the *selected* NIC, so it runs `nic use N`
    first and restores the original selection afterwards.
  * A term net session on the NIC being tested is unreliable on real hardware,
    and a disconnect sent to a different NIC than the one the load polls cannot
    end the load. It therefore holds the session on a *different* NIC and never
    kills the client mid-load.
  * "The tape looks the right size" proves nothing -- the previous tape may
    still be loaded -- so success requires the tape handle (`#N` in `tape
    status`) to have advanced as well as the size to match.
  * A gapped transfer leaves the machine waiting for another tape, so a failed
    attempt is retried at a slower pace, and finally a one-byte tape releases
    the loop, so the machine is never left stuck in `tape load net`.
"""

import re
import subprocess
import sys
import tempfile
import time
from argparse import ArgumentParser
from pathlib import Path

TOOLS = Path(__file__).resolve().parent
ROOT = TOOLS.parent
#: A one-byte flat tape (`ret`): the smallest valid image, used to release a
#: `tape load net` that a gapped transfer left waiting.
RELEASE_TAPE = b"\xc3"


class Machine:
    def __init__(self, iface: str | None, mac: str):
        self.iface = iface
        self.mac = mac

    def _iface_args(self) -> list[str]:
        return ["--iface", self.iface] if self.iface else []

    def term(self, commands: list[str], mac: str | None = None, command_timeout: float = 30.0,
             timeout: float = 180.0) -> str:
        """Runs `commands` in one term net session and returns the output."""
        result = subprocess.run(
            [sys.executable, str(TOOLS / "term_net.py"), *self._iface_args(), "--mac", mac or self.mac,
             "--command-timeout", str(command_timeout), *commands],
            capture_output=True, text=True, timeout=timeout)
        return result.stdout

    def start_term(self, commands: list[str], mac: str, command_timeout: float,
                   until: str | None = None) -> subprocess.Popen:
        extra = ["--until", until] if until else []
        return subprocess.Popen(
            [sys.executable, str(TOOLS / "term_net.py"), *self._iface_args(), "--mac", mac,
             "--command-timeout", str(command_timeout), *extra, *commands],
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)

    def send_tape(self, mac: str, path: Path, delay: float) -> bool:
        result = subprocess.run(
            [sys.executable, str(TOOLS / "send_tape.py"), *self._iface_args(), "--mac", mac,
             "--delay", str(delay), str(path)], capture_output=True, text=True, timeout=600)
        return result.returncode == 0

    def nics(self) -> list[tuple[int, str, bool]]:
        """`(number, mac, is_active)` for every NIC, parsed from `nic list`."""
        found = []
        for line in self.term(["nic list"]).splitlines():
            match = re.match(r"([ *])NIC (\d+): .*MAC ((?:[0-9a-f]{2}:){5}[0-9a-f]{2})", line)
            if match:
                found.append((int(match.group(2)), match.group(3), match.group(1) == "*"))
        return found

    def loaded_tape(self, mac: str | None = None) -> tuple[int, int] | None:
        """`(handle, size)` of the loaded tape, or None."""
        match = re.search(r"Tape loaded: #(\d+); (\d+) byte", self.term(["tape status"], mac=mac))
        return (int(match.group(1)), int(match.group(2))) if match else None


def cmd_wait(machine: Machine, args) -> int:
    deadline = time.monotonic() + args.timeout
    while time.monotonic() < deadline:
        out = machine.term(["uptime"], command_timeout=5, timeout=40)
        match = re.search(r"Uptime: ([\d.]+) seconds", out)
        if match:
            print(f"up (uptime {match.group(1)} s)")
            return 0
        time.sleep(3)
    print("machine did not answer in time", file=sys.stderr)
    return 1


def cmd_run(machine: Machine, args) -> int:
    sys.stdout.write(machine.term(args.commands))
    return 0


def cmd_push(machine: Machine, args) -> int:
    kernel = Path(args.kernel)
    result = subprocess.run(
        [sys.executable, str(TOOLS / "send_kernel_update.py"), *machine._iface_args(), "--mac", machine.mac,
         str(kernel)], capture_output=True, text=True)
    tail = (result.stdout.strip().splitlines() or [""])[-1]
    print(tail)
    if "ACK" not in tail:
        print("the machine did not accept the update", file=sys.stderr)
        return 1
    time.sleep(8)  # let it go down before polling for the new boot
    if cmd_wait(machine, args) != 0:
        return 1
    sys.stdout.write(machine.term(args.show))
    return 0


def cmd_tape_into(machine: Machine, args) -> int:
    nics = machine.nics()
    if not nics:
        print("could not read `nic list` from the machine", file=sys.stderr)
        return 1
    by_number = {number: mac for number, mac, _ in nics}
    original = next((number for number, _, active in nics if active), 1)
    if args.nic not in by_number:
        print(f"no NIC {args.nic}; machine has {sorted(by_number)}", file=sys.stderr)
        return 1
    target_mac = by_number[args.nic]
    others = [mac for number, mac in by_number.items() if number != args.nic]
    if not others:
        print("need a second NIC to hold the session on", file=sys.stderr)
        return 1
    session = args.session_mac or others[0]
    path = Path(args.file)
    size = path.stat().st_size

    before = machine.loaded_tape(session)
    machine.term([f"nic use {args.nic}"], mac=session)
    # `tape load net` prints as it waits (relayed live), so a quiet moment does not
    # mean it finished: have the client wait for the "Tape loaded" marker instead.
    client = machine.start_term(["tape load net"], session, command_timeout=args.timeout, until="Tape loaded")
    time.sleep(args.ready_wait)  # let the machine reach its receive loop

    release = Path(tempfile.mkdtemp()) / "release.bin"
    release.write_bytes(RELEASE_TAPE)
    delay = args.delay
    success = False
    for attempt in range(1, args.attempts + 1):
        machine.send_tape(target_mac, path, delay)
        time.sleep(3)
        after = machine.loaded_tape(session)
        if after and after[1] == size and (before is None or after[0] > before[0]):
            success = True
            break
        print(f"attempt {attempt} at {delay}s did not load; retrying slower", file=sys.stderr)
        delay = max(delay * 3, 0.003)  # slower each time, even from an unpaced first try
    if not success:
        machine.send_tape(target_mac, release, 0.003)  # frees the waiting loop
    try:
        client.communicate(timeout=60)
    except subprocess.TimeoutExpired:
        client.kill()
        client.communicate()
    machine.term([f"nic use {original}"], mac=session)
    final = machine.loaded_tape(session)
    print(f"{path.name} ({size} B) into NIC {args.nic} ({target_mac}): "
          f"{'LOADED' if success else 'FAILED'}; tape handle {before} -> {final}")
    return 0 if success else 1


def main() -> int:
    parser = ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--iface", help="host interface (default: auto-detected, but pass it explicitly)")
    parser.add_argument("--mac", required=True, help="any NIC of the machine: a MAC or an address-book number")
    sub = parser.add_subparsers(dest="command", required=True)
    wait = sub.add_parser("wait", help="wait until the machine answers a session")
    wait.add_argument("--timeout", type=float, default=180)
    run = sub.add_parser("run", help="run shell commands in one session")
    run.add_argument("commands", nargs="+")
    push = sub.add_parser("push", help="push a kernel via KUP, wait for the reboot")
    push.add_argument("kernel")
    push.add_argument("--timeout", type=float, default=240)
    push.add_argument("--show", nargs="+", default=["debug slots", "nic list"], help="commands to run after boot")
    tape = sub.add_parser("tape-into", help="load a tape into NIC N, safely")
    tape.add_argument("nic", type=int, help="NIC number as `nic list` shows it (1-based)")
    tape.add_argument("file")
    tape.add_argument("--delay", type=float, default=0.003, help="inter-frame delay of the first attempt")
    tape.add_argument("--attempts", type=int, default=3)
    tape.add_argument("--session-mac", help="MAC to hold the session on (default: another NIC)")
    tape.add_argument("--ready-wait", type=float, default=7.0, help="seconds to let `tape load net` start")
    tape.add_argument("--timeout", type=float, default=90.0, help="how long the session waits for the load")
    args = parser.parse_args()
    machine = Machine(args.iface, args.mac)
    return {"wait": cmd_wait, "run": cmd_run, "push": cmd_push, "tape-into": cmd_tape_into}[args.command](machine, args)


if __name__ == "__main__":
    sys.exit(main())
