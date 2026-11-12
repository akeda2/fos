#!/usr/bin/env python3
"""Server kernel (mf) on aarch64, first slice: the aarch64 UEFI loader (QEMU virt + AAVMF) starts it, and it reaches
the end of boot and the serial shell: PCI, the virtio disk and its FOSFS volume, physical memory, the MMU page tables, the heap self-test, GIC timer ticks and the scheduler self-tests.
Run `make test-mf-aarch64-boot`."""
import os
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import test_aarch64_boot as base  # noqa: E402

IMAGE = Path(os.environ.get("FOSFOR_IMAGE", base.ROOT / "build/fosfor-mf-aarch64/fosfor-qemu.img"))


EL0_LINE = re.compile(r"EL0: hello ok, bad pointer ok, page fault ok, stack guard ok, undefined instruction ok, preempted spinner ok, "
                      r"ELF ok, big image ok, NEON state ok, 0 frames not returned\.")


def main() -> None:
    import http.server
    import threading
    page = b"<html><body><h1>Hello from the host</h1><a href=\"/x\">link</a></body></html>"

    class Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            self.send_response(200)
            self.send_header("Content-Type", "text/html")
            self.send_header("Content-Length", str(len(page)))
            self.end_headers()
            self.wfile.write(page)

        def log_message(self, *_):
            pass

    # QEMU's user-mode network reaches the host as 10.0.2.2.
    server = http.server.ThreadingHTTPServer(("0.0.0.0", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    port = server.server_address[1]
    text = base.boot(IMAGE, commands=("web -d 10.0.2.2:%d/" % port, "ed t.txt", "a", "hello", "from ed", ".", "w", "q", "cat t.txt", "fterm help", "version", "cpuinfo", "uptime", "nic", "run hello", "run thrcheck", "run netcheck", "fscheck readme.txt 0 7", "run httpd &0", "io r8 0x60", "usb", "disks"), usb=True, cpus=4,
                     usb_keys=("c", "p", "u", "i", "n", "f", "o", "ret"))
    base.check("server kernel (mf, slice 8): running from the UEFI loader at EL" in text, "the mf kernel starts on aarch64", text)
    base.check("Physical memory:" in text, "the physical allocator is up", text)
    base.check("Page tables: loaded" in text, "the MMU is on with our page tables", text)
    base.check("Heap: self-test passed" in text, "the heap works", text)
    ticks = re.search(r"(\d+) timer interrupts", text)
    base.check(ticks is not None and 8 <= int(ticks.group(1)) <= 12, "the GIC delivers timer ticks", text)
    base.check("Scheduler: 3 kernel threads took 15 interleaved steps" in text and " 0 stack frame(s) not returned" in text, "kernel threads switch and their stacks are returned", text)
    base.check("preemption ok" in text and "sleep ok" in text and "mutex ok" in text, "timer preemption, sleep and mutex work", text)
    base.check("SMP: 4 CPU(s) online (3 other started, 0 failed)." in text and "wake IPI ok" in text, "PSCI starts three more CPUs at EL1; threads run on all of them and the wake IPI works", text)
    base.check(EL0_LINE.search(text) is not None, "EL0 processes: system calls, faults contained, preemption, an ELF, NEON state across switches", text)
    base.check("reached the end of boot" in text, "boot reaches its end", text)
    base.check("PCI: config space at" in text and "MSI: GICv2m frame" in text, "PCI (ECAM) and MSI are up", text)
    base.check("00:02.0 1af4:1001" in text, "PCI lists the virtio disk", text)
    base.check("disk1: " in text and "(virtio 0)" in text, "the virtio disk is found", text)
    base.check("FOSFS: mounted on disk1" in text, "the boot disk's FOSFS volume is mounted", text)
    base.check("USB: 1 xHCI, 0 EHCI and 0 OHCI controller(s), 2 usable device(s)" in text, "xHCI finds the keyboard and the stick", text)
    base.check("USB keyboard 0627:0001" in text and "1 USB keyboard(s)" in text, "`usb` lists the keyboard", text)
    base.check(re.search(r"^disk2: .*USB", text, re.M) is not None or "USB storage 46f4:0001" in text, "the USB stick is a disk", text)
    base.check(text.count("features    fp asimd") >= 2, "keys typed on the USB keyboard reach the shell", text)
    base.check("fos> " in text, "the shell prompt appears on the serial console", text)
    base.check("server kernel (mf)" in text.split("fos> ", 1)[-1], "`version` answers", text)
    base.check("implementer" in text and "features" in text, "`cpuinfo` reads the ID registers", text)
    base.check(re.search(r"\d+ s$", text, re.M) is not None, "`uptime` answers", text)
    base.check("virtio-net at 00:01.0: MAC" in text, "the virtio-net card is brought up", text)
    base.check("nic1: virtio-net" in text, "`nic` lists it", text)
    shell = text.split("fos> ", 1)[-1]
    base.check("Hello from the host" in shell and "1. http://10.0.2.2:" in shell, "the web tape fetches a page from the host over the tape network calls and lists its links", text)
    base.check("written t.txt" in shell and "hello\nfrom ed" in shell.replace("\r", ""), "the ed tape edits and saves a file", text)
    base.check("Telnet (port 23)" in shell, "`fterm help` is printed from the tape's .fos_help section", text)
    base.check("thrcheck: PASS threads share memory" in shell, "tape threads, futexes and a kill with sleeping and spinning threads work at EL0", text)
    base.check("netcheck: 4/4 replies." in shell, "the netcheck tape gets a DHCP lease and pings through the tape network calls", text)
    base.check("fscheck: 7 bytes [Welcome]" in shell, "fs_read_at reads a byte range from a tape", text)
    base.check("httpd: NIC 1 at 10.0.2.15:80" in shell, "httpd starts as a service with its worker threads", text)
    base.check("no file `hello`" not in shell and "first file: readme.txt" in shell, "the boot volume's tapes are found from the shell thread (DMA from a stack buffer)", text)
    base.check("hello from a v2 tape" in shell and "primes up to 100000: 9592" in shell and "Process 12 exited with code 0." in shell,
               "an aarch64 tape runs at EL0 through the tape ABI (files, alloc, clock) and exits", text)
    base.check("no I/O port space" in text, "`io` explains there are no ports", text)
    # A firmware that enters at EL2 (a Pi 4): the kernel leaves it for EL1, where EL0 programs can run.
    el2 = base.boot(IMAGE, commands=("cpuinfo", "uptime"), el2=True, cpus=4)
    base.check("from the UEFI loader at EL2" in el2 and "running at EL1" in el2, "entered at EL2, the kernel drops to EL1 and carries on", el2)
    base.check("preemption ok" in el2 and "timer interrupts (INTID 30)" in el2, "the timer and the scheduler work after the drop", el2)
    base.check("SMP: 4 CPU(s) online (3 other started, 0 failed)." in el2 and "Scheduler: SMP ok (8 unpinned threads ran on 4 of 4" in el2, "PSCI starts three more CPUs after the drop from EL2 and they run threads", el2)
    base.check(EL0_LINE.search(el2) is not None, "EL0 processes run after the drop from EL2", el2)
    sd = base.boot(IMAGE, steps=(("disks\r", "fos> "),), sd=True)
    base.check(re.search(r"disk2: \d+ MiB, SD card", sd) is not None, "the SDHCI controller and its card are registered as a disk", sd)


def unattended() -> None:
    """A fatal exception and a hung service thread each write the `crash` file, reset the machine (PSCI SYSTEM_RESET) and the
    next boot reports the file and counts an unclean boot. The image is a copy with a `debug` file (written by the next boot)."""
    import subprocess
    import tempfile
    target = base.ROOT / "build/cargo-target"
    with tempfile.TemporaryDirectory() as scratch:
        debug = Path(scratch) / "debug"
        debug.write_text("reboot-on-crash\nreboot-delay 1\nwatchdog 6\n")
        for command, expected in (("debug crash now", "exception 4: ESR 0x02000000"), ("debug hang now", "watchdog: no progress")):
            image = Path(scratch) / "fosfor.img"
            subprocess.run([sys.executable, str(base.ROOT / "tools/build_image.py"), "--arch", "aarch64", "--loader", str(target / "aarch64-unknown-uefi/release/fosfor-loader.efi"),
                            "--kernel", str(target / "aarch64-unknown-none-softfloat/release/fosfor-kernel-mf"), "--file", f"debug={debug}", "--output", str(image)], check=True, capture_output=True)
            first = base.boot(image, commands=(command,), keep=True)
            base.check("Crash record written to the file `crash`." in first and "*** resetting in 1 s" in first, f"`{command}` records the crash and resets", first)
            second = base.boot(image, commands=("slots",), keep=True)
            base.check("Previous boot crashed (file `crash`, now removed):" in second and expected in second, f"the next boot reports the crash file ({expected})", second)
            base.check("unclean boots: 1" in second, "and counts the reset as an unclean boot", second)


def screen() -> None:
    """The pixel console (ramfb here, the HDMI/DSI framebuffer on a Pi): the boot text is drawn and a tape draws through gfx_*."""
    import tempfile
    shot = Path(tempfile.mkdtemp()) / "screen.ppm"
    text = base.boot(IMAGE, shot, ("version",))
    base.check("Framebuffer console:" in text and "characters" in text, "the boot framebuffer becomes the text console", text)
    total, lit = base.lit_pixels(shot) if shot.exists() else (0, 0)
    base.check(total > 0 and lit > 500, "the shell's text is on the screen", f"{lit}/{total} pixels lit")
    gfx_shot = shot.with_name("gfx.ppm")
    gfx = base.boot(IMAGE, None, (), (), ("run gfxcheck secs=3", "gfxcheck: drawn", gfx_shot))
    pixel, size = base.ppm_reader(gfx_shot)
    width, height = size
    base.check("gfxcheck: screen" in gfx and "gfxcheck: drawn" in gfx, "a tape reads the screen size and draws through gfx_* at EL0", gfx)
    base.check((pixel(10, 10), pixel(100, 10), pixel(170, 10)) == ((255, 0, 0), (0, 255, 0), (0, 0, 255)), "gfx_fill: red, green and blue bars have the right colours", "")
    base.check(pixel(100, 102) == (100, 0, 0) and pixel(width - 1, height - 1) == (255, 0, 255), "gfx_blit: a ramp and a clipped block", "")
    base.check("gfxcheck: done" in gfx, "the tape ends and the shell returns", gfx)


def rotation() -> None:
    """`rotate r` in settings: tapes draw on an upright logical screen (width and height swap) turned onto the framebuffer."""
    import subprocess
    import tempfile
    target = base.ROOT / "build/cargo-target"
    with tempfile.TemporaryDirectory() as scratch:
        settings = Path(scratch) / "settings"
        settings.write_text("rotate r\n")
        image = Path(scratch) / "fosfor.img"
        subprocess.run([sys.executable, str(base.ROOT / "tools/build_image.py"), "--arch", "aarch64", "--loader", str(target / "aarch64-unknown-uefi/release/fosfor-loader.efi"),
                        "--kernel", str(target / "aarch64-unknown-none-softfloat/release/fosfor-kernel-mf"), "--file", f"settings={settings}",
                        "--file", f"gfxcheck={base.ROOT / 'build/fosfor-aarch64/tapes/gfxcheck'}", "--output", str(image)], check=True, capture_output=True)
        shot = Path(scratch) / "rot.ppm"
        text = base.boot(image, None, (), (), ("run gfxcheck secs=3", "gfxcheck: drawn", shot))
        pixel, (width, height) = base.ppm_reader(shot)
        base.check("settings: rotate [0] r" in text, "`rotate r` is a setting of mf", text)
        base.check(f"gfxcheck: screen {height}x{width}" in text, "the tape sees the swapped, upright screen size", text)
        # logical (x, y) is physical (W-1-y, x) turned right
        base.check((pixel(width - 11, 10), pixel(width - 11, 100), pixel(width - 11, 170)) == ((255, 0, 0), (0, 255, 0), (0, 0, 255)), "gfx_fill: the bars land turned a quarter", "")
        base.check(pixel(width - 103, 100) == (100, 0, 0) and pixel(0, height - 1) == (255, 0, 255), "gfx_blit: the ramp and the clipped block land turned", "")


def pins() -> None:
    """The Pi's header, on a simulated pin bank (`gpio-sim`, QEMU has no BCM2711): `gpio`, a denied pin, a tape-less `i2c`/`i2s`.
    The image is a copy with a `settings` file."""
    import subprocess
    import tempfile
    target = base.ROOT / "build/cargo-target"
    with tempfile.TemporaryDirectory() as scratch:
        settings = Path(scratch) / "settings"
        settings.write_text("gpio-sim\ngpio-deny 5\n")
        image = Path(scratch) / "fosfor.img"
        subprocess.run([sys.executable, str(base.ROOT / "tools/build_image.py"), "--arch", "aarch64", "--loader", str(target / "aarch64-unknown-uefi/release/fosfor-loader.efi"),
                        "--kernel", str(target / "aarch64-unknown-none-softfloat/release/fosfor-kernel-mf"), "--file", f"settings={settings}", "--output", str(image)], check=True, capture_output=True)
        text = base.boot(image, commands=("gpio mode 7 out 1", "gpio read 7", "gpio mode 5 out 1", "gpio pull 9 up", "gpio read 9", "i2c status", "i2s probe", "audio", "cpufreq", "midi", "midi send 90 3c 64", "midi bogus"))
        shell = text.split("fos> ", 1)[-1]
        base.check("settings: gpio-sim -- a RAM-backed GPIO bank" in text, "gpio-sim is a setting of mf on aarch64", text)
        base.check("is not supported by the server kernel yet" not in text, "gpio-sim and gpio-deny are not reported as ignored", text)
        base.check("GPIO7" in shell and "out" in shell and "high" in shell, "`gpio mode 7 out 1` drives a simulated pin high", text)
        base.check("gpio:" in shell and "not allowed" in shell.lower(), "a denied pin is refused", text)
        base.check("midi: no USB MIDI device" in shell and "midi send:" in shell and "Usage: midi" in shell, "`midi` lists, sends and explains usage (no device under QEMU)", text)
        base.check("cpufreq: needs the Raspberry Pi 4" in shell, "`cpufreq` declines off a Pi 4 (QEMU has no VideoCore mailbox)", text)
        base.check("pull up" in shell, "`gpio pull` sets and shows a pull-up", text)


def remote_gpio() -> None:
    """The Pi's pins served over the network (`gpio-remote` + `synckey`, a simulated bank): a host signs a challenge, writes a pin and
    reads it back; a write to a denied pin and a wrong key are refused."""
    import struct
    import subprocess
    import tempfile
    sys.path.insert(0, str(base.ROOT / "tools"))
    import gpio as client
    from test_boot import BROADCAST, TAG_DISCOVER_REPLY, TAG_DISCOVER_REQUEST
    target = base.ROOT / "build/cargo-target"
    key = b"correct horse"
    seen = {}

    def ask(wire, guest, op, args=b"", key=key):
        wire.send(guest, client.TAG_REQUEST, b"\0")
        nonce = None
        for _, tag, payload in wire.frames(3):
            if tag == client.TAG_REPLY and payload[0] == 0:
                nonce = payload[1:1 + client.NONCE_LEN]
                break
        if nonce is None:
            return None
        wire.send(guest, client.TAG_REQUEST, bytes([op]) + nonce + args + client.mac_of(b"fosgpio1", op, nonce, args, key))
        for _, tag, payload in wire.frames(3):
            if tag == client.TAG_REPLY:
                size = client.DATA_LEN[op] if payload[0] == 0 or (op == client.OP_SET and payload[0] == 4) else 0
                data = payload[1 + client.NONCE_LEN:1 + client.NONCE_LEN + size]
                good = client.mac_of(b"fosgpio1r", payload[0], nonce, data, key)
                return payload[0], data, payload[1 + client.NONCE_LEN + size:1 + client.NONCE_LEN + size + client.MAC_LEN] == good
        return None

    def talk(wire) -> None:
        guest = None
        for _ in range(60):
            wire.send(BROADCAST, TAG_DISCOVER_REQUEST, b"host")
            for source, tag, payload in wire.frames(0.25):
                if tag == TAG_DISCOVER_REPLY:
                    guest = source
            if guest:
                break
        seen["mode"] = ask(wire, guest, client.OP_FUNCTION, bytes([7, 1, 1]))
        seen["state"] = ask(wire, guest, client.OP_STATE)
        seen["denied"] = ask(wire, guest, client.OP_FUNCTION, bytes([5, 1, 1]))
        seen["wrongkey"] = ask(wire, guest, client.OP_STATE, key=b"battery staple")

    with tempfile.TemporaryDirectory() as scratch:
        files = {"settings": "gpio-sim\ngpio-remote\ngpio-deny 5\n", "synckey": key.decode() + "\n"}
        args = []
        for name, text in files.items():
            (Path(scratch) / name).write_text(text)
            args += ["--file", f"{name}={Path(scratch) / name}"]
        image = Path(scratch) / "fosfor.img"
        subprocess.run([sys.executable, str(base.ROOT / "tools/build_image.py"), "--arch", "aarch64", "--loader", str(target / "aarch64-unknown-uefi/release/fosfor-loader.efi"),
                        "--kernel", str(target / "aarch64-unknown-none-softfloat/release/fosfor-kernel-mf"), *args, "--output", str(image)], check=True, capture_output=True)
        text = base.boot(image, wire=talk)
    base.check(seen.get("mode") and seen["mode"][0] == 0 and seen["mode"][2], "a signed remote `mode 7 out 1` is accepted and the answer is signed", str(seen))
    state = seen.get("state")
    base.check(bool(state) and state[0] == 0 and state[2] and client.decode_state(state[1])["levels"] >> 7 & 1, "the remote state shows pin 7 high", str(seen))
    base.check(bool(seen.get("denied")) and seen["denied"][0] == 4, "a denied pin is refused over the network", str(seen))
    base.check(seen.get("wrongkey") is None, "a request signed with another key is not answered", str(seen))


def install() -> None:
    """`install disk2 ... ext4=data` copies the ESP (the Pi/aarch64 loader), the kernel and the files to a blank disk and
    formats an ext4 volume; the new disk then boots alone and lists the volume, and e2fsck finds the volume clean."""
    import subprocess
    import tempfile
    with tempfile.TemporaryDirectory(dir=base.ROOT / "build") as scratch:
        target = Path(scratch) / "target.img"
        with open(target, "wb") as handle:
            handle.truncate(256 << 20)
        first = base.boot(IMAGE, commands=("install disk2 fosfs=64M ext4=scratch", "install disk2 confirm", "disks", "bootcode"), extra=target)
        base.check("It is blank." in first and "becomes an ext4 volume 'scratch'" in first, "`install` announces what it will do", first)
        base.check("ext4 volume 'scratch' ready" in first, "the install formats the ext4 volume", first)
        base.check("is now bootable" in first, "the install finishes", first)
        base.check(re.search(r"ext\d: 'scratch'", first.rsplit("fos> disks", 1)[-1]) is not None, "`disks` lists the ext4 volume", first)
        base.check("UEFI loader: \\EFI\\BOOT\\BOOTAA64.EFI" in first, "`bootcode` reads the AArch64 loader from the ESP", first)
        found = subprocess.run(["sgdisk", "--print", str(target)], capture_output=True, text=True).stdout
        base.check("EF00" in found and "8300" in found, "the target has an ESP and a Linux partition", found)
        start = int(re.search(r"^\s+4\s+(\d+)\s+(\d+)", found, re.M).group(1))
        end = int(re.search(r"^\s+4\s+(\d+)\s+(\d+)", found, re.M).group(2))
        volume = Path(scratch) / "volume.img"
        with open(target, "rb") as source, open(volume, "wb") as out:
            source.seek(start * 512)
            out.write(source.read((end - start + 1) * 512))
        fsck = subprocess.run(["e2fsck", "-fn", str(volume)], capture_output=True, text=True)
        base.check(fsck.returncode == 0, "e2fsck finds the installed ext4 volume clean", fsck.stdout + fsck.stderr)
        second = base.boot(target, commands=("disks", "ls"), keep=True)
        base.check("FOSFS: mounted on disk1" in second, "the installed disk boots alone and mounts its FOSFS volume", second)
        base.check(re.search(r"ext\d: 'scratch'", second) is not None, "and lists its ext4 volume", second)


def network() -> None:
    """The guest's virtio-net on a multicast socket network: discovery, then a `term net` session (receive and transmit)."""
    from test_boot import BROADCAST, TAG_DISCOVER_REPLY, TAG_DISCOVER_REQUEST, TAG_HELLO, TAG_DATA
    seen = {}

    def talk(wire) -> None:
        guest = None
        for _ in range(60):
            wire.send(BROADCAST, TAG_DISCOVER_REQUEST, b"host")
            for source, tag, payload in wire.frames(0.25):
                if tag == TAG_DISCOVER_REPLY:
                    guest, seen["name"] = source, payload.rstrip(b"\0").decode("latin-1")
            if guest:
                break
        base.check(guest is not None, "the guest answers a discovery probe over virtio-net", "")
        wire.send(guest, TAG_HELLO)
        wire.until("fos> ", 10)
        wire.send(guest, TAG_DATA, b"version\n")
        seen["version"] = wire.until("fos> ", 10)

    text = base.boot(IMAGE, wire=talk)
    base.check(seen.get("name", "").startswith("fos-") or seen.get("name"), "the guest announces its name", text)
    base.check("server kernel (mf)" in seen.get("version", ""), "a `term net` session runs `version` over virtio-net", seen.get("version", ""))


if __name__ == "__main__":
    main()
    unattended()
    screen()
    rotation()
    pins()
    remote_gpio()
    install()
    network()
