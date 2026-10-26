---
name: fosadm
description: Deploys and inspects Fosfor OS (fos) machines over Ethernet with tools/fosadm.py - discover hosts, compare kernel and tapes against the build, push the kernel (KUP) and tapes, run commands, copy files. Use when asked to deploy, push the kernel or tapes, update or check what the machines run, or add a tape to a host.
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
python3 tools/fosadm.py --iface IFACE status [-v] [HOST...] # kernel slot, tape sizes vs build; -v lists missing tapes
python3 tools/fosadm.py --iface IFACE deploy [HOST...]      # plan, then kernel (KUP) + changed tapes
```
`deploy` options: `--dry-run` (plan only), `-y` (no prompt), `--tapes-only`, `--kernel-only`,
`--force-kernel` (size matches but push anyway), `--add-missing`, `--from DIR` (a release folder).
Also: `run HOST cmd...`, `put HOST FILE [NAME] [--disk N]`, `shell HOST`, `bootcode HOST [--update]`.

Order: `status` (or `deploy --dry-run`) first, show the plan to the user, then deploy.

## Behaviour to know
- Tapes are compared by **size** (stored tapes are staged images, bytes differ). Say what was verified.
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
