# Fosfor OS

Fosfor OS (short form **fos**, from the Swedish *fosfor*, phosphorus) is a small educational
operating system for x86-64 PCs, with a recorder/audio-lab and network-lab bent. It is one
64-bit `no_std` Rust kernel that boots two ways, through **UEFI** or through the **BIOS**, loads
programs ("tapes") from a flat custom filesystem, and can be driven over plain Ethernet from
another machine. It is written to be read: boot stages and drivers narrate what they do and
why, and the code avoids frameworks that would hide how the hardware behaves.

The previous 32-bit BIOS edition is frozen at the git tag `legacy-v1` (branch `legacy`); its
README is kept in [`docs/legacy-v1-readme.md`](docs/legacy-v1-readme.md).

## Try it

Needs GNU Make, Python 3, `qemu-system-x86_64` with OVMF, and the pinned Rust nightly
(`rust-toolchain.toml`, with `rust-src`).

```sh
make image      # build/fosfor/fosfor.img: loader + kernel + every tape
make run        # boot it in QEMU (UEFI), serial on the terminal
make test       # unit tests and the boot test
make test-bios  # the same tests booted through BIOS (SeaBIOS)
make help       # every target
```

At the `fos>` prompt, `help` lists the commands. Try `disks`, `ls`, `cat readme.txt`,
`nic`, `usb`, `tape run hello`, `calc 3 4 +`. A tape with a help screen shows it for
`help NAME`, `NAME help` or `NAME --help` (`help ptp`), without running it.

To run on a real machine write the image to a stick yourself, with
`sudo python3 tools/write_stick.py /dev/sdX`. The tools never pick or write a block device on
their own.

## What is in it

| | |
| --- | --- |
| Boot | A UEFI loader (`boot/uefi/`) or BIOS boot code (`boot/bios/`: an MBR, then a stage two in the gap before the first partition that reaches long mode, reads the kernel, chooses a VBE video mode from the monitor's EDID and builds the same `BootInfo`). Both run an A/B kernel-slot state machine: a pushed kernel gets one try and falls back to the previous one. |
| Kernel | 64-bit, identity-mapped, local APIC and I/O APIC from the ACPI tables, MSI preferred. Framebuffer text console, PS/2 and USB keyboards, a shell, a crash recorder, an optional watchdog. |
| Disks | AHCI, NVMe, USB mass storage over xHCI and EHCI (including through hubs, with replug recovery). **FOSFS** (flat, 64 files, byte-compatible with the legacy edition) plus FAT32 and exFAT for exchanging files with PCs. |
| Network | e1000, e1000e, I350, RTL8168, RTL8139 and 3C905 NICs. A raw-Ethernet protocol set (chat, discovery, a network file system, a remote terminal, kernel update); IPv4 and above live in the tapes. |
| Audio | USB audio (xHCI) and Intel HD Audio behind one interface. |
| Tapes | Programs loaded from disk or over the network into one 4 MiB region and run through a versioned, append-only table of kernel calls (`abi/src/tape.rs`). |

### Tapes

| Tape | What it does |
| --- | --- |
| `hello`, `calc`, `memtest` | The smallest example; an RPN calculator; a RAM tester and soak test. |
| `ed` | A line editor with cursor keys; saves to FOSFS, FAT32 or exFAT. |
| `net-chat`, `ip-chat`, `telnet` | Raw-Ethernet chat, IPv4/UDP chat, a Telnet client. |
| `ptp` | PTPv2 (IEEE 1588, the clock AES67 uses) over UDP, or over Ethernet with `l2` (ethertype 0x88F7, no IP; `ptp4l -2`): `tape run ptp [master] [l2]`. As a slave it measures this machine's clock offset, path delay and rate against a master. `master ntp=dhcp\|A.B.C.D` first asks an NTP server the time (DHCP option 42, else the gateway) so the master's time is real; `settime tz=+2` on a slave then sets that machine's clock and RTC from it and ends (`date` shows it); the NIC is the one chosen with `nic use N`. |
| `router`, `bridge`, `nettest` | A layer-3 router (NAT, DHCP, health-checked uplinks), a layer-2 learning bridge, a link-impairment and traffic generator. |
| `play`, `record`, `recorder`, `audio-net` | WAV playback and recording, a field recorder, audio streaming between hosts (raw Ethernet or RTP, Opus). RTP follows a PTP clock when one is found (`ptp=auto\|l2\|off`; raw and AVTP use Ethernet PTP, `ptp l2`) and a receiver shows the one-way delay. `priority=wifi\|safe\|balanced\|low\|live\|auto` picks ring and packet time (40 ms/2 ms ... 3 ms/0.125 ms). |

Reference documents: [`NETWORKING.md`](NETWORKING.md), [`AUDIO.md`](AUDIO.md),
[`docs/recorder.md`](docs/recorder.md), [`docs/nettest.md`](docs/nettest.md),
[`docs/rust-tapes-for-dummies.md`](docs/rust-tapes-for-dummies.md) (write your own tape).

## Driving a machine from another

A running fos machine answers a small Ethernet protocol, so a PC on the same segment can use it
without a keyboard or screen on it (the tools need raw-socket rights; see `AGENTS.md`):

```sh
tools/devloop.py --iface IF --mac MAC run "disks"      # run a shell command
tools/devloop.py --iface IF --mac MAC push KERNEL      # push a kernel; it falls back if it does not boot
tools/foscopy.py --iface IF --mac MAC FILE :disk1/NAME # copy a file onto its disk
tools/term_net.py --iface IF --mac MAC                 # an interactive terminal
```

`debug` files on the machine's disk (see `AGENTS.md`) turn on reboot-on-crash, the watchdog and
the network kernel update.

## Where things are

| Path | What |
| --- | --- |
| `boot/uefi/`, `boot/bios/` | The two boot paths. |
| `kernel/` | The kernel (`term.rs` is the shell). |
| `abi/` | What loader, kernel and tapes share: `BootInfo`, slot metadata, the tape call table. |
| `crates/` | Shared libraries with host tests: `fat`, `tape-rt`, `tape-net`, `tape-panel`, `audio-wire`, `audio-rec`, `opus-codec`. |
| `tapes/` | One crate per tape. |
| `tools/` | Image builder, host tools and the end-to-end test scripts. |
| `docs/` | `design.md` (design and milestone log), `shared-codebase.md` (how the two boot paths share one tree), `history/`. |

`AGENTS.md` is the current-state development guide (build and test, hardware rules, the tape
ABI, debug mode); `DEFERRED.md` lists what was put aside and why.

## Hardware it has run on

QEMU (UEFI and BIOS) covers most of it. Real machines so far: an AMD Athlon 64 on an nForce
board (BIOS boot, EHCI, RTL8168 and 3C905 NICs, an NVIDIA card), a Skylake desktop (UEFI, xHCI,
I350 and RTL8168 NICs), a Sandy Bridge board (BIOS-era EHCI behind a rate-matching hub, NEC
xHCI), and a Dell Latitude E7270 laptop (UEFI, I219-LM).

## Licence

See `LICENSE`. The vendored libopus under `crates/opus-codec/vendor/opus` keeps its own BSD
licence (`COPYING` there).
