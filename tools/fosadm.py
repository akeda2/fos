#!/usr/bin/env python3
"""fosadm: find fos machines on the network and show what they run.

  fosadm.py hosts                  discover machines (forgets stale peers first)
  fosadm.py status [HOST...]       kernel slot and tapes against the build (SHA-256; --sizes: sizes only)
  fosadm.py deploy [HOST...]       push the kernel (KUP) and tapes that differ (by SHA-256), after showing a plan;
                                   hosts are handled side by side over Ethernet (--one-by-one: in turn), lines tagged [host]
  fosadm.py run|put|shell|bootcode HOST ...   one command, a file, a terminal, the boot code

With `--serial PORT` (e.g. /dev/ttyUSB0) there is one machine, on that console serial line, and no Ethernet is used:
status, deploy (kernel through `kup serial`, tapes through `recv`), run and put work as above. The shell must be at its
prompt; --baud is the console speed (115200), --fast raises it for the transfers (aarch64 only). shell opens a terminal on
the line (Ctrl-C or Ctrl-D leaves); bootcode needs Ethernet. tools/send_kernel_serial.py and tools/send_file_serial.py are the single-purpose senders.

Machines announce `hostname` + NIC number (skylake3 is NIC 3 of skylake), so the
NICs of one machine are grouped under one host name. Tapes are compared by SHA-256 (--sizes: by size)
against --from DIR (default: the build tree: build/fosfor/tapes and the release
kernel). Stored tapes are byte-identical to the build, so each tape is compared by SHA-256 (status and deploy: by default, --sizes opts out), which also catches a rebuild that kept its
size; a deploy always verifies what it copied that way. The machine hashes its own
files (its `sha256` command), so no tape crosses the link; a kernel without that command
means a NETFS fetch over Ethernet, sizes over --serial. tools/verify_tapes.py is the Ethernet check alone.

Raw Ethernet needs CAP_NET_RAW; if it is missing the tool says how to grant it
to a dedicated Python copy (it never does so silently).
"""
import argparse
import concurrent.futures
import math
import os
import re
import shutil
import socket
import subprocess
import sys
import threading
from pathlib import Path

TOOLS = Path(__file__).resolve().parent
ROOT = TOOLS.parent
sys.path.insert(0, str(TOOLS))
import send_tape  # noqa: E402
import send_kernel_serial as kup  # noqa: E402
import foscopy  # noqa: E402
import tape_manifest  # noqa: E402
import verify_tapes  # noqa: E402

BUILD_KERNEL = ROOT / "build/cargo-target/x86_64-fos-kernel/release/fosfor-kernel"
BUILD_KERNEL_MF = ROOT / "build/cargo-target/x86_64-fos-kernel/release/fosfor-kernel-mf"
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


class Serial:
    """A machine on its console serial line: the shell at its prompt, `kup serial` and `recv` for the transfers."""

    PROMPT = b"fos>"

    def __init__(self, port: str, baud: int, fast: int):
        import termios
        self.termios, self.baud, self.fast = termios, baud, fast
        self.fd = os.open(port, os.O_RDWR | os.O_NOCTTY)
        self.attributes = termios.tcgetattr(self.fd)
        self.attributes[0] = self.attributes[1] = self.attributes[3] = 0
        self.attributes[2] = termios.CS8 | termios.CREAD | termios.CLOCAL
        self.set_speed(baud)

    def set_speed(self, baud: int) -> None:
        self.attributes[4] = self.attributes[5] = getattr(self.termios, f"B{baud}")
        self.termios.tcsetattr(self.fd, self.termios.TCSADRAIN, self.attributes)

    def read(self, timeout: float) -> bytes:
        import select
        ready, _, _ = select.select([self.fd], [], [], timeout)
        return os.read(self.fd, 4096) if ready else b""

    def write(self, data: bytes) -> None:
        while data:
            data = data[os.write(self.fd, data):]

    def ask(self, commands: list[str], timeout: float = 20) -> str:
        """Types each command and collects what the shell prints up to its next prompt."""
        import time
        text = b""
        for command in commands:
            self.write(b"\r")
            self.until_prompt(min(timeout, 10))   # boot messages, a running command: wait for a prompt first
            while self.read(1.0):                 # and for the prompts still on their way (the Enter's own, a finished transfer's)
                pass
            self.write(command.encode() + b"\r")
            text += self.until_prompt(timeout)
        return text.decode("utf-8", "replace").replace("\r", "")

    def until_prompt(self, timeout: float) -> bytes:
        import time
        answer = b""
        end = time.monotonic() + timeout
        while time.monotonic() < end and not answer.rstrip(b" ").endswith(self.PROMPT):
            answer += self.read(0.2)
        return answer

    def interactive(self) -> int:
        """A terminal on the line: keys go out as typed (Escape aborts a running command), output is shown. Ctrl-C or Ctrl-D leaves,
        like `term_net.py --interactive`; at the end of piped input it waits for the last answer, then leaves."""
        import select
        import time
        import tty
        stdin = sys.stdin.fileno()
        saved = None
        if os.isatty(stdin):
            saved = self.termios.tcgetattr(stdin)
            tty.setcbreak(stdin)
        sys.stderr.write("[fosadm: serial shell, Ctrl-C or Ctrl-D leaves]\r\n")
        self.write(b"\r")
        closed_at = None
        try:
            while closed_at is None or time.monotonic() < closed_at:
                watched = [self.fd] + ([stdin] if closed_at is None else [])
                ready, _, _ = select.select(watched, [], [], 0.2)
                if self.fd in ready:
                    data = os.read(self.fd, 4096)
                    os.write(sys.stdout.fileno(), data)
                if stdin in ready:
                    data = os.read(stdin, 1024)
                    if not data:
                        closed_at = time.monotonic() + 2.0
                    elif b"\x03" in data or b"\x04" in data:
                        break
                    else:
                        self.write(data.replace(b"\n", b"\r") if not os.isatty(stdin) else data)
        except KeyboardInterrupt:
            pass
        finally:
            if saved is not None:
                self.termios.tcsetattr(stdin, self.termios.TCSADRAIN, saved)
        return 0

    def send(self, shell_command: bytes, data: bytes) -> int:
        try:
            return kup.push(self.read, self.write, data, log=lambda message: None, baud=self.fast,
                            set_speed=self.set_speed, shell_command=shell_command)
        finally:
            self.set_speed(self.baud)


class Host:
    def __init__(self, name: str):
        self.name = name
        self.nics: dict[int, bytes] = {}
        self.serial: Serial | None = None
        # Tapes that live only in bin/ (FOSFS v4); a bare name that is in the root is the root's, as `run` takes it.
        self.in_bin: set[str] = set()
        self.has_bin = False      # a FOSFS v4 volume with a bin/ folder: new tapes belong there

    def placed(self, name: str) -> str:
        return "bin/" + name if name in self.in_bin else name

    @property
    def mac(self) -> bytes:
        return self.nics[min(self.nics)]

    def label(self) -> str:
        if self.serial:
            return "serial"
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
    if not wanted or (hosts and hosts[0].serial):
        return hosts
    chosen = []
    for text in wanted:
        match = [h for h in hosts if h.name == text.lower()
                 or any(send_tape.format_mac(m) == text.lower() for m in h.nics.values())]
        if not match:
            raise SystemExit(f"no discovered host matches {text!r}; known: {', '.join(h.name for h in hosts) or 'none'}")
        chosen += match
    return chosen


def find_hosts(args, iface: str) -> list[Host]:
    if args.serial:
        host = Host("serial")
        host.serial = Serial(args.serial, args.baud, args.fast)
        return [host]
    return discover(iface, args.keep, args.timeout)


def ask_host(iface: str, host: Host, commands: list[str], timeout: float = 60) -> str:
    if host.serial:
        return host.serial.ask(commands, min(timeout, 60))
    result = subprocess.run(
        [sys.executable, str(TOOLS / "term_net.py"), "--iface", iface, "--mac", send_tape.format_mac(host.mac),
         "--command-timeout", "10", *commands], capture_output=True, text=True, timeout=timeout)
    return result.stdout


def listing(host: Host, text: str) -> dict[str, int]:
    """Name -> size from the answers to `ls` and `ls bin`. A name in both is the root's (`run` takes the root's first);
    `host.in_bin` is the names that only bin/ has."""
    marker = re.search(r"^.*\bls bin\s*$", text, re.M)
    root = parse_ls(text[:marker.start()] if marker else text)
    folder = parse_ls(text[marker.end():]) if marker else {}
    host.in_bin = {name for name in folder if name not in root}
    host.has_bin = re.search(r"^bin\s+<dir>", text, re.M) is not None
    return {**folder, **root}


def parse_ls(text: str) -> dict[str, int]:
    return {m.group(1): int(m.group(2)) for m in re.finditer(r"^([a-z0-9._-]+)\s+(\d+) bytes", text, re.M)}


def source_files(source: Path | None, arch: str = "x86_64") -> tuple[Path | None, Path, dict[str, int]]:
    """Kernel, tape folder and tape sizes for one CPU. A release folder holds one sub-folder per CPU
    (`x86_64/`, `aarch64/`, each with `kernel` and `tapes/`); a flat folder is taken as x86_64."""
    mf = arch == "x86_64-mf"
    arch = "x86_64" if mf else arch
    if source is None:
        kernel, tapes = (BUILD_KERNEL_A64, BUILD_TAPES_A64) if arch == "aarch64" else (BUILD_KERNEL, BUILD_TAPES)
        kernel = BUILD_KERNEL_MF if mf else kernel
    else:
        base = source / arch if (source / arch).is_dir() else (source if arch == "x86_64" else source / "-none-")
        kernel, tapes = base / ("kernel-mf" if mf else "kernel"), base / "tapes"
        if not kernel.exists():
            kernel = base / ("fosfor-kernel-mf" if mf else "fosfor-kernel")
        if not tapes.is_dir():
            tapes = base
    sizes = {p.name: p.stat().st_size for p in tapes.iterdir() if p.is_file() and not p.name.startswith(".")} \
        if tapes.is_dir() else {}
    return (kernel if kernel.exists() else None), tapes, sizes


def kernel_sectors(kernel: Path | None) -> int | None:
    return math.ceil(kernel.stat().st_size / 512) if kernel else None


def arch_of(text: str) -> str:
    """`aarch64`, `x86_64`, or `x86_64-mf` for the server kernel (its `version` says "server kernel (mf)"):
    it has its own kernel image, so the embedded one must never be pushed to it."""
    if "server kernel (mf)" in text:
        return "x86_64-mf"
    return "aarch64" if "aarch64" in text else "x86_64"


def cmd_hosts(args, iface: str) -> int:
    hosts = find_hosts(args, iface)
    if not hosts:
        print("no fos machines answered.")
        return 1
    for host in hosts:
        note = host_setting(host, "note")
        print(f"{host.name:<14} NIC {host.label()}" + (f"  # {note}" if note else ""))
    return 0


def cmd_status(args, iface: str) -> int:
    hosts = pick(find_hosts(args, iface), args.hosts)
    for host in hosts:
        out = ask_host(iface, host, ["debug slots", "ls", "ls bin", "about", "version"])
        print(f"{host.name} ({host.label()})")
        arch = arch_of(out)
        kernel, tape_dir, tapes = source_files(Path(args.source) if args.source else None, arch)
        want_sectors = kernel_sectors(kernel)
        booted = re.search(r"booted slot: (\d+)", out)
        sizes = re.search(r"slot sizes \(sectors\): (\d+) / (\d+)", out)
        files = listing(host, out)
        if not booted or not sizes:
            print("  no usable answer (is a tape running? try again or stop it first)")
            continue
        have = int(sizes.group(1 + int(booted.group(1))))
        state = "unknown" if want_sectors is None else ("current" if have == want_sectors else f"differs (build {want_sectors})")
        print(f"  {arch} kernel: slot {booted.group(1)}, {have} sectors, {state}")
        old = [n for n, s in tapes.items() if n in files and files[n] != s]
        missing = [n for n in tapes if n not in files]
        states = content_states(iface, host, tape_dir, sorted(n for n in tapes if n in files)) if args.content else {}
        if states and set(states.values()) == {"unsupported"}:
            print("  this kernel has no `sha256` command yet (deploy it first); falling back to sizes")
            states = {}
        if states:
            old = sorted(n for n, state in states.items() if state == "differs")
            unread = sorted(n for n, state in states.items() if state == "missing")
            if unread:
                print(f"  could not read (no NETFS answer?): {' '.join(unread)}")
            print(f"  tapes (by content): {sum(1 for state in states.values() if state == 'same')} identical, {len(old)} differ, {len(missing)} missing")
        else:
            print(f"  tapes: {sum(1 for n in tapes if n in files) - len(old)} current, {len(old)} differ, {len(missing)} missing")
        if old:
            print(f"    differ: {' '.join(sorted(old))}")
        if args.verbose and missing:
            print(f"    missing: {' '.join(sorted(missing))}")
    return 0


def parse_sha256(out: str) -> dict[str, str]:
    """name -> hex digest from the lines of the shell's `sha256` command ("DIGEST  LENGTH  NAME")."""
    return {m.group(3): m.group(1) for m in re.finditer(r"^([0-9a-f]{64})  (\d+)  (\S+)\s*$", out, re.M)}


def manifest_hashes(iface: str, host: Host) -> dict[str, str] | None:
    """name -> hex SHA-256 of the machine's stamped tapes from its signed tapesync manifest, or None (no `synckey` file
    here, serial link, the machine has no key or another one, or it did not answer)."""
    key = tape_manifest.read_key()
    if key is None or host.serial:
        return None
    link = foscopy.RawTransport(iface)
    try:
        return {name: entry["sha256"] for name, entry in tape_manifest.fetch(link, host.mac, key).items()}
    except (tape_manifest.ManifestError, foscopy.Fatal):
        return None
    finally:
        link.sock.close()


def content_states(iface: str, host: Host, tape_dir: Path, names: list[str]) -> dict[str, str]:
    """name -> "same" | "differs" | "missing" | "unsupported", comparing SHA-256 with the build. Three ways, best first:
    the machine's signed tapesync manifest (needs `synckey` in the repository root; one small answer, authenticated);
    its `sha256` shell command (it hashes its own files; either link); a NETFS fetch of each tape (Ethernet only).
    Nothing but hashes crosses the link in the first two. "unsupported": serial and a kernel without `sha256`."""
    import hashlib
    import tempfile

    def built(name: str) -> str:
        return hashlib.sha256((tape_dir / name).read_bytes()).hexdigest()

    states: dict[str, str] = {}
    manifest = manifest_hashes(iface, host)
    if manifest is not None:
        for name in names:
            if name in manifest:
                states[name] = "same" if manifest[name] == built(name) else "differs"
    rest = [name for name in names if name not in states]   # not stamped tapes there (or no manifest): ask the shell
    for start in range(0, len(rest), 6):
        batch = rest[start:start + 6]
        out = ask_host(iface, host, ["sha256 " + " ".join(host.placed(name) for name in batch)], 60)
        found = {name.removeprefix("bin/") if name.removeprefix("bin/") in host.in_bin else name: digest for name, digest in parse_sha256(out).items()}
        if not found and "not found" not in out:
            break
        for name in batch:
            states[name] = "missing" if name not in found else ("same" if found[name] == built(name) else "differs")
    else:
        return states
    if host.serial:
        return {name: "unsupported" for name in names}
    mac = send_tape.format_mac(host.mac)
    with tempfile.TemporaryDirectory() as scratch:
        for name in rest:
            if name not in states:
                states[name] = verify_tapes.compare(iface, mac, name, str(tape_dir / name), scratch)[0]
    return states


def stop_running_tape(iface: str, host: Host) -> None:
    """A lone ESC through term net ends a foreground tape (ptp from a start file, say); harmless at an idle prompt."""
    if host.serial:
        host.serial.write(b"\x1b")
        return
    send_tape_cmd = (f"(printf '\\x1b'; sleep 2) | {sys.executable} {TOOLS / 'term_net.py'} --iface {iface} "
                     f"--mac {send_tape.format_mac(host.mac)} --interactive")
    subprocess.run(["bash", "-c", send_tape_cmd], capture_output=True, timeout=30)


def probe(iface: str, host: Host) -> tuple[str, int, dict[str, int], bool] | None:
    """`(cpu, booted slot's sectors, files, stopped_a_tape)`; sends ESC once if the shell does not answer."""
    stopped = False
    for attempt in range(2):
        out = ask_host(iface, host, ["debug slots", "ls", "ls bin", "about", "version"])
        booted = re.search(r"booted slot: (\d+)", out)
        sizes = re.search(r"slot sizes \(sectors\): (\d+) / (\d+)", out)
        if booted and sizes:
            return arch_of(out), int(sizes.group(1 + int(booted.group(1)))), listing(host, out), stopped
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


PRINT_LOCK = threading.Lock()


def say(host: Host, text: str, parallel: bool) -> None:
    """One line of output; when hosts run side by side each line carries the host name so the streams can be told apart."""
    with PRINT_LOCK:
        print(f"[{host.name}] {text.lstrip()}" if parallel else text, flush=True)


def run_hosts(function, items: list, parallel: bool) -> list:
    """`function(item)` for every item, side by side (one thread each; the work is waiting for the machines and for
    subprocesses) or one after the other; results in the order of `items`. Serial links are always one at a time."""
    if not parallel or len(items) < 2:
        return [function(item) for item in items]
    with concurrent.futures.ThreadPoolExecutor(max_workers=len(items)) as pool:
        return list(pool.map(function, items))


def plan_host(iface: str, args, host: Host, parallel: bool):
    """The deploy plan for one host: `(host, new_kernel, tapes, stopped, arch)`, or None when it is skipped."""
    do_tapes = not args.kernel_only
    if host_setting(host, "skip", False):
        say(host, f"{host.name}: skipped (skip = true in hosts.toml)", parallel)
        return None
    state = probe(iface, host)
    if state is None:
        say(host, f"{host.name}: no answer, skipped", parallel)
        return None
    arch, have, files, stopped = state
    kernel, tape_dir, tapes = source_files(Path(args.source) if args.source else None, arch)
    do_kernel = kernel is not None and not args.tapes_only
    want = kernel_sectors(kernel)
    new_kernel = do_kernel and (args.force_kernel or have != want)
    states = content_states(iface, host, tape_dir, sorted(n for n in tapes if n in files)) if do_tapes and args.content else {}
    if states and set(states.values()) == {"unsupported"}:
        say(host, f"{host.name}: this kernel has no `sha256` command yet; planning by size", parallel)
        states = {}
    if states:
        changed = sorted(n for n, state in states.items() if state != "same")
    else:
        changed = sorted(n for n, s in tapes.items() if do_tapes and n in files and files[n] != s)
    tape_list = changed + (sorted(n for n in tapes if do_tapes and n not in files) if args.add_missing else [])
    return host, new_kernel, tape_list, stopped, arch


def deploy_host(iface: str, args, plan, parallel: bool) -> int:
    """Carries out one host's plan; returns the number of failures."""
    host, new_kernel, tape_list, stopped, arch = plan
    failed = 0

    def print(text: str) -> None:
        say(host, text, parallel)

    mac = "" if host.serial else send_tape.format_mac(host.mac)
    kernel, tape_dir, tapes = source_files(Path(args.source) if args.source else None, arch)
    want = kernel_sectors(kernel)
    print(f"== {host.name}")
    if new_kernel:
        if host.serial:
            try:
                host.serial.send(b"kup serial", kernel.read_bytes())
            except SystemExit as refusal:
                print(f"  kernel: not accepted ({refusal}); skipped")
                return failed + 1
        else:
            result = subprocess.run([sys.executable, str(TOOLS / "send_kernel_update.py"), "--iface", iface, "--mac", mac,
                                     str(kernel)], capture_output=True, text=True)
            if "ACK" not in result.stdout:
                print(f"  kernel: not accepted ({(result.stdout + result.stderr).strip().splitlines()[-1:]}); skipped")
                return failed + 1
        print("  kernel: accepted, waiting for the reboot...")
        after = wait_up(iface, host)
        if after is None:
            print("  kernel: the host did not come back in time; check it by hand")
            return failed + 1
        if after[1] != want:
            print(f"  kernel: FELL BACK to the old kernel (slot {after[0]}, {after[1]} sectors); tapes not touched")
            return failed + 1
        print(f"  kernel: running (slot {after[0]}, {after[1]} sectors)")
    if tape_list:
        state = probe(iface, host)
        stopped = stopped or (state is not None and state[3])
        if state is None:
            print("  tapes: host does not answer; skipped")
            return failed + 1
        new_names = {name for name in tape_list if name not in state[2]} if host.has_bin else set()
        for name in tape_list:
            if host.serial:
                if name in new_names:
                    host.in_bin.add(name)
                try:
                    host.serial.send(f"recv disk1/{host.placed(name)}".encode(), (tape_dir / name).read_bytes())
                    ok = True
                except SystemExit as refusal:
                    print(f"  tape {name}: {refusal}")
                    ok = False
            else:
                ok = subprocess.run([sys.executable, str(TOOLS / "foscopy.py"), "--iface", iface, "--mac", mac,
                                     str(tape_dir / name), f":disk1/{name}"], capture_output=True, text=True).returncode == 0
            if ok and name in new_names and not host.serial:
                # NETFS puts a bare name in the root unless the tape already lives in bin/; a new tape is moved there.
                moved = ask_host(iface, host, [f"mv {name} bin/{name}"])
                ok = "Moved " in moved
                if ok:
                    host.in_bin.add(name)
                else:
                    print(f"  tape {name}: copied to the root, but `mv` to bin/ failed: {moved.strip()[-120:]}")
            print(f"  tape {name}: {'ok' if ok else 'FAILED'}")
            failed += not ok
        states = content_states(iface, host, tape_dir, tape_list)
        if set(states.values()) == {"unsupported"}:
            files = listing(host, ask_host(iface, host, ["ls", "ls bin"]))
            bad = [n for n in tape_list if files.get(n) != tapes[n]]
            print("  verified by size (no `sha256` command on this kernel)" if not bad else f"  size mismatch after copy: {' '.join(bad)}")
        else:
            bad = [n for n, state in states.items() if state != "same"]
            print("  verified by content (SHA-256)" if not bad else f"  not identical to the build after copy: {' '.join(bad)}")
        failed += bool(bad)
    restart = host_setting(host, "restart")
    if stopped and restart and host.serial:
        host.serial.ask([restart])
        print(f"  a running tape was stopped; ran `{restart}` to bring it back (hosts.toml).")
    elif stopped and restart:
        subprocess.run([sys.executable, str(TOOLS / "term_net.py"), "--iface", iface, "--mac", mac,
                        "--command-timeout", "8", restart], capture_output=True, timeout=60)
        print(f"  a running tape was stopped; ran `{restart}` to bring it back (hosts.toml).")
    elif stopped:
        print("  a running tape was stopped; run `start` on the host (or reboot it) to bring it back.")
    return failed


def cmd_deploy(args, iface: str) -> int:
    hosts = pick(find_hosts(args, iface), args.hosts)
    parallel = not args.serial and not args.one_by_one
    plans = [p for p in run_hosts(lambda host: plan_host(iface, args, host, parallel), hosts, parallel) if p]
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
    failed = sum(run_hosts(lambda plan: deploy_host(iface, args, plan, parallel), todo, parallel))
    return 1 if failed else 0


def one_host(iface: str, args) -> Host:
    found = pick(find_hosts(args, iface), [args.host])
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
    if host.serial:
        try:
            host.serial.send(f"recv disk{args.disk}/{name}".encode(), Path(args.file).read_bytes())
        except SystemExit as refusal:
            print(f"put: {refusal}")
            return 1
        return 0
    result = subprocess.run([sys.executable, str(TOOLS / "foscopy.py"), "--iface", iface, "--mac",
                             send_tape.format_mac(host.mac), args.file, f":disk{args.disk}/{name}"])
    return result.returncode


def cmd_shell(args, iface: str) -> int:
    if args.serial:
        return Serial(args.serial, args.baud, args.fast).interactive()
    host = one_host(iface, args)
    return subprocess.run([sys.executable, str(TOOLS / "term_net.py"), "--iface", iface, "--mac",
                           send_tape.format_mac(host.mac), "--interactive"]).returncode


def cmd_bootcode(args, iface: str) -> int:
    if args.serial:
        raise SystemExit("bootcode needs Ethernet (its update must be confirmed in the same session).")
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
    parser.add_argument("--serial", metavar="PORT", help="use the console serial line of one machine instead of Ethernet")
    parser.add_argument("--baud", type=int, default=115200, help="console speed for --serial (default 115200)")
    parser.add_argument("--fast", type=int, default=0, help="--serial: raise the speed for transfers (aarch64 shell only)")
    parser.add_argument("--keep", "--no-discover-forget", dest="keep", action="store_true",
                        help="keep the remembered peers instead of forgetting them before discovery")
    parser.add_argument("--timeout", type=float, default=2.5, help="seconds to listen for announcements")
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("hosts", help="list the machines found on the network")
    status = sub.add_parser("status", help="show kernel and tape state against the build")
    status.add_argument("hosts", nargs="*", help="host names or MACs (default: all)")
    status.add_argument("--from", dest="source", help="release folder (default: the build tree)")
    status.add_argument("--sizes", dest="content", action="store_false", help="compare sizes, not SHA-256 (quicker, misses same-size changes)")
    status.add_argument("-v", "--verbose", action="store_true")
    deploy = sub.add_parser("deploy", help="push the kernel (KUP) and changed tapes")
    deploy.add_argument("hosts", nargs="*", help="host names or MACs (default: all)")
    deploy.add_argument("--from", dest="source", help="release folder (default: the build tree)")
    deploy.add_argument("--sizes", dest="content", action="store_false", help="plan by comparing sizes, not SHA-256 (quicker, misses same-size changes)")
    deploy.add_argument("--kernel-only", action="store_true")
    deploy.add_argument("--tapes-only", action="store_true")
    deploy.add_argument("--force-kernel", action="store_true", help="push the kernel even if the size matches")
    deploy.add_argument("--add-missing", action="store_true", help="also install tapes the host does not have")
    deploy.add_argument("--one-by-one", action="store_true", help="handle the hosts one after the other (default: all at once over Ethernet)")
    deploy.add_argument("--dry-run", action="store_true", help="show the plan and stop")
    deploy.add_argument("-y", "--yes", action="store_true", help="do not ask before acting")
    for name, text in (("run", "run one shell command on a host"), ("put", "copy a file to a host's boot volume"),
                       ("shell", "open an interactive shell (term net)"), ("bootcode", "show or update the boot code")):
        sp = sub.add_parser(name, help=text)
        sp.add_argument("host", **({"nargs": "?", "default": ""} if name == "shell" else {}), help="host name or MAC (shell: not needed with --serial)")
        if name == "run":
            sp.add_argument("words", nargs="+", help="the command")
        if name == "put":
            sp.add_argument("file")
            sp.add_argument("name", nargs="?", help="name on the host (default: the file's name)")
            sp.add_argument("--disk", type=int, default=1)
        if name == "bootcode":
            sp.add_argument("--update", action="store_true", help="rewrite it (asks you to type the host name)")
    args = parser.parse_args()
    if args.command == "shell" and not args.serial and not args.host:
        parser.error("the following arguments are required: host")
    if not args.serial:
        ensure_privilege()
    CONFIG.update(load_config())
    iface = "" if args.serial else (args.iface or CONFIG.get("iface") or send_tape.default_interface())
    return {"hosts": cmd_hosts, "status": cmd_status, "deploy": cmd_deploy, "run": cmd_run,
            "put": cmd_put, "shell": cmd_shell, "bootcode": cmd_bootcode}[args.command](args, iface)


if __name__ == "__main__":
    sys.exit(main())
