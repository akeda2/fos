#!/usr/bin/env python3
"""End-to-end test for `tools/foscopy.py` and NETFS serving from `term net`.

One QEMU guest (hostname `alpha`, so its NIC answers as `alpha1`) shares a
private multicast segment with this script. `foscopy.py` is run as a real
subprocess (its `--mcast` transport, so no raw-socket privilege is needed) in
two phases:

  1. While the guest sits in the automatic boot-time `term net` wait -- where
     a real machine spends its life -- PUT a multi-chunk binary file (with
     trailing zero bytes and Escape bytes) by hostname, GET it back and
     compare; GET a missing file; address the machine by MAC; and check the
     purely local refusals (bad name, empty file, two remote operands).
  2. After a local F12 to the idle prompt, `cat` confirms the PUT really
     landed on disk, then PUT/GET once more (the idle-prompt service point).

Not part of `make test`; run it by hand or through `tools/run_tests.py`.
"""

import os
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import test_debug_mode_e2e as dbg  # noqa: E402

ROOT = dbg.ROOT
FOSCOPY = ROOT / "tools" / "foscopy.py"
GROUP, PORT = "230.42.42.77", 4277
GUEST_MAC = "52:54:00:12:3c:01"
HOSTNAME_FILE = ROOT / "tools" / "hostname-test-alpha.txt"

# The NetGuest helper reads these module globals; give it a segment of its own
# so this test cannot hear (or be heard by) the debug-mode test's guest.
dbg.MCAST_GROUP, dbg.MCAST_PORT, dbg.GUEST_MAC = GROUP, PORT, bytes.fromhex(GUEST_MAC.replace(":", ""))


def foscopy(tmp: Path, *args: str, expect_ok: bool = True) -> subprocess.CompletedProcess:
    env = dict(os.environ, FOS_PEERS_FILE=str(tmp / "peers.json"))
    result = subprocess.run(
        [sys.executable, str(FOSCOPY), "--mcast", f"{GROUP}:{PORT}", "--timeout", "15", *args],
        capture_output=True, text=True, env=env, timeout=90,
    )
    if (result.returncode == 0) != expect_ok:
        raise SystemExit(f"foscopy {args} {'failed' if expect_ok else 'unexpectedly succeeded'}:\n"
                         f"{result.stdout}{result.stderr}")
    return result


def press_f12(guest) -> None:
    """Local F12 via the QEMU monitor: leaves `term net` for the prompt.
    (NetGuest.escape_to_idle_prompt would wait for the boot banner again.)"""
    start = len(guest.output)
    monitor = dbg.socketlib.socket(dbg.socketlib.AF_UNIX, dbg.socketlib.SOCK_STREAM)
    monitor.connect(str(guest.monitor_sock))
    monitor.settimeout(5)
    monitor.recv(65536)
    monitor.sendall(b"sendkey f12\n")
    monitor.close()
    guest.read_until(b"fos> ", 10, start)


def check(condition: bool, message: str, detail: str = "") -> None:
    if not condition:
        raise SystemExit(f"FAIL: {message}\n{detail}")
    print(f"PASS: {message}")


def main() -> None:
    for required in (dbg.STAGE1, dbg.STAGE2, dbg.KERNEL):
        if not required.exists():
            raise SystemExit(f"{required} does not exist; run `make image` first.")
    with tempfile.TemporaryDirectory(prefix="fos-foscopy-test-") as tmp_name:
        tmp = Path(tmp_name)
        image = tmp / "alpha.img"
        subprocess.run([sys.executable, str(dbg.BUILD_IMAGE), "--stage1", str(dbg.STAGE1), "--stage2", str(dbg.STAGE2),
                        "--kernel", str(dbg.KERNEL), "--output", str(image),
                        "--file", "hostname", str(HOSTNAME_FILE)], check=True)
        guest = dbg.NetGuest(image, tmp / "monitor.sock")
        try:
            guest.read_until(b"Waiting for a remote terminal client to connect", 30)
            time.sleep(1)

            blob = bytes(range(256)) * 11 + b"\x1b[A\x1b" + b"\x00" * 37  # 2,857 B: 3 chunks, zero tail
            (tmp / "blob.bin").write_bytes(blob)

            # --- Phase 1: the guest is inside `term net`, not at the prompt.
            result = foscopy(tmp, str(tmp / "blob.bin"), "alpha1:disk1/blob.bin")
            check("Copied" in result.stdout, "PUT by hostname while the machine sits in `term net`", result.stderr)
            foscopy(tmp, "alpha1:disk1/blob.bin", str(tmp / "back.bin"))
            check((tmp / "back.bin").read_bytes() == blob, "GET returns the PUT bytes exactly (binary, zero tail)")

            foscopy(tmp, f"{GUEST_MAC}:disk1/blob.bin", str(tmp / "by-mac.bin"))
            check((tmp / "by-mac.bin").read_bytes() == blob, "a full MAC works as HOST (`MAC:disk1/NAME`)")

            (tmp / "dest").mkdir()
            foscopy(tmp, "--mac", GUEST_MAC, ":disk1/blob.bin", str(tmp / "dest"))
            check((tmp / "dest" / "blob.bin").read_bytes() == blob, "a directory destination reuses the remote name")

            (tmp / "second.txt").write_text("hello from the host\n")
            foscopy(tmp, str(tmp / "second.txt"), f"{GUEST_MAC}/")
            foscopy(tmp, f"{GUEST_MAC}/second.txt", str(tmp / "second.back"))
            check((tmp / "second.back").read_text() == "hello from the host\n",
                  "a trailing slash on a PUT destination reuses the source's name")

            missing = foscopy(tmp, "alpha1:disk1/nothere", str(tmp / "x"), expect_ok=False)
            check("file not found" in missing.stderr, "GET of a missing file reports NOT_FOUND", missing.stderr)
            check(not (tmp / "x").exists(), "a failed GET leaves no local file behind")

            baddisk = foscopy(tmp, "alpha1:disk9/blob.bin", str(tmp / "x"), expect_ok=False)
            check("not available" in baddisk.stderr, "a missing disk number reports DISK_UNAVAILABLE", baddisk.stderr)

            unknown = foscopy(tmp, "--discover-timeout", "1", "nosuchhost1:disk1/a", str(tmp / "x"), expect_ok=False)
            check("no host named" in unknown.stderr, "an unknown hostname is reported with what was heard", unknown.stderr)

            (tmp / "Bad_Name.txt").write_text("x")
            check("legal FOSFS name" in foscopy(tmp, str(tmp / "Bad_Name.txt"), "alpha1:disk1/Bad_Name.txt",
                                                  expect_ok=False).stderr, "an illegal FOSFS name is refused locally")
            (tmp / "empty").write_bytes(b"")
            check("empty" in foscopy(tmp, str(tmp / "empty"), "alpha1:disk1/empty", expect_ok=False).stderr,
                  "an empty file is refused locally")
            check("exactly one" in foscopy(tmp, "a1:disk1/x", "a1:disk1/y", expect_ok=False).stderr,
                  "two remote operands are refused")

            # --- Phase 2: the idle prompt (also proves the refactor kept it working).
            press_f12(guest)
            out = guest.run_command("cat second.txt").decode("utf-8", "replace")
            check("hello from the host" in out, "the PUT file is really on the machine's disk", out)
            (tmp / "third.txt").write_text("served from the idle prompt\n")
            foscopy(tmp, str(tmp / "third.txt"), "alpha1:disk1/third.txt")
            out = guest.run_command("cat third.txt").decode("utf-8", "replace")
            check("served from the idle prompt" in out, "PUT works at the idle prompt")
            foscopy(tmp, "alpha1:disk1/blob.bin", str(tmp / "back2.bin"))
            check((tmp / "back2.bin").read_bytes() == blob, "GET works at the idle prompt")
            print("PASS: foscopy.py and NETFS serving from term net work end to end.")
        finally:
            guest.terminate()


if __name__ == "__main__":
    main()
