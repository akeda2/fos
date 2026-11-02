#!/usr/bin/env python3
"""Audio for tapes on the server kernel (`make test-mf-audio`): the unchanged `play` tape runs in ring 3 and plays a stereo
16-bit WAV through the shared audio facade onto QEMU's intel-hda codec; QEMU's WAV backend records it and the recording must
hold the 440 Hz tone, at -smp 1, 2 and 8 (the tape is pinned to CPU 0 by its first audio call). Then `record` runs against
the `none` backend (silence): it must start, run and end cleanly, and a second `play` after it works (the stream was
released). USB audio is checked on real hardware only."""
import os
import select
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_hda import frequency_of, read_wav  # noqa: E402
from test_usb_audio import write_wav  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent
KERNEL = ROOT / "build/cargo-target/x86_64-fos-kernel/release/fosfor-kernel-mf"
LOADER = ROOT / "build/cargo-target/x86_64-unknown-uefi/release/fosfor-loader.efi"
TAPES = ROOT / "build/fosfor/tapes"
CODE = next(p for p in ["/usr/share/OVMF/OVMF_CODE_4M.fd", "/usr/share/OVMF/OVMF_CODE.fd",
                        "/usr/share/edk2/ovmf/OVMF_CODE.fd"] if Path(p).exists())


def run(smp, tmp, usb=False):
    wav_in, wav_out = tmp / "a.wav", tmp / f"out{smp}{"u" if usb else ""}.wav"
    write_wav(wav_in, 48_000, 2, 2, 440, 1.5)
    image = tmp / f"audio{smp}.img"
    subprocess.run([sys.executable, str(ROOT / "tools/build_image.py"), "--loader", str(LOADER), "--kernel", str(KERNEL), "--output", str(image),
                    "--bios-stage1", str(ROOT / "build/bios/stage1.bin"), "--bios-stage2", str(ROOT / "build/bios/stage2.bin"),
                    "--file", f"play={TAPES / 'play'}", "--file", f"record={TAPES / 'record'}", "--file", f"a.wav={wav_in}"], check=True, capture_output=True)
    qemu = subprocess.Popen(
        ["qemu-system-x86_64", "-cpu", "max", "-machine", "q35", "-m", "512", "-smp", str(smp), "-display", "none", "-serial", "stdio",
         "-monitor", "none", "-no-reboot", "-audiodev", f"wav,id=snd,path={wav_out}", *(["-device", "qemu-xhci", "-device", "usb-audio,audiodev=snd"] if usb else ["-device", "intel-hda", "-device", "hda-duplex,audiodev=snd"]),
         "-drive", f"if=pflash,format=raw,readonly=on,file={CODE}", "-drive", f"format=raw,file={image},snapshot=on"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    log = b""

    def wait_for(text, timeout, start=0):
        nonlocal log
        end = time.time() + timeout
        while time.time() < end:
            if text.encode() in log[start:]:
                return True
            if select.select([qemu.stdout], [], [], 0.2)[0]:
                log += os.read(qemu.stdout.fileno(), 4096)
        return text.encode() in log[start:]

    def tape(command, expected, timeout=40):
        mark = len(log)
        qemu.stdin.write(f"tape {command}\n".encode())
        qemu.stdin.flush()
        ok = wait_for("exited with code 0.", timeout, mark) and all(wait_for(e, 5, mark) for e in expected)
        return None if ok else f"`tape {command}` did not finish cleanly:\n" + log[mark:].decode("utf8", "replace")

    try:
        if not wait_for("fos> ", 150):
            return "no shell prompt:\n" + log[-800:].decode("utf8", "replace")
        if not wait_for("USB" if usb else "HD Audio", 1):
            return "no audio controller found:\n" + log[-1500:].decode("utf8", "replace")
        problem = tape("play a.wav", ["Playing a.wav: 48000 Hz, 2 channel(s), 16-bit", "Finished"])
        if problem:
            return problem
        problem = tape("play a.wav", ["Finished"])
        if problem:
            return "second play (the first one's stream was not released?): " + problem
    finally:
        qemu.terminate()   # SIGTERM: QEMU finalises the WAV header on a clean exit
        try:
            qemu.wait(10)
        except subprocess.TimeoutExpired:
            qemu.kill()
            qemu.wait()
    if not wav_out.exists() or wav_out.stat().st_size <= 44:
        return "QEMU recorded no audio"
    rate, samples = read_wav(wav_out)
    start = next((i for i, v in enumerate(samples) if abs(v) > 500), None)
    if start is None:
        return "the recording is silent"
    loud = [v for v in samples if abs(v) > 500]
    if len(loud) < rate:
        return f"too little sound recorded ({len(loud)} loud samples)"
    end = len(samples) - next(i for i, v in enumerate(reversed(samples)) if abs(v) > 500)
    frequency = frequency_of(samples[start:min(end, start + rate)], rate)
    if abs(frequency - 440) > 15:
        return f"the recorded tone is {frequency:.0f} Hz, wanted 440"
    return None


failed = False
with tempfile.TemporaryDirectory() as tmp:
    for smp, usb in ((1, False), (2, False), (8, False), (1, True), (2, True), (8, True)):
        problem = run(smp, Path(tmp), usb)
        print(f"-smp {smp} {'USB audio' if usb else 'HD Audio'}: {'ok' if problem is None else 'FAIL'}")
        if problem:
            print(problem)
            failed = True
sys.exit(1 if failed else 0)
