#!/usr/bin/env python3
"""Named services on the server kernel (`make test-mf-service`): two httpd services on two virtual interfaces, started by a
`start` file that holds `service start all`. Stopping, starting and restarting one leaves the other answering; a service
marked `restart` comes back by itself after its process is killed, one without stays down."""
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
IMAGE = Path(os.environ.get("FOSFOR_IMAGE_MF_SERVICE", ROOT / "build" / "fosfor-mf" / "fosfor-service.img"))
CODE = next(p for p in ["/usr/share/OVMF/OVMF_CODE_4M.fd", "/usr/share/OVMF/OVMF_CODE.fd",
                        "/usr/share/edk2/ovmf/OVMF_CODE.fd"] if Path(p).exists())
PORT_A = port(28101)
PORT_B = port(28102)
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
         "-nic", f"user,model=e1000,hostfwd=tcp:127.0.0.1:{PORT_A}-10.0.2.15:80,hostfwd=tcp:127.0.0.1:{PORT_B}-10.0.2.16:80",
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

    def pids():
        _, out = command("service", "COMMAND", 10)
        return {m.group(1): int(m.group(3)) for m in re.finditer(r"^(\w+)\s+(running|stopped|restart in \d+ s)\s+(\d+)", out, re.M)}, out

    def answers(port, timeout=40):
        end = time.time() + timeout
        while time.time() < end:
            if get(port) == 200:
                return True
            time.sleep(1)
        return False

    try:
        if not wait_for("fos> ", 150):
            failures.append("no shell prompt:\n" + log[-800:].decode("utf8", "replace"))
            return finish()
        wait_for("worker threads", 60)
        time.sleep(1)
        check(log.count(b"worker threads") >= 2 or wait_for("worker threads", 40, log.find(b"worker threads") + 1),
              "`service start all` from the start file started both", log[-800:].decode("utf8", "replace"))
        check(answers(PORT_A) and answers(PORT_B), "both services answer")
        table, out = pids()
        check(table.get("web", 0) > 0 and table.get("files", 0) > 0, "`service` lists both as running", out)
        ok, out = command("service start web", "already running", 10)
        check(ok, "starting a running service is refused politely", out)

        ok, out = command("help httpd", "httpd", 15)
        check(ok and "root=" in out, "`help TAPE` prints a tape's usage without running it", out)
        ok, out = command("httpd help", "root=", 15)
        check(ok and "runs in the background" not in out, "`TAPE help` prints the usage too", out)

        ok, out = command("service stop files", "stopped", 15)
        check(ok, "`service stop files`", out)
        time.sleep(1)
        check(get(PORT_B) != 200, "files no longer answers")
        check(get(PORT_A) == 200, "web is untouched")
        ok, out = command("service start files", "run httpd", 15)
        check(ok and answers(PORT_B), "`service start files` brings it back", out)

        before = pids()[0]
        ok, out = command("service restart web", "run httpd", 30)
        check(ok and answers(PORT_A), "`service restart web` restarts it", out)
        after = pids()[0]
        check(after["web"] != before["web"] and after["files"] == before["files"], "restarting web left files alone", f"{before} {after}")

        # crash: web is `restart`, files is not
        table = pids()[0]
        mark = len(log)
        command(f"kill {table['web']}", "", 3)
        check(wait_for("starting again in 2 s", 20, mark), "a killed `restart` service is scheduled again", log[mark:].decode("utf8", "replace"))
        check(answers(PORT_A, 60), "web is back by itself")
        table2 = pids()[0]
        check(table2["web"] not in (0, table["web"]), "web has a new process", str(table2))
        mark = len(log)
        command(f"kill {table2['files']}", "", 3)
        time.sleep(3)
        table3, out = pids()
        check(table3["files"] == 0 and "starting again" not in log[mark:].decode("utf8", "replace"), "files has no `restart` and stays down", out)
        check(get(PORT_A) == 200, "web answers still")

        ok, out = command("service stop all", "stopped", 20)
        table4, out = pids()
        check(all(pid == 0 for pid in table4.values()), "`service stop all` stops everything", out)
        ok, out = command("service start all", "run httpd", 20)
        check(answers(PORT_A) and answers(PORT_B), "`service start all` starts everything", out)
        # configuration lives in settings/: the root's httpd.cfg moves there and both services still find it
        command("service stop all", "stopped", 20)
        ok, out = command("settings migrate", "Moved", 15)
        check(ok and "httpd.cfg" in out, "`settings migrate` moves httpd.cfg into settings/", out)
        ok, out = command("ls settings", "httpd.cfg", 10)
        check(ok, "settings/ holds httpd.cfg", out)
        ok, out = command("ls", "bin", 10)
        check("httpd.cfg" not in out, "the root no longer holds it", out)
        command("service start all", "run httpd", 20)
        check(answers(PORT_A) and answers(PORT_B), "`@httpd.cfg` is found in settings/")
        ok, out = command("service stop all", "stopped", 20)
        ok, out = command("service stop nosuch", "no service", 10)
        check(ok, "an unknown name is refused", out)
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
