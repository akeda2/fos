#!/usr/bin/env python3
"""Scripts and bare command names in the v2 shell: `NAME` runs the script `NAME.sh` (comments, blank lines, `$1`..`$9`,
`$*`, nesting with a limit) or the tape `NAME`; built-in commands win; `start` can use them; an unknown word is reported."""

import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_audio_net import Guest  # noqa: E402
from test_boot import check  # noqa: E402

FILES = {
    "hi.sh": "# a comment\n\nuptime\nmemory\n",
    "args.sh": "write out.txt first=$1 second=$2 all=$*\ncat out.txt\n",
    "outer.sh": "inner.sh x\nmemory\n",
    "inner.sh": "uptime\n",
    "loop.sh": "loop\n",
    "ls.sh": "uptime\n",
    "start": "hi\n",
}


def main():
    with tempfile.TemporaryDirectory() as tmp:
        files = {name: text.replace("\\\n", "\n") for name, text in FILES.items()}
        guest = Guest(tmp, "scripts", "52:54:00:12:34:ef", files=files)
        try:
            guest.wait_for("M3 complete")
            guest.wait_for("fos> ")
            boot = guest.text()
            check("start> hi" in boot and "hi.sh> uptime" in boot and "hi.sh> memory" in boot, "the start file runs a script by its bare name", boot[-700:])
            check("a comment" not in boot.split("hi.sh>")[1], "comments and blank lines are skipped", boot[-500:])

            mark = guest.send("hi")
            guest.pump(1.0)
            text = guest.text()[mark:]
            check("hi.sh> uptime" in text and "ticks at 100 Hz" in text and "Managed:" in text, "typing the name runs NAME.sh", text)

            mark = guest.send("start")
            guest.pump(1.5)
            text = guest.text()[mark:]
            check("start file: running 1 command(s)." in text and "start> hi" in text and "hi.sh> uptime" in text, "typing `start` runs the start file again", text)

            mark = guest.send("args a b c")
            guest.pump(1.0)
            text = guest.text()[mark:]
            check("first=a second=b all=a b c" in text, "$1, $2 and $* are replaced by the arguments", text)

            mark = guest.send("outer")
            guest.pump(1.0)
            text = guest.text()[mark:]
            check("outer.sh> inner.sh x" in text and "inner.sh> uptime" in text and "outer.sh> memory" in text, "a script can run another script", text)

            mark = guest.send("loop")
            guest.pump(4.0)
            text = guest.text()[mark:]
            check("nested too deep" in text, "script nesting is limited", text)

            mark = guest.send("hello")
            guest.pump(1.5)
            text = guest.text()[mark:]
            check("Tape returned" in text, "a bare name with no script runs the tape of that name", text)

            mark = guest.send("ls")
            guest.pump(1.0)
            text = guest.text()[mark:]
            check("hi.sh" in text and "ls.sh>" not in text, "a built-in command wins over a script of the same name", text)

            mark = guest.send("nosuchcommand")
            guest.pump(0.5)
            check("Unknown command: nosuchcommand" in guest.text()[mark:], "an unknown word is still reported", guest.text()[mark:])
        finally:
            guest.stop()
    print("scripts: all checks passed")


if __name__ == "__main__":
    main()
