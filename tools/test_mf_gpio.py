#!/usr/bin/env python3
"""`gpio @HOST` on the server kernel (docs/big-kernel.md): kernel-mf is a GPIO client with the code the embedded kernel uses
(`crates/kernel-core/src/gpio_client.rs`); it has no pins itself. alpha is an embedded-kernel guest with a simulated bank
(`gpio-sim`, remote writes on, pin 5 denied), charlie has another key. Also: an embedded-kernel guest (bravo) asks the mf machine
and is told it has no pins. Checks: local `gpio` says no pins; remote list/read/write/refusal; a wrong key gets no good answer.
QEMU only: real pins are for the Pi."""

import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import test_audio_net as audio_net  # noqa: E402
import test_debug_mode as base  # noqa: E402
from test_boot import ROOT, check  # noqa: E402

base.KERNEL = ROOT / "build/cargo-target/x86_64-fos-kernel/release/fosfor-kernel-mf"
audio_net.GROUP, audio_net.PORT = base.MCAST_GROUP, base.MCAST_PORT
READY = "entering the service loop"


def main():
    with tempfile.TemporaryDirectory() as tmp:
        machine = base.Machine(tmp, {"hostname": "mfhost\n", "synckey": "correct horse\n"}, {})
        machine.start()
        guests = []
        try:
            machine.wait_for(READY)
            machine.connect()
            alpha = audio_net.Guest(tmp, "gpio-a", "52:54:00:12:36:a1", files={"hostname": "alpha", "synckey": "correct horse\n", "settings": "gpio-sim\ngpio-remote\ngpio-deny 5\n"})
            charlie = audio_net.Guest(tmp, "gpio-c", "52:54:00:12:36:c3", files={"hostname": "charlie", "synckey": "battery staple\n", "settings": "gpio-sim\ngpio-remote\n"})
            bravo = audio_net.Guest(tmp, "gpio-b", "52:54:00:12:36:b2", files={"hostname": "bravo", "synckey": "correct horse\n"})
            guests = [alpha, charlie, bravo]
            for guest in guests:
                guest.wait_for("M3 complete")
                guest.wait_for("fos> ")

            out = machine.run("gpio list")
            check("no GPIO pins" in out, "the server kernel has no pins of its own", out[-300:])
            out = machine.run("gpio bogus")
            check("Usage: gpio" in out, "a bad command prints the usage", out[-300:])
            out = machine.run("nic discover", 30)
            check("alpha1" in out and "charlie1" in out, "nic discover finds the machines", out[-400:])
            alpha.pump(2.5)

            out = machine.run("gpio @alpha1 list", 30)
            check("GPIO9" in out and "pins shown" in out, "list shows alpha's pins", out[-500:])
            alpha.pump(2.5)
            out = machine.run("gpio @alpha1 mode 8 out 1", 30)
            check("done" in out, "mf changes a pin remotely", out[-300:])
            alpha.pump(2.5)
            out = machine.run("gpio @alpha1 read 8", 30)
            check("GPIO8" in out and "out" in out and "high" in out, "and reads it back", out[-300:])
            alpha.pump(2.5)
            out = machine.run("gpio @alpha1 mode 5 out 1", 30)
            check("not allowed" in out, "a denied pin is refused", out[-300:])
            alpha.pump(2.5)
            out = machine.run("gpio @alpha1 pull 9 up", 30)
            check("done" in out, "a pull can be set", out[-300:])
            alpha.pump(2.5)
            out = machine.run("gpio @alpha1 read 9", 30)
            check("pull up" in out and "high" in out, "and shows", out[-300:])
            alpha.pump(2.5)
            out = machine.run("gpio @charlie1 read 9", 30)
            check("not signed" in out or "no answer" in out, "another key gets no good answer", out[-300:])
            out = machine.run("gpio @nobody read 9", 30)
            check("no machine called" in out, "an unknown name is reported", out[-300:])

            mark = bravo.send("gpio @mfhost1 read 9")
            bravo.wait_for("fos> ", 40, mark)
            out = bravo.text()[mark:]
            check("no GPIO pins" in out, "an embedded machine asking the server kernel is told it has none", out[-300:])
        finally:
            for guest in guests:
                guest.stop()
            machine.stop()
    print("test-mf-gpio: all checks passed")


if __name__ == "__main__":
    main()
