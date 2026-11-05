#!/usr/bin/env python3
"""Fail when a TapeApi call is unsupported on a kernel without that being written down here.

Three kernels fill in `fosfor_abi::tape::TapeApi`: the x86 embedded kernel (`kernel/src/tape.rs`), the Pi 4 embedded
kernel (`kernel/src/tape_aarch64.rs`) and FOS-mf (`kernel-mf/src/tapeapi.rs`, a system-call table). A call a kernel
cannot serve answers "none"/"failed" instead of crashing, so a gap is easy to add and easy to forget. This script reads
the three sources, lists the calls each kernel only stubs, and compares that with GAPS below: a stub that is not listed
fails (document it or implement it), a listed gap that is now served fails too (delete the entry), so the list is
always the truth. `--table` prints the whole matrix.
"""
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ABI = ROOT / "abi" / "src" / "tape.rs"
X86 = ROOT / "kernel" / "src" / "tape.rs"
PI = ROOT / "kernel" / "src" / "tape_aarch64.rs"
COMMON = ROOT / "kernel" / "src" / "tape_common.rs"
MF = ROOT / "kernel-mf" / "src" / "tapeapi.rs"
MF_THREADS = ROOT / "crates" / "kernel-core" / "src" / "tape_threads.rs"

KERNELS = ("x86", "pi4", "mf")

NIC_EXTRAS = ("nic_max_frame_len nic_ts_enable nic_ts_clock nic_ts_rx nic_send_stamped nic_ts_adjust nic_ts_step "
              "nic_ts_all nic_multicast_join nic_vlan_filter nic_tx_offload nic_counters").split()

THREADS = {name: "the embedded kernels run one tape at a time on one CPU; threads and futexes are an mf feature"
           for name in ("thread_spawn", "thread_join", "futex_wait", "futex_wake")}
UNWIRED = "not wired into mf yet (the call answers 0, so a tape sees no such device)"

# Calls a kernel stubs, and why. Field name -> reason, per kernel. Keep it honest: an entry is either a hardware fact
# or a to-do, and says which.
GAPS = {
    "x86": dict(THREADS),
    "pi4": {
        **THREADS,
        **{name: "the Pi 4 NIC driver has no timestamp, offload, VLAN, multicast-hash or counter support yet"
           for name in NIC_EXTRAS},
    },
    "mf": {
        "set_input_mode": "no-op: the mf shell cooks input itself",
        **{name: "the x86 server has no GPIO header or I2C bus (the Pi 4 has); tapes see 'no pins'"
           for name in ("gpio_pins", "gpio_read", "gpio_write", "gpio_mode", "gpio_pull", "gpio_function",
                        "gpio_pull_get", "i2c_speed", "i2c_transfer")},
    },
}


def fields() -> list[str]:
    text = ABI.read_text()
    body = text[text.index("pub struct TapeApi {"):]
    body = body[: body.index("\n}")]
    return re.findall(r"^    pub (\w+):", body, re.M)


def table_values(path: Path) -> dict[str, str]:
    """`field: value,` lines of the TapeApi literal in an embedded kernel."""
    text = path.read_text()
    start = text.index("TapeApi {", text.index("-> TapeApi"))
    body = text[start : text.index("\n    }\n", start)]
    return dict(re.findall(r"^\s+(\w+): ([\w:]+),", body, re.M))


def stub_names() -> set[str]:
    """Functions in tape_common::unsupported, plus the thread calls kernel-core answers with THREAD_UNSUPPORTED."""
    text = COMMON.read_text()
    block = text[text.index("pub mod unsupported") :]
    names = set(re.findall(r"^\s+(api_\w+)\(", block, re.M))
    return names


def embedded_unsupported(path: Path, stubs: set[str]) -> set[str]:
    unsupported = set()
    text = path.read_text()
    for field, value in table_values(path).items():
        leaf = value.split("::")[-1]
        if (leaf in stubs and f"fn {leaf}(" not in text) or "tape_threads::" in value:
            unsupported.add(field)
    return unsupported


DATA_FIELDS = ("version", "size", "args", "args_len", "memory_map", "memory_map_count", "memory_map_descriptor_size")


def mf_unsupported(all_fields: list[str]) -> set[str]:
    """mf answers a call in the `match` of `call`: one arm per slot constant; no arm, or an arm that only returns a fixed
    value (0, the argument, a negative error), is a stub."""
    text = MF.read_text()
    call = text[text.index("pub fn call(") :]
    constants = dict(re.findall(r"^const (\w+): u64 = slot\(offset_of!\(TapeApi, (\w+)\)\);", text, re.M))
    served = set()
    for names, body in re.findall(r"^        ([A-Z0-9_]+(?: \| [A-Z0-9_]+)*)(?: if .+?)? => (.*)$", call, re.M):
        if re.fullmatch(r"(0|a|\(-\d+i64\) as u64),", body):
            continue
        served.update(constants[n] for n in names.split(" | ") if n in constants)
    return {f for f in all_fields if f not in served and f not in DATA_FIELDS}


def support() -> dict[str, set[str]]:
    all_fields = fields()
    stubs = stub_names()
    return {
        "x86": embedded_unsupported(X86, stubs),
        "pi4": embedded_unsupported(PI, stubs),
        "mf": mf_unsupported(all_fields),
    }


def main() -> int:
    all_fields = fields()
    missing = {}
    for kernel, path in (("x86", X86), ("pi4", PI)):
        missing[kernel] = [f for f in all_fields if f not in table_values(path) and f not in DATA_FIELDS]
    actual = support()
    if "--table" in sys.argv:
        print(f"{'call':<24}" + "".join(f"{k:>6}" for k in KERNELS))
        for field in all_fields:
            print(f"{field:<24}" + "".join(f"{'-' if field in actual[k] else 'yes':>6}" for k in KERNELS))
        return 0
    bad = []
    for kernel in KERNELS:
        for field in missing.get(kernel, []):
            bad.append(f"{kernel}: TapeApi.{field} is not in the table the kernel hands to tapes")
        for field in sorted(actual[kernel] - set(GAPS[kernel])):
            bad.append(f"{kernel}: {field} is a stub; implement it or add it to GAPS with a reason")
        for field in sorted(set(GAPS[kernel]) - actual[kernel]):
            bad.append(f"{kernel}: {field} is listed in GAPS but is served now; remove the entry")
    if bad:
        print("\n".join(bad))
        return 1
    print("ABI support: " + ", ".join(f"{k} {len(all_fields) - len(actual[k])}/{len(all_fields)}" for k in KERNELS))
    return 0


if __name__ == "__main__":
    sys.exit(main())
