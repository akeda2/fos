#!/usr/bin/env python3
"""Runs `make test-NAME` targets in parallel: `run_tests.py JOBS TARGET...` (`make test-all TEST_JOBS=N` calls it).

Each worker has a slot (`FOS_TEST_SLOT`, see tools/testslot.py) that moves the test's fixed host ports, so tests do not
share a QEMU segment. A test's output goes to build/test-logs/TARGET.log. A failed run's log is kept as TARGET.failed.log (TARGET.alone.failed.log for the retry) and its last lines are
printed, so a build log shows why. A target that fails is run once more, alone, at
the end: a test that only fails while the machine is busy is reported as such ("PASS (alone)") instead of being hidden.
Targets that took longest last time start first, so the run does not end on one long test."""
import json
import os
import queue
import shutil
import subprocess
import sys
import threading
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
LOGS = ROOT / "build" / "test-logs"
TIMES = LOGS / "times.json"


def slot_image(slot):
    """A private copy of the image per slot: QEMU refuses a second writer on one image file."""
    source = ROOT / "build" / "fosfor" / "fosfor.img"
    copy = ROOT / "build" / "test-slots" / str(slot) / "fosfor.img"
    if not copy.exists() or copy.stat().st_mtime < source.stat().st_mtime:
        copy.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source, copy)
    return copy


def run(target, slot, kind="failed"):
    environment = dict(os.environ, FOS_TEST_SLOT=str(slot), FOSFOR_IMAGE=str(slot_image(slot)))
    started = time.time()
    with open(LOGS / f"{target}.log", "w") as log:
        # -o image: test-all has built the image once; parallel makes must not rebuild it under each other
        code = subprocess.call(["make", "--no-print-directory", "-o", "image", target], cwd=ROOT, env=environment, stdout=log, stderr=subprocess.STDOUT)
    if code != 0:
        # The next run of this target overwrites TARGET.log, and a retry that passes would hide why the first try failed.
        keep = LOGS / f"{target}.{kind}.log"
        shutil.copyfile(LOGS / f"{target}.log", keep)
        lines = [line for line in keep.read_text(errors="replace").splitlines() if not line.startswith("PASS")]
        print(f"--- {target}: last lines that are not PASS (whole log: {keep.relative_to(ROOT)})", *("    " + line[:200] for line in lines[-12:]), sep="\n", flush=True)
    return code == 0, time.time() - started


def main():
    jobs, targets = int(sys.argv[1]), sys.argv[2:]
    LOGS.mkdir(parents=True, exist_ok=True)
    times = json.loads(TIMES.read_text()) if TIMES.exists() else {}
    targets.sort(key=lambda target: -times.get(target, 0))
    work = queue.Queue()
    for target in targets:
        work.put(target)
    results, lock, begin = {}, threading.Lock(), time.time()

    def worker(slot):
        while True:
            try:
                target = work.get_nowait()
            except queue.Empty:
                return
            ok, seconds = run(target, slot)
            with lock:
                results[target] = ok
                times[target] = seconds
                print(f"{'PASS' if ok else 'FAIL'} {target} ({seconds:.0f}s, slot {slot}, {len(results)}/{len(targets)})", flush=True)

    threads = [threading.Thread(target=worker, args=(slot,)) for slot in range(min(jobs, len(targets)))]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    failed = [target for target in targets if not results[target]]
    for target in failed:
        if os.environ.get("FOS_NO_RETRY"):
            break
        ok, seconds = run(target, 0, "alone.failed")
        print(f"{'PASS (alone)' if ok else 'FAIL (alone)'} {target} ({seconds:.0f}s)", flush=True)
        results[target] = ok
    TIMES.write_text(json.dumps(times))
    still = [target for target in targets if not results[target]]
    print(f"{len(targets)} tests in {time.time() - begin:.0f}s with {jobs} workers; logs in build/test-logs/")
    if still:
        print("FAILED:", " ".join(still))
        return 1
    print("all tests passed" + (f" ({len(failed)} only when run alone: {' '.join(failed)})" if failed else ""))
    return 0


sys.exit(main())
