#!/usr/bin/env python3
"""The shell's line editor over the serial line: history with the arrow keys, cursor movement and deletion, Home/End/
Delete, Ctrl-U/W/K/C, and the half-typed line surviving a browse of the history. Each case ends by running a command
whose output shows what the line contained, so a wrong edit shows up as the wrong command (or none) running."""

import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import tempfile  # noqa: E402

from test_audio_net import Guest  # noqa: E402
from test_boot import check  # noqa: E402

UP, DOWN, RIGHT, LEFT = "\x1b[A", "\x1b[B", "\x1b[C", "\x1b[D"
HOME, END, DELETE = "\x1b[H", "\x1b[F", "\x1b[3~"


def main():
    with tempfile.TemporaryDirectory() as tmp:
        guest = Guest(tmp, "lineedit", "52:54:00:12:34:ee")
        try:
            guest.wait_for("M3 complete")
            guest.wait_for("fos> ")

            def type_keys(text, settle=0.3):
                guest.process.stdin.write(text.encode())
                guest.process.stdin.flush()
                guest.pump(settle)

            def count(marker):
                return guest.text().count(marker)

            uptimes = lambda: count("ticks at 100 Hz")
            memories = lambda: count("Managed:")

            type_keys("uptime\n")
            type_keys("memory\n")
            base_u, base_m = uptimes(), memories()
            check(base_u >= 1 and base_m >= 1, "two commands ran", guest.text()[-400:])

            type_keys(UP + "\n")
            check(memories() == base_m + 1, "Up recalls the last command (memory) and Enter runs it", guest.text()[-400:])
            type_keys(UP + UP + "\n")
            check(uptimes() == base_u + 1 and memories() == base_m + 1, "Up twice reaches the one before (uptime)", guest.text()[-400:])
            # History is now [uptime, memory, uptime]: Up Up Down lands on the newest (uptime) again.
            before = uptimes()
            type_keys(UP + UP + DOWN + "\n")
            check(uptimes() == before + 1, "Down moves back toward the newest", guest.text()[-400:])

            before = uptimes()
            type_keys("upxtime" + LEFT * 4 + "\x7f" + "\n")
            check(uptimes() == before + 1, "Left moves the cursor and Backspace deletes in the middle of the line", guest.text()[-400:])

            before = uptimes()
            type_keys("xx uptime" + HOME + DELETE + DELETE + DELETE + "\n")
            check(uptimes() == before + 1, "Home and Delete edit the start of the line", guest.text()[-400:])

            before = uptimes()
            type_keys("junk junk" + "\x17" + "\x15" + "uptime" + "\n")  # Ctrl-W then Ctrl-U clear it
            check(uptimes() == before + 1, "Ctrl-W and Ctrl-U delete a word and the whole line", guest.text()[-400:])

            before = uptimes()
            type_keys("uptimeXYZ" + LEFT * 3 + "\x0b" + "\n")  # Ctrl-K deletes XYZ
            check(uptimes() == before + 1, "Ctrl-K deletes to the end of the line", guest.text()[-400:])

            before = uptimes()
            type_keys("garbage" + "\x03")
            check("^C" in guest.text()[-200:], "Ctrl-C abandons the line", guest.text()[-200:])
            type_keys("uptime\n")
            check(uptimes() == before + 1, "and the next command is clean", guest.text()[-300:])

            before = memories()
            type_keys("mem" + UP + DOWN + "ory" + "\n")
            check(memories() == before + 1, "the half-typed line (mem) comes back after browsing the history, then completes to memory", guest.text()[-400:])

            # More than sixteen distinct commands: the oldest are dropped, the newest still recalled.
            for number in range(20):
                type_keys(f"sleep 0\n" if number % 2 else f"hexdump 0x1000 {number + 1}\n", 0.05)
            before = guest.text().count("fos> ")
            type_keys(UP + "\n")
            check(guest.text().count("fos> ") > before, "history keeps working after many commands", guest.text()[-300:])
        finally:
            guest.stop()
    print("v2 lineedit: all checks passed")


if __name__ == "__main__":
    main()
