---
name: fosadm
description: Deploys and inspects Fosfor OS (fos) machines over Ethernet or a console serial line with tools/fosadm.py - discover hosts, compare kernel and tapes against the build, push the kernel (KUP) and tapes, run commands, copy files. Use when asked to deploy, push the kernel or tapes, update or check what the machines run, or add a tape to a host.
---

# fosadm

Run from the repo root (`/home/akeda/dev/fos`). Only act when the user asks to deploy/push.
Never `bootcode --update` or write boot code without an explicit yes. Never grant CAP_NET_RAW yourself.

## Setup
- Build first: `make image` (kernel and all tapes; `make image-pi4` for aarch64). Do not trust file dates.
- `tools/fosadm.py` re-executes itself under `~/.local/bin/python3-netraw` when raw sockets are missing.
- Always pass `--iface IFACE`, the wired NIC on the test network (the default may be a VPN; `ip -br link` lists them).
  On the maintainer's workstation it is `enp0s31f6`; ask or check on any other machine.
- Peer names are hostname + NIC number (skylake3 = NIC 3 of skylake); fosadm groups NICs per host, so deploy once per host.

## Commands
```
python3 tools/fosadm.py --iface IFACE hosts                 # discover (forgets stale peers first)
python3 tools/fosadm.py --iface IFACE status [-v] [--sizes] [HOST...] # kernel slot, tapes vs build (SHA-256; --sizes: sizes only)
python3 tools/fosadm.py --iface IFACE deploy [HOST...]      # plan, then kernel (KUP) + changed tapes
```
`deploy` options: `--dry-run` (plan only), `-y` (no prompt), `--tapes-only`, `--kernel-only`,
`--sizes` (plan by size, not SHA-256; deploy compares content by default), `--force-kernel` (size matches but push anyway), `--add-missing`, `--from DIR` (a release folder).
Also: `run HOST cmd...`, `put HOST FILE [NAME] [--disk N]`, `shell HOST`, `bootcode HOST [--update]`.

Order: `status` (or `deploy --dry-run`) first, show the plan to the user, then deploy.

## Serial deploy (one machine on its console line)
```
python3 tools/fosadm.py --serial PORT [--fast 921600] status [-v]
python3 tools/fosadm.py --serial PORT [--fast 921600] deploy [--dry-run|-y|--tapes-only|--kernel-only|--force-kernel|--add-missing]
python3 tools/fosadm.py --serial PORT run serial "ls"            # HOST argument is ignored
python3 tools/fosadm.py --serial PORT [--fast 921600] put serial FILE [NAME]
```
- No `--iface`, no raw-socket rights. The kernel goes through `kup serial`, tapes/files through `recv` (x86 and Pi 4 shells).
- The shell must be at its prompt; a running tape is stopped with ESC like over Ethernet. `shell` and `bootcode` need Ethernet.
- Ports: `ls -l /dev/serial/by-id /dev/ttyUSB*`; the user must be in `dialout`. A workstation may have several adapters
  (an LTE modem shows up as ttyUSB2-4); probe with `status` and ask the user when unsure which is the target.
- Speed: ~11 KB/s at 115200 baud (a 1 MB tape is about 90 s); `--fast 921600` is ~4x faster but the **Pi 4 shell only**
  (x86 COM1 keeps its speed). Measured on a Pi 4: 200 KB in 18 s normal, 4.3 s fast; forced kernel deploy 40 s.
- `status` compares the kernel by size in sectors, so a changed kernel of the same sector count reads "current":
  use `--force-kernel` to push anyway. The Pi comes back on the other slot (slot 0 <-> 1) after a deploy.
- A Pi 4 SD card once refused the first write and then failed all reads until `reset` (not reproduced; the kernel now
  prints `SD: error: ...` on the first SD error). If a put is refused with "could not be written", look at the console
  for that line, then `reset` and retry.
- `make test-fosadm-serial` tests it against QEMU (COM1 on a pty).

## Behaviour to know
- **Serial instead of Ethernet:** see "Serial deploy" above; `--serial` replaces discovery with the one machine on that port.
- Tapes are compared by **size** by default; stored tapes are byte-identical to the build, so status and deploy compare SHA-256 by default (`--sizes` opts out) (Ethernet: fetched; serial: the kernel's `sha256` command, once the kernel has it) and a deploy verifies what it copied that way. Say what was verified.
- By default only tapes a host **already has** are updated. A host lacking a tape says "up to date";
  use `--add-missing` to install new ones (it installs every missing tape, so check `status -v` first;
  for a single tape use `put HOST build/fosfor/tapes/NAME NAME`).
- Per-machine files (`fosplay.cfg`, `start`, `settings`, `hostname`, `debug`) are never pushed.
- aarch64 hosts (Pi 4) get `build/fosfor-aarch64/tapes` and their own kernel automatically.
- After a tape ABI change redeploy every tape.

## Traps
- A running tape blocks the shell, foscopy and delays KUP's reboot. If a host is stuck, send a lone ESC through term net:
  `(printf '\x1b'; sleep 2) | python3-netraw tools/term_net.py --iface IFACE --mac N --interactive`
- amd1 runs `ptp master` from its `start` file; after the KUP reboot it starts again. A service
  (`run ... &N`) does not block KUP or copies.
- A pushed kernel that returns on the other slot with "no FOSFS data partition" in `dmesg` was reverted
  by A/B fallback (`debug slots` shows the slot sizes).
- Wrong or duplicate host names: run `hosts` again (it forgets stale peers).
- Manual fallback if fosadm cannot do it: `tools/devloop.py --iface IFACE --mac N push KERNEL`,
  `tools/foscopy.py --iface IFACE --mac N build/fosfor/tapes/NAME :disk1/NAME`; verify with `devloop.py ... run "ls"`.

## Report
State what was verified (sizes, slots) and what only hardware shows. Do not commit unless asked.
