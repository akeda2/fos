#!/usr/bin/env python3
"""Make crates/audio-decode/testdata/fixture.ape from synthetic audio (needs Monkey's Audio's `mac`)."""
import math, struct, subprocess, sys, wave

MAC, OUT = sys.argv[1], sys.argv[2]
RATE, N = 44100, 8192
seed = 0x1234_5678
frames = bytearray()
for i in range(N):
    seed = (seed * 1103515245 + 12345) & 0x7fffffff
    noise = (seed >> 8) % 2001 - 1000
    t = i / RATE
    left = 9000 * math.sin(2 * math.pi * 440 * t) + 4000 * math.sin(2 * math.pi * 1250 * t) + noise
    right = 8000 * math.sin(2 * math.pi * 660 * t + 1.0) + 3000 * math.sin(2 * math.pi * 97 * t) - noise // 2
    frames += struct.pack("<hh", int(left), int(right))
with wave.open("/tmp/fixture.wav", "wb") as w:
    w.setnchannels(2); w.setsampwidth(2); w.setframerate(RATE); w.writeframes(bytes(frames))
subprocess.run([MAC, "/tmp/fixture.wav", OUT, "-c3000"], check=True, stdout=subprocess.DEVNULL)

def item(key, value):
    v = value.encode()
    return struct.pack("<II", len(v), 0) + key.encode() + b"\0" + v

items = item("Title", "Fixture") + item("Artist", "Test")
def header(flags):
    return b"APETAGEX" + struct.pack("<IIII", 2000, len(items) + 32, 2, flags) + b"\0" * 8
with open(OUT, "ab") as f:
    f.write(header(0xA0000000) + items + header(0x80000000))
