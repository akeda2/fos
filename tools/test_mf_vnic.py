#!/usr/bin/env python3
"""Virtual NICs on the server kernel (`make test-mf-vnic`): two virtual interfaces on one e1000, one from a `vnic` line in
`settings` and one from the `vnic add` command, each running its own `httpd`. QEMU's user-mode network gives each MAC its own
DHCP lease (10.0.2.15 and .16) and forwards a host port to each address, so a reply on both proves the card's frames are split
by destination MAC (DHCP and ARP broadcasts reach every interface, unicast only its owner). Also: `vnic del` refuses while a
tape runs on the interface, works after `kill`, and the number is reused."""
import http.client
import os
import re
import select
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from testslot import port  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent
IMAGE = Path(os.environ.get("FOSFOR_IMAGE_MF_VNIC", ROOT / "build" / "fosfor-mf" / "fosfor-vnic.img"))
CODE = next(p for p in ["/usr/share/OVMF/OVMF_CODE_4M.fd", "/usr/share/OVMF/OVMF_CODE.fd",
                        "/usr/share/edk2/ovmf/OVMF_CODE.fd"] if Path(p).exists())
PORT_A = port(28091)
PORT_B = port(28092)
PORT_C = port(28093)
failures = []


def check(ok, text, detail=""):
    print(("PASS: " if ok else "FAIL: ") + text)
    if not ok:
        failures.append(f"{text}: {detail}")


def get(host_port):
    connection = http.client.HTTPConnection("127.0.0.1", host_port, timeout=30)
    try:
        connection.request("GET", "/")
        response = connection.getresponse()
        response.read()
        return response.status
    except OSError as error:
        return str(error)
    finally:
        connection.close()


def main():
    qemu = subprocess.Popen(
        ["qemu-system-x86_64", "-cpu", "max", "-machine", "q35", "-m", "1024", "-smp", "2", "-display", "none",
         "-nic", f"user,model=e1000,hostfwd=tcp:127.0.0.1:{PORT_A}-10.0.2.15:80,hostfwd=tcp:127.0.0.1:{PORT_B}-10.0.2.16:80,hostfwd=tcp:127.0.0.1:{PORT_C}-10.0.2.17:80",
         "-serial", "stdio", "-monitor", "none", "-no-reboot",
         "-drive", f"if=pflash,format=raw,readonly=on,file={CODE}", "-drive", f"format=raw,file={IMAGE},snapshot=on"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    log = b""

    def wait_for(text, timeout, start=0):
        nonlocal log
        end = time.time() + timeout
        while time.time() < end and text.encode() not in log[start:]:
            if select.select([qemu.stdout], [], [], 0.2)[0]:
                data = os.read(qemu.stdout.fileno(), 4096)
                if not data:
                    break
                log += data
        return text.encode() in log[start:]

    def command(text, expect, timeout=30):
        mark = len(log)
        qemu.stdin.write(text.encode() + b"\n")
        qemu.stdin.flush()
        found = wait_for(expect, timeout, mark) if expect else True
        wait_for("fos> ", timeout, mark)
        return found, log[mark:].decode("utf8", "replace")

    try:
        if not wait_for("fos> ", 150):
            failures.append("no shell prompt:\n" + log[-800:].decode("utf8", "replace"))
            return finish()
        check(b"vnic web: nic2 on nic1" in log, "the `vnic` line of settings made nic2 at boot", log[-600:].decode("utf8", "replace"))
        ok, out = command("vnic add files on 1", "Use it with")
        check(ok and "nic3 on nic1" in out, "`vnic add` makes nic3", out)
        ok, out = command("vnic add files on 1", "that name is taken")
        check(ok, "a second `files` is refused", out)
        ok, out = command("vnic add bad on 2", "must be a physical")
        check(ok, "a virtual interface cannot sit on a virtual one", out)
        ok, out = command("vnic add 7 on 1", "not a number")
        check(ok, "a numeric name is refused", out)
        ok, out = command("vnic", "files")
        check(ok and "web: nic2 on nic1" in out and "files: nic3 on nic1" in out, "`vnic` lists both", out)

        for name in ("web", "files"):
            mark = len(log)
            ok, out = command(f"run httpd @httpd.cfg &{name}", "process", 30)
            check(ok, f"httpd starts on &{name}", out)
            if not wait_for("worker threads", 40, mark):
                failures.append(f"httpd on {name} did not start:\n" + log[mark:].decode("utf8", "replace")[-800:])
                return finish()
        time.sleep(2)
        check(get(PORT_A) == 200, "the first lease (10.0.2.15) answers", str(get(PORT_A)))
        check(get(PORT_B) == 200, "the second lease (10.0.2.16) answers", str(get(PORT_B)))
        ok, out = command("nic", "frames", 10)
        counts = [int(n) for n in re.findall(r"vnic \w+ on nic1.*\n\s+frames: (\d+) received", out)]
        check(len(counts) == 2 and all(n > 0 for n in counts), "both virtual interfaces counted received frames", out)

        ok, out = command("vnic del files", "kill", 10)
        check(ok, "`vnic del` refuses while httpd runs on it", out)
        ok, out = command("ps", "httpd", 10)
        pids = re.findall(r"^(\d+)\s+\d+\s+httpd", out, re.M)
        check(len(pids) == 2, "two httpd processes", out)
        if len(pids) == 2:
            command(f"kill {pids[-1]}", "", 3)
            time.sleep(1)
            ok, out = command("vnic del files", "removed", 10)
            check(ok, "`vnic del` works after the tape ended", out)
            ok, out = command("vnic add again on 1", "nic3 on nic1", 10)
            check(ok, "the freed number is reused", out)

        # &+N: an interface for one tape, named after it, gone when it ends
        mark = len(log)
        ok, out = command("run httpd @httpd.cfg &+1", "process", 30)
        check(ok and "vnic httpd: nic4 on nic1" in out, "`&+1` makes nic4 named httpd for the tape", out)
        if not wait_for("worker threads", 40, mark):
            failures.append("httpd on &+1 did not start:\n" + log[mark:].decode("utf8", "replace")[-800:])
            return finish()
        time.sleep(2)
        check(get(PORT_C) == 200, "the tape on its own interface answers (10.0.2.17)", str(get(PORT_C)))
        ok, out = command("vnic del httpd", "kill", 10)
        check(ok, "its interface cannot be deleted while it runs", out)
        ok, out = command("ps", "httpd", 10)
        pids = re.findall(r"^(\d+)\s+\d+\s+httpd", out, re.M)
        check(len(pids) == 2, "web's httpd and the new one are running", out)
        if len(pids) == 2:
            command(f"kill {pids[-1]}", "", 3)
            time.sleep(1)
            ok, out = command("vnic", "web", 10)
            check(ok and "httpd:" not in out and "web: nic2" in out, "the interface went with the tape, web stays", out)
            mark = len(log)
            ok, out = command("run httpd @httpd.cfg &+1", "process", 30)
            check(ok and "vnic httpd: nic4 on nic1" in out, "the number and name are reused", out)
        ok, out = command("run nosuchtape &+1", "no file", 10)
        ok2, listing = command("vnic", "web", 10)
        check(ok and "nosuchtape" not in listing and listing.count("nic1 MAC") == 3, "a tape that cannot start leaves no interface behind", out + listing)
        ok, out = command("run httpd &+9", "physical NIC", 10)
        check(ok, "&+N on a missing NIC is refused", out)
        check(b"CPU exception" not in log, "the kernel did not fault", log[-1200:].decode("utf8", "replace"))
    finally:
        qemu.kill()
    return finish()


def finish():
    for failure in failures:
        print("FAIL: " + failure)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
