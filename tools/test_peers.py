#!/usr/bin/env python3
"""peers.py --forget must clear the heard host names too, or two MACs can answer to one name."""

import os
import subprocess
import sys
import tempfile
from pathlib import Path

with tempfile.TemporaryDirectory() as tmp:
    book = Path(tmp) / "peers.json"
    env = dict(os.environ, FOS_PEERS_FILE=str(book))
    sys.path.insert(0, str(Path(__file__).resolve().parent))
    os.environ["FOS_PEERS_FILE"] = str(book)
    import send_tape

    send_tape.remember_peer_names({bytes.fromhex("aca7f1b3085c"): "clock1", bytes.fromhex("7054d290094f"): "clock1"})
    try:
        send_tape.resolve_peer("clock1", [])
        raise SystemExit("FAIL: two machines named clock1 should be ambiguous")
    except SystemExit as error:
        assert "2 machines are called clock1" in str(error), error
    print("PASS: a name heard from two MACs is ambiguous")
    subprocess.run([sys.executable, str(Path(__file__).resolve().parent / "peers.py"), "--forget"], env=env, check=True, capture_output=True)
    assert send_tape.load_peer_names() == {}, "names survived --forget"
    print("PASS: --forget clears the heard names")
    print("PASS: peers")
