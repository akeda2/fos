#!/usr/bin/env python3
"""The `clock` tape against a fake NTP server (10.7.0.99, a day ahead of the host: tools/test_ptp_ntp.py): the tape gets its
address from `ip=`, asks the server, sets the machine's clock (`date` afterwards shows the server's time plus the zone), and
draws the face. The screenshot is decoded: the seven-segment digits must read the NTP time (HH:MM and the seconds), the red
dots must be lit up to the second, `h` switches to 12 hours, `k` changes the theme, Escape returns the console.
Run with `make test-clock`."""

import sys
import tempfile
import threading
import time
from datetime import datetime, timedelta, timezone
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_audio_net import Guest  # noqa: E402
from test_boot import check  # noqa: E402
from test_gfx import screenshot  # noqa: E402
from test_ptp_ntp import SKEW, serve  # noqa: E402

MAC, MASTER_MAC = "52:54:00:12:35:01", "52:54:00:12:35:02"
ZONE_HOURS = 2
RED, BACKGROUND, YELLOW = (0xff, 0x1a, 0x1a), (0, 0, 0), (0xff, 0xd4, 0x00)
SEGMENTS = [0x3f, 0x06, 0x5b, 0x4f, 0x66, 0x6d, 0x7d, 0x07, 0x7f, 0x6f]
HOUR_SHAPES = {segments: digit for digit, segments in enumerate(SEGMENTS)}


class Face:
    """tapes/clock/src/main.rs `Geometry` and `draw_face`, in Python."""

    def __init__(self, width, height):
        self.cx, self.d = width // 2, max(min(width, height - 58) - 24, 120)
        self.cy = 34 + (height - 58) // 2
        self.ring = self.d // 2 - self.d // 52

    def s(self, per_mille, least):
        return max(self.d * per_mille // 1000, least)

    def dot(self, index):
        import math
        angle = index * math.tau / 60
        return round(self.cx + self.ring * math.sin(angle)), round(self.cy - self.ring * math.cos(angle))

    def hhmm_boxes(self):
        w, h, t = self.s(125, 6), self.s(220, 12), self.s(28, 2)
        pair, colon_gap, colon_w = self.s(20, 2), self.s(15, 2), self.s(30, 3)
        total = 4 * w + 2 * pair + 2 * colon_gap + colon_w
        x, y = self.cx - total // 2, self.cy - self.s(190, 0)
        boxes = []
        for index in range(4):
            boxes.append((x, y, w, h, t))
            x += w
            if index in (0, 2):
                x += pair
            elif index == 1:
                x += colon_gap + colon_w + colon_gap
        return boxes

    def seconds_boxes(self):
        w, h, t, gap = self.s(57, 4), self.s(100, 8), self.s(14, 2), self.s(12, 2)
        x, y = self.cx - (2 * w + gap) // 2, self.cy + self.s(90, 0)
        return [(x, y, w, h, t), (x + w + gap, y, w, h, t)]


def read_digit(pixel, box):
    """The digit whose lit segments (yellow at each segment's centre) a box shows, or None for blank."""
    x, y, w, h, t = box
    middle = y + h // 2
    centres = [(1, x + w // 2, y + t // 2), (2, x + w - t // 2, y + h // 4), (4, x + w - t // 2, y + 3 * h // 4), (8, x + w // 2, y + h - t // 2),
               (16, x + t // 2, y + 3 * h // 4), (32, x + t // 2, y + h // 4), (64, x + w // 2, middle)]
    lit = sum(bit for bit, cx, cy in centres if pixel(cx, cy) == YELLOW)
    return HOUR_SHAPES.get(lit)


def main():
    stop, answered = threading.Event(), []
    server = threading.Thread(target=serve, args=(stop, answered))
    server.start()
    with tempfile.TemporaryDirectory() as tmp:
        monitor = Path(tmp) / "monitor"
        guest = Guest(tmp, "clock", MAC, extra=["-monitor", f"unix:{monitor},server,nowait"])
        try:
            guest.wait_for("M3 complete")
            guest.wait_for("fos> ")
            mark = guest.send(f"run clock source=ntp ip=10.7.0.1/24 ntp=10.7.0.99 resync=10 tz=+{ZONE_HOURS}")
            guest.wait_for("clock: NTP time ", 30, mark)
            check(len(answered) >= 1, f"the NTP server was asked ({len(answered)} requests)")
            time.sleep(4)
            wanted = datetime.now(timezone.utc) + timedelta(seconds=SKEW, hours=ZONE_HOURS)
            width, height, pixel = screenshot(monitor, "clock.ppm")
            face = Face(width, height)
            digits = [read_digit(pixel, box) for box in face.hhmm_boxes()]
            seconds = [read_digit(pixel, box) for box in face.seconds_boxes()]
            check(None not in digits + seconds, f"every digit reads as a digit ({digits} {seconds})")
            shown = datetime(wanted.year, wanted.month, wanted.day, digits[0] * 10 + digits[1], digits[2] * 10 + digits[3], seconds[0] * 10 + seconds[1])
            check(abs((shown - wanted.replace(tzinfo=None, microsecond=0)).total_seconds()) < 12, f"the digits show the NTP time plus the zone ({shown:%H:%M:%S}, wanted about {wanted:%H:%M:%S})")
            lit = sum(1 for index in range(60) if pixel(*face.dot(index)) == RED)
            check(abs(lit - (seconds[0] * 10 + seconds[1] + 1)) <= 1, f"the red dots are lit up to the second ({lit} lit at second {seconds[0]}{seconds[1]})")

            guest.process.stdin.write(b"h")
            guest.process.stdin.flush()
            time.sleep(2)
            _, _, pixel = screenshot(monitor, "clock12.ppm")
            twelve = [read_digit(pixel, box) for box in face.hhmm_boxes()]
            expected_hour = (wanted.hour + 11) % 12 + 1
            check(twelve[1] == expected_hour % 10 and twelve[2] is not None, f"h switches to 12 hours ({twelve}, hour {expected_hour})")
            guest.process.stdin.write(b"k")
            guest.process.stdin.flush()
            time.sleep(2)
            _, _, pixel = screenshot(monitor, "clock_light.ppm")
            check(pixel(8, height // 2) == (0xff, 0xff, 0xff), "k switches to the light theme (white background)")

            guest.process.stdin.write(b"\x1b")
            guest.process.stdin.flush()
            guest.wait_for("clock: stopped.", 10, mark)
            time.sleep(1)
            date_mark = guest.send("date")
            guest.wait_for("(local time, from the RTC)", 10, date_mark)
            shown = guest.text()[date_mark:].split("(local")[0].split()[-2:]
            when = datetime.strptime(" ".join(shown), "%Y-%m-%d %H:%M:%S")
            check(abs((when - (datetime.now(timezone.utc) + timedelta(seconds=SKEW, hours=ZONE_HOURS)).replace(tzinfo=None)).total_seconds()) < 90, f"the machine's clock was set from the server ({when})")

            mark = guest.send("run clock ntp=off")
            time.sleep(3)
            _, _, pixel = screenshot(monitor, "clock_free.ppm")
            free = [read_digit(pixel, box) for box in face.seconds_boxes()]
            check(None not in free, f"ntp=off shows the machine's clock ({free})")
            guest.process.stdin.write(b"\x1b")
            guest.process.stdin.flush()
            guest.wait_for("clock: stopped.", 10, mark)

            mark = guest.send("run clock ntp=off lead=3")
            time.sleep(2)
            seen = set()
            for _ in range(10):
                _, _, pixel = screenshot(monitor, "clock_lead.ppm")
                second = (lambda d: d[0] * 10 + d[1] if None not in d else None)([read_digit(pixel, box) for box in face.seconds_boxes()])
                time.sleep(0.9)
                if second is None:
                    continue
                target = second // 5 * 5 + 5
                should = second % 5 <= 3
                lit = pixel(*face.dot(target)) == RED
                seen.add(second % 5)
                check(lit == should, f"lead=3: at second {second} the mark at {target} is {'lit' if lit else 'dark'}, wanted {'lit' if should else 'dark'}")
            check(len(seen) >= 3, f"lead=3: saw {len(seen)} different positions in the 5-second cycle")
            _, _, pixel = screenshot(monitor, "clock_dim.ppm")
            bar = lambda: sum(1 for x in range(16, 400) for y in range(6, 20) if pixel(x, y) == YELLOW)
            check(bar() == 0, "the status line is dim once the bars have timed out")
            guest.process.stdin.write(b"n")
            guest.process.stdin.flush()
            time.sleep(1.5)
            _, _, pixel = screenshot(monitor, "clock_lit.ppm")
            check(bar() > 50, "a key lights the status line")
            guest.process.stdin.write(b"\x1b")
            guest.process.stdin.flush()
            guest.wait_for("clock: stopped.", 10, mark)

            master = Guest(tmp, "master", MASTER_MAC)
            master.wait_for("M3 complete")
            master.wait_for("fos> ")
            master_mark = master.send("run ptp master both ip=10.7.0.2/24 ntp=10.7.0.99 secs=300")
            master.wait_for("ptp master: NIC 1 10.7.0.2", 40, master_mark)
            for mode in (" spin", "", " l2"):
                mark = guest.send(f"run clock source=ptp{mode} ip=10.7.0.1/24 tz=+{ZONE_HOURS}")
                guest.wait_for("clock: PTP locked to a master", 40, mark)
                time.sleep(3)
                wanted = datetime.now(timezone.utc) + timedelta(seconds=SKEW, hours=ZONE_HOURS)
                _, _, pixel = screenshot(monitor, "clock_ptp.ppm")
                digits = [read_digit(pixel, box) for box in face.hhmm_boxes()]
                seconds = [read_digit(pixel, box) for box in face.seconds_boxes()]
                check(None not in digits + seconds, f"PTP{mode}: every digit reads as a digit ({digits} {seconds})")
                shown = datetime(wanted.year, wanted.month, wanted.day, digits[0] * 10 + digits[1], digits[2] * 10 + digits[3], seconds[0] * 10 + seconds[1])
                check(abs((shown - wanted.replace(tzinfo=None, microsecond=0)).total_seconds()) < 12, f"PTP{mode}: the digits show the master's time plus the zone ({shown:%H:%M:%S}, wanted about {wanted:%H:%M:%S})")
                guest.process.stdin.write(b"\x1b")
                guest.process.stdin.flush()
                guest.wait_for("clock: stopped.", 10, mark)
            # source=auto (the default): Ethernet PTP first, NTP when the master goes away, NTP alone when there never was one.
            mark = guest.send(f"run clock ip=10.7.0.1/24 ntp=10.7.0.99 tz=+{ZONE_HOURS}")
            guest.wait_for("clock: PTP locked to a master over Ethernet", 40, mark)
            check("asking NTP" not in guest.text()[mark:], "auto: NTP was not asked while a PTP master answered")
            master.stop()
            guest.wait_for("no PTP master for 30 s; asking NTP", 80, mark)
            guest.wait_for("clock: time from NTP", 20, mark)
            guest.process.stdin.write(b"\x1b")
            guest.process.stdin.flush()
            guest.wait_for("clock: stopped.", 10, mark)
            mark = guest.send(f"run clock ip=10.7.0.1/24 ntp=10.7.0.99 tz=+{ZONE_HOURS}")
            guest.wait_for("clock: time from NTP", 30, mark)
            guest.process.stdin.write(b"\x1b")
            guest.process.stdin.flush()
            guest.wait_for("clock: stopped.", 10, mark)
        finally:
            stop.set()
            server.join()
            guest.stop()
    print("clock: all checks passed")


if __name__ == "__main__":
    main()
