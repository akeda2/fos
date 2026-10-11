"""Port numbers for parallel test runs.

Tests use fixed host ports (QEMU `socket,mcast=` segments, host-forwarded and listening TCP ports). `tools/run_tests.py`
runs several tests at once and gives each worker its own `FOS_TEST_SLOT`; `port()` moves a test's ports by 1000 per slot
so two tests never share a segment. Without the variable (a single `make test-NAME`) nothing moves."""
import os

SLOT = int(os.environ.get("FOS_TEST_SLOT", "0"))


def port(number):
    return number + SLOT * 1000
