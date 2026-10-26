#!/usr/bin/env python3
"""fosadm: find fos machines on the network and show what they run.

  fosadm.py hosts                  discover machines (forgets stale peers first)
  fosadm.py status [HOST...]       kernel slot and tape sizes against the build
  fosadm.py deploy [HOST...]       push the kernel (KUP) and changed tapes, after showing a plan
  fosadm.py run|put|shell|bootcode HOST ...   one command, a file, a terminal, the boot code

Machines announce `hostname` + NIC number (skylake3 is NIC 3 of skylake), so the
NICs of one machine are grouped under one host name. Tapes are compared by size
against --from DIR (default: the build tree: build/fosfor/tapes and the release
kernel), because stored tapes are staged images whose bytes differ.

Raw Ethernet needs CAP_NET_RAW; if it is missing the tool says how to grant it
to a dedicated Python copy (it never does so silently).
"""
import argparse
import math
import os
import re
import shutil
import socket
import subprocess
import sys
from pathlib import Path

TOOLS = Path(__file__).resolve().parent
ROOT = TOOLS.parent
sys.path.insert(0, str(TOOLS))
import send_tape  # noqa: E402

BUILD_KERNEL = ROOT / "build/cargo-target/x86_64-fos-kernel/release/fosfor-kernel"
BUILD_TAPES = ROOT / "build/fosfor/tapes"
BUILD_KERNEL_A64 = ROOT / "build/cargo-target/aarch64-unknown-none-softfloat/release/fosfor-kernel"
BUILD_TAPES_A64 = ROOT / "build/fosfor-aarch64/tapes"
NETRAW = Path.home() / ".local/bin/python3-netraw"


def has_raw_socket() -> bool:
    try:
        socket.socket(socket.AF_PACKET, socket.SOCK_RAW, socket.htons(3)).close()
        return True
    except PermissionError:
        return False


def ensure_privilege() -> None:
    """Returns when raw sockets work; otherwise explains and, on a typed yes, re-runs under a capable Python."""
    if has_raw_socket():
        return
    if NETRAW.exists() and os.environ.get("FOSADM_REEXEC") != "1":
        os.environ["FOSADM_REEXEC"] = "1"
        os.execv(str(NETRAW), [str(NETRAW), *sys.argv])
    print("fosadm talks to machines with raw Ethernet frames, which needs the CAP_NET_RAW capability.\n"
          "The safe way is a dedicated copy of Python that holds only that capability:\n\n"
          f"  cp \"$(command -v python3)\" {NETRAW}\n"
          f"  sudo setcap cap_net_raw+eip {NETRAW}\n")
    if sys.stdin.isatty() and shutil.which("sudo"):
        if input("Run these two commands now (sudo will ask for your password)? [y/N] ").strip().lower() == "y":
            subprocess.run(["cp", shutil.which("python3"), str(NETRAW)], check=True)
            subprocess.run(["sudo", "setcap", "cap_net_raw+eip", str(NETRAW)], check=True)
            os.environ["FOSADM_REEXEC"] = "1"
            os.execv(str(NETRAW), [str(NETRAW), *sys.argv])
    raise SystemExit("no raw-socket permission; nothing was changed.")


def load_config() -> dict:
    """`hosts.toml` from ./ and ~/.config/fosadm/ (the first found). Example:

        iface = "enp0s31f6"
        [host.amd1]
        note = "master clock"
        restart = "start"     # run on the host after a deploy that stopped a tape
        skip = false          # true: deploy leaves it alone
    """
    import tomllib
    for path in (Path("hosts.toml"), Path.home() / ".config/fosadm/hosts.toml"):
        if path.is_file():
            try:
                return tomllib.loads(path.read_text())
            except tomllib.TOMLDecodeError as error:
                raise SystemExit(f"{path}: {error}")
    return {}


CONFIG: dict = {}


def host_setting(host: "Host", key: str, default=None):
    return CONFIG.get("host", {}).get(host.name, {}).get(key, default)


def machine_name(name: str) -> str:
    """`skylake3` -> `skylake`: the NIC number is appended to the hostname."""
    return re.sub(r"\d+$", "", name) or name


class Host:
    def __init__(self, name: str):
        self.name = name
        self.nics: dict[int, bytes] = {}

    @property
    def mac(self) -> bytes:
        return self.nics[min(self.nics)]

    def label(self) -> str:
        return ", ".join(f"{n}={send_tape.format_mac(m)}" for n, m in sorted(self.nics.items()))


def discover(iface: str, keep: bool, timeout: float) -> list[Host]:
    if not keep:
        send_tape.PEERS_CACHE_FILE.unlink(missing_ok=True)
        send_tape.forget_peer_names()
    source = send_tape.interface_mac(iface)
    with socket.socket(socket.AF_PACKET, socket.SOCK_RAW, socket.htons(3)) as raw:
        raw.bind((iface, 0))
        macs = send_tape.discover(raw, iface, source, timeout)
        macs += [m for m in send_tape.discover(raw, iface, source, 0.5) if m not in macs]
    names = send_tape.load_peer_names()
    peers = send_tape.load_known_peers()
    for mac in macs:
        send_tape.remember_known_peer(peers, mac)
    send_tape.save_known_peers(peers)
    hosts: dict[str, Host] = {}
    for mac in macs:
        full = names.get(mac, send_tape.format_mac(mac))
        base = machine_name(full)
        number = int(m.group()) if (m := re.search(r"\d+$", full)) else 1
        hosts.setdefault(base, Host(base)).nics[number] = mac
    return sorted(hosts.values(), key=lambda h: h.name)


def pick(hosts: list[Host], wanted: list[str]) -> list[Host]:
    if not wanted:
        return hosts
    chosen = []
    for text in wanted:
        match = [h for h in hosts if h.name == text.lower()
                 or any(send_tape.format_mac(m) == text.lower() for m in h.nics.values())]
        if not match:
            raise SystemExit(f"no discovered host matches {text!r}; known: {', '.join(h.name for h in hosts) or 'none'}")
        chosen += match
    return chosen


def ask_host(iface: str, host: Host, commands: list[str], timeout: float = 60) -> str:
    result = subprocess.run(
        [sys.executable, str(TOOLS / "term_net.py"), "--iface", iface, "--mac", send_tape.format_mac(host.mac),
         "--command-timeout", "10", *commands], capture_output=True, text=True, timeout=timeout)
    return result.stdout


def parse_ls(text: str) -> dict[str, int]:
    return {m.group(1): int(m.group(2)) for m in re.finditer(r"^([a-z0-9._-]+)\s+(\d+) bytes", text, re.M)}


def source_files(source: Path | None, arch: str = "x86_64") -> tuple[Path | None, Path, dict[str, int]]:
    """Kernel, tape folder and tape sizes for one CPU. A release folder holds one sub-folder per CPU
    (`x86_64/`, `aarch64/`, each with `kernel` and `tapes/`); a flat folder is taken as x86_64."""
    if source is None:
        kernel, tapes = (BUILD_KERNEL_A64, BUILD_TAPES_A64) if arch == "aarch64" else (BUILD_KERNEL, BUILD_TAPES)
    else:
        base = source / arch if (source / arch).is_dir() else (source if arch == "x86_64" else source / "-none-")
        kernel, tapes = base / "kernel", base / "tapes"
        if not kernel.exists():
            kernel = base / "fosfor-kernel"
        if not tapes.is_dir():
            tapes = base
    sizes = {p.name: p.stat().st_size for p in tapes.iterdir() if p.is_file() and not p.name.startswith(".")} \
        if tapes.is_dir() else {}
    return (kernel if kernel.exists() else None), tapes, sizes


def kernel_sectors(kernel: Path | None) -> int | None:
    return math.ceil(kernel.stat().st_size / 512) if kernel else None


def arch_of(text: str) -> str:
    return "aarch64" if "aarch64" in text else "x86_64"


def cmd_hosts(args, iface: str) -> int:
    hosts = discover(iface, args.keep, args.timeout)
    if not hosts:
        print("no fos machines answered.")
        return 1
    for host in hosts:
        note = host_setting(host, "note")
        print(f"{host.name:<14} NIC {host.label()}" + (f"  # {note}" if note else ""))
    return 0


def cmd_status(args, iface: str) -> int:
    hosts = pick(discover(iface, args.keep, args.timeout), args.hosts)
    for host in hosts:
        out = ask_host(iface, host, ["debug slots", "ls", "about"])
        print(f"{host.name} ({host.label()})")
        arch = arch_of(out)
        kernel, _, tapes = source_files(Path(args.source) if args.source else None, arch)
        want_sectors = kernel_sectors(kernel)
        booted = re.search(r"booted slot: (\d+)", out)
        sizes = re.search(r"slot sizes \(sectors\): (\d+) / (\d+)", out)
        files = parse_ls(out)
        if not booted or not sizes:
            print("  no usable answer (is a tape running? try again or stop it first)")
            continue
        have = int(sizes.group(1 + int(booted.group(1))))
        state = "unknown" if want_sectors is None else ("current" if have == want_sectors else f"differs (build {want_sectors})")
        print(f"  {arch} kernel: slot {booted.group(1)}, {have} sectors, {state}")
        old = [n for n, s in tapes.items() if n in files and files[n] != s]
        missing = [n for n in tapes if n not in files]
        print(f"  tapes: {sum(1 for n in tapes if n in files) - len(old)} current, {len(old)} differ, {len(missing)} missing")
        if old:
            print(f"    differ: {' '.join(sorted(old))}")
        if args.verbose and missing:
            print(f"    missing: {' '.join(sorted(missing))}")
    return 0


def stop_running_tape(iface: str, host: Host) -> None:
    """A lone ESC through term net ends a foreground tape (ptp from a start file, say); harmless at an idle prompt."""
    send_tape_cmd = (f"(printf '\\x1b'; sleep 2) | {sys.executable} {TOOLS / 'term_net.py'} --iface {iface} "
                     f"--mac {send_tape.format_mac(host.mac)} --interactive")
    subprocess.run(["bash", "-c", send_tape_cmd], capture_output=True, timeout=30)


def probe(iface: str, host: Host) -> tuple[str, int, dict[str, int], bool] | None:
    """`(cpu, booted slot's sectors, files, stopped_a_tape)`; sends ESC once if the shell does not answer."""
    stopped = False
    for attempt in range(2):
        out = ask_host(iface, host, ["debug slots", "ls", "about"])
        booted = re.search(r"booted slot: (\d+)", out)
        sizes = re.search(r"slot sizes \(sectors\): (\d+) / (\d+)", out)
        if booted and sizes:
            return arch_of(out), int(sizes.group(1 + int(booted.group(1)))), parse_ls(out), stopped
        if attempt == 0:
            stop_running_tape(iface, host)
            stopped = True
    return None


def wait_up(iface: str, host: Host, previous_uptime_gone_for: float = 240) -> tuple[int, int] | None:
    """Waits for the host to go down and come back; returns (booted slot, sectors) of the new boot."""
    import time
    time.sleep(10)
    deadline = time.monotonic() + previous_uptime_gone_for
    while time.monotonic() < deadline:
        found = probe_quiet(iface, host)
        if found:
            return found
        time.sleep(5)
    return None


def probe_quiet(iface: str, host: Host) -> tuple[int, int] | None:
    out = ask_host(iface, host, ["debug slots"], timeout=40)
    booted = re.search(r"booted slot: (\d+)", out)
    sizes = re.search(r"slot sizes \(sectors\): (\d+) / (\d+)", out)
    if booted and sizes:
        return int(booted.group(1)), int(sizes.group(1 + int(booted.group(1))))
    return None


def cmd_deploy(args, iface: str) -> int:
    do_tapes = not args.kernel_only
    hosts = pick(discover(iface, args.keep, args.timeout), args.hosts)
    plans = []
    for host in hosts:
        if host_setting(host, "skip", False):
            print(f"{host.name}: skipped (skip = true in hosts.toml)")
            continue
        state = probe(iface, host)
        if state is None:
            print(f"{host.name}: no answer, skipped")
            continue
        arch, have, files, stopped = state
        kernel, tape_dir, tapes = source_files(Path(args.source) if args.source else None, arch)
        do_kernel = kernel is not None and not args.tapes_only
        want = kernel_sectors(kernel)
        new_kernel = do_kernel and (args.force_kernel or have != want)
        tape_list = sorted(n for n, s in tapes.items() if do_tapes and n in files and files[n] != s) \
            + (sorted(n for n in tapes if do_tapes and n not in files) if args.add_missing else [])
        plans.append((host, new_kernel, tape_list, stopped, arch))
    print("Plan:")
    for host, new_kernel, tape_list, _, arch in plans:
        what = (["kernel"] if new_kernel else []) + [f"{len(tape_list)} tape(s): {' '.join(tape_list)}"] * bool(tape_list)
        print(f"  {host.name:<14} {arch:<8} {'; '.join(what) or 'up to date'}")
    todo = [p for p in plans if p[1] or p[2]]
    if not todo:
        return 0
    if args.dry_run:
        return 0
    if not args.yes and input("Go ahead? [y/N] ").strip().lower() != "y":
        print("nothing was changed.")
        return 1
    failed = 0
    for host, new_kernel, tape_list, stopped, arch in todo:
        mac = send_tape.format_mac(host.mac)
        kernel, tape_dir, tapes = source_files(Path(args.source) if args.source else None, arch)
        want = kernel_sectors(kernel)
        print(f"== {host.name}")
        if new_kernel:
            result = subprocess.run([sys.executable, str(TOOLS / "send_kernel_update.py"), "--iface", iface, "--mac", mac,
                                     str(kernel)], capture_output=True, text=True)
            if "ACK" not in result.stdout:
                print(f"  kernel: not accepted ({(result.stdout + result.stderr).strip().splitlines()[-1:]}); skipped")
                failed += 1
                continue
            print("  kernel: accepted, waiting for the reboot...")
            after = wait_up(iface, host)
            if after is None:
                print("  kernel: the host did not come back in time; check it by hand")
                failed += 1
                continue
            if after[1] != want:
                print(f"  kernel: FELL BACK to the old kernel (slot {after[0]}, {after[1]} sectors); tapes not touched")
                failed += 1
                continue
            print(f"  kernel: running (slot {after[0]}, {after[1]} sectors)")
        if tape_list:
            state = probe(iface, host)
            stopped = stopped or (state is not None and state[3])
            if state is None:
                print("  tapes: host does not answer; skipped")
                failed += 1
                continue
            for name in tape_list:
                result = subprocess.run([sys.executable, str(TOOLS / "foscopy.py"), "--iface", iface, "--mac", mac,
                                         str(tape_dir / name), f":disk1/{name}"], capture_output=True, text=True)
                print(f"  tape {name}: {'ok' if result.returncode == 0 else 'FAILED'}")
                failed += result.returncode != 0
            files = parse_ls(ask_host(iface, host, ["ls"]))
            bad = [n for n in tape_list if files.get(n) != tapes[n]]
            print("  verified by size" if not bad else f"  size mismatch after copy: {' '.join(bad)}")
            failed += bool(bad)
        restart = host_setting(host, "restart")
        if stopped and restart:
            subprocess.run([sys.executable, str(TOOLS / "term_net.py"), "--iface", iface, "--mac", mac,
                            "--command-timeout", "8", restart], capture_output=True, timeout=60)
            print(f"  a running tape was stopped; ran `{restart}` to bring it back (hosts.toml).")
        elif stopped:
            print("  a running tape was stopped; run `start` on the host (or reboot it) to bring it back.")
    return 1 if failed else 0


def one_host(iface: str, args) -> Host:
    found = pick(discover(iface, args.keep, args.timeout), [args.host])
    if len(found) != 1:
        raise SystemExit(f"{args.host!r} matches {len(found)} hosts; use a MAC address")
    return found[0]


def cmd_run(args, iface: str) -> int:
    host = one_host(iface, args)
    sys.stdout.write(ask_host(iface, host, [" ".join(args.words)], timeout=300))
    return 0


def cmd_put(args, iface: str) -> int:
    host = one_host(iface, args)
    name = args.name or Path(args.file).name
    result = subprocess.run([sys.executable, str(TOOLS / "foscopy.py"), "--iface", iface, "--mac",
                             send_tape.format_mac(host.mac), args.file, f":disk{args.disk}/{name}"])
    return result.returncode


def cmd_shell(args, iface: str) -> int:
    host = one_host(iface, args)
    return subprocess.run([sys.executable, str(TOOLS / "term_net.py"), "--iface", iface, "--mac",
                           send_tape.format_mac(host.mac), "--interactive"]).returncode


def cmd_bootcode(args, iface: str) -> int:
    host = one_host(iface, args)
    if not args.update:
        sys.stdout.write(ask_host(iface, host, ["bootcode"]))
        return 0
    print(f"This rewrites the boot code of {host.name} (stage1.bin/stage2.bin or loader.efi must already be on its\n"
          "boot volume). A power cut in the middle can leave a machine that needs a stick to recover.")
    if input(f"Type the host name ({host.name}) to continue: ").strip() != host.name:
        print("nothing was changed.")
        return 1
    # `confirm` must be the very next command, in the same session.
    sys.stdout.write(ask_host(iface, host, ["bootcode update", "bootcode update confirm"], timeout=120))
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--iface", help="network interface (default: auto-detected)")
    parser.add_argument("--keep", "--no-discover-forget", dest="keep", action="store_true",
                        help="keep the remembered peers instead of forgetting them before discovery")
    parser.add_argument("--timeout", type=float, default=2.5, help="seconds to listen for announcements")
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("hosts", help="list the machines found on the network")
    status = sub.add_parser("status", help="show kernel and tape state against the build")
    status.add_argument("hosts", nargs="*", help="host names or MACs (default: all)")
    status.add_argument("--from", dest="source", help="release folder (default: the build tree)")
    status.add_argument("-v", "--verbose", action="store_true")
    deploy = sub.add_parser("deploy", help="push the kernel (KUP) and changed tapes")
    deploy.add_argument("hosts", nargs="*", help="host names or MACs (default: all)")
    deploy.add_argument("--from", dest="source", help="release folder (default: the build tree)")
    deploy.add_argument("--kernel-only", action="store_true")
    deploy.add_argument("--tapes-only", action="store_true")
    deploy.add_argument("--force-kernel", action="store_true", help="push the kernel even if the size matches")
    deploy.add_argument("--add-missing", action="store_true", help="also install tapes the host does not have")
    deploy.add_argument("--dry-run", action="store_true", help="show the plan and stop")
    deploy.add_argument("-y", "--yes", action="store_true", help="do not ask before acting")
    for name, text in (("run", "run one shell command on a host"), ("put", "copy a file to a host's boot volume"),
                       ("shell", "open an interactive shell (term net)"), ("bootcode", "show or update the boot code")):
        sp = sub.add_parser(name, help=text)
        sp.add_argument("host", help="host name or MAC")
        if name == "run":
            sp.add_argument("words", nargs="+", help="the command")
        if name == "put":
            sp.add_argument("file")
            sp.add_argument("name", nargs="?", help="name on the host (default: the file's name)")
            sp.add_argument("--disk", type=int, default=1)
        if name == "bootcode":
            sp.add_argument("--update", action="store_true", help="rewrite it (asks you to type the host name)")
    args = parser.parse_args()
    ensure_privilege()
    CONFIG.update(load_config())
    iface = args.iface or CONFIG.get("iface") or send_tape.default_interface()
    return {"hosts": cmd_hosts, "status": cmd_status, "deploy": cmd_deploy, "run": cmd_run,
            "put": cmd_put, "shell": cmd_shell, "bootcode": cmd_bootcode}[args.command](args, iface)


if __name__ == "__main__":
    sys.exit(main())
