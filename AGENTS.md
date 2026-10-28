# Fosfor OS development guide

> **SIZE LIMIT -- read before adding anything.** This file (`CLAUDE.md` is a
> symlink to it) is loaded into every session, and Claude Code **refuses to
> load it above 150,000 bytes**. Keep it far below that: it is a *current-state
> reference*, not a journal. `tools/check_agents_size.py` fails above 125,000.
> New material goes in `docs/`, `NETWORKING.md`, `AUDIO.md`, `DEFERRED.md` or
> `README.md`, with at most a short pointer here. Bug postmortems and "first we
> tried A, then B" narratives belong in `docs/history/`.

## What this is

Fosfor OS (short form **fos**, from Swedish *fosfor*; the name `fos` stays in
`fosfs`, `foscopy`, wire magics such as `FOSMAP` and the GPT GUID `f05b2a01-...`
because those are on-disk and on-the-wire contracts) is a small educational
operating system with a recorder/audio-lab and network-lab bent: a 64-bit
`no_std` Rust kernel, ELF "tapes" loaded and run from the shell, a flat custom
filesystem (FOSFS) with FAT32/exFAT for exchanging files with other systems.

Keep changes small and explainable; avoid frameworks and abstractions that obscure
hardware behaviour. Boot stages and kernel subsystems **narrate** what they do and
why (which firmware call, what a PIC/APIC setup accomplishes), not just "done".

**Editions.** This tree is the only line under development: one 64-bit kernel that boots
through **UEFI** (`boot/uefi/`) or **BIOS** (`boot/bios/`); only POST/boot differs, and both
hand the kernel the same `BootInfo`. The previous 32-bit BIOS edition is frozen at tag
`legacy-v1` / branch `legacy` (Pentium 4 machines, the protected-mode teaching tree); its old
guide is `docs/legacy-v1-guide.md`. Do not backport features to it without being asked. The
plan and status are in `docs/shared-codebase.md`. The v2 prefix is gone from build outputs
(`build/fosfor/fosfor.img`, crate names `fosfor-*`, `FOSFOR_IMAGE`); `fos` stays in `fosfs`,
`foscopy`, wire magics and the GPT GUID on purpose (section 7 of that doc).

## Source map

| Path | What |
| --- | --- |
| `boot/uefi/` | The UEFI loader (PE/COFF, `x86_64-unknown-uefi`): narrates, runs the A/B slot state machine, loads the kernel ELF, builds `BootInfo`, `ExitBootServices`. |
| `boot/bios/` | The BIOS boot chain, plain GNU as/ld: `stage1.S` (the MBR) and `stage2.S` (real mode -> long mode: A20, E820, ACPI RSDP, GPT, the same A/B slot logic, the kernel read above 1 MiB through unreal mode, a VBE framebuffer, ELF placement, `BootInfo`). Lives in the MBR and the GPT gap at LBA 64, so one disk image boots under both firmwares. |
| `kernel/` | The one 64-bit kernel (`x86_64-fos-kernel` target, no SSE, `panic = abort`). CPU-specific code (boot entry, GDT/IDT, APIC, ACPI, MSI, paging, COM1, CPU power) is in `kernel/src/arch/x86_64/`, re-exported at the crate root (`crate::cpu`...); `docs/aarch64-plan.md`. Shell is `term.rs`; drivers, `fosfs.rs`, `fatvol.rs`, `tape.rs` (the tape ABI implementation), `debug.rs`, `audio.rs` (backend facade over `usb_audio.rs` and `hda.rs`). |
| `abi/` | `no_std` crate shared by loader, kernel and tapes: `BootInfo`, slot metadata + `select_slot`, the `TapeApi` table (`abi/src/tape.rs`), ELF helpers, keymaps. |
| `crates/` | Shared `no_std` libraries with host tests: `fat` (FAT32/exFAT: root directory only, contiguous-run writer, `format_exfat`), `tape-rt` (`config`: `TAPE.cfg` / `@NAME` settings files, `save` key), `tape-panel`, `tape-net`, `tape-ip` (IPv4, DHCP, IGMP over the tape ABI), `audio-wire`, `audio-rec`, `opus-codec` (vendored libopus); for audio tapes with a screen `audio-screen`, `gfx` (`plot`, `scope`, `meter`), `dsp` (`stereo`, `level`), `ltas`, `noise` (`docs/design.md` "Shared pieces"). |
| `tapes/` | One crate per tape (library and binary together) (the shared libraries they use are in `crates/`). `tapes/tape.ld` is the shared linker script, `tapes/.cargo/config.toml` the shared target setup. |
| `tools/` | Image builder (`build_image.py`), host tools (`devloop.py`, `term_net.py`, `send_tape.py`, `send_kernel_update.py`, `foscopy.py`, `peers.py`, `send_frame.py`), and the end-to-end test scripts (`test_*.py`). |
| `targets/` | Rust target specs. `x86_64-fos-tape.json` (soft-float) is what tapes really build with. |
| `docs/` | `shared-codebase.md` (the unification plan and its status), `design.md` (the design and milestone log), `recorder.md`, `nettest.md`, `rust-tapes-for-dummies.md`, `history/` (append-only write-ups). |

Also read on demand: `docs/config.md` (every `settings`, `debug`, `start` and `synckey` option), `NETWORKING.md`, `AUDIO.md`, `DEFERRED.md` (things put aside),
`README.md`.

## Boot, memory, disks

- **Boot**: UEFI: firmware -> `\EFI\BOOT\BOOTX64.EFI`. BIOS: MBR -> stage two (see
  `boot/bios/stage2.S`'s header for the steps and its low-memory map; the kernel's crash-record
  page at `0x5000` must stay untouched). Either way the kernel ELF64 is read by LBA from an A/B
  slot and entered in long mode, identity-mapped, interrupts off; it is a position-independent
  image placed wherever the loader chooses (BIOS: chosen from the E820 map, the lowest 2 MiB-aligned
  usable region with about 10 MiB free above 1 MiB). `BootInfo` (`abi/src/lib.rs`, version 5) is the loader/kernel ABI, passed by
  pointer; validate magic, version and size. Stage two writes it by hand, so a unit test in
  `abi` pins its layout: change both together. On BIOS `runtime_services` is 0 and the memory
  map is E820 converted to UEFI descriptor types (usable RAM below 1 MiB is "loader data").
  Code outside boot must not care which firmware booted it (the ACPI reader, for one, maps
  table pages the map does not describe, because a BIOS may label them reserved).
- **Image** (`tools/build_image.py`): GPT with an ESP (FAT32), a raw payload partition
  (GPT type `f05b2a01-0000-4f05-8000-0000000000a1`: slot metadata block, then kernel
  slot 0 and 1, 8 MiB each) and a FOSFS data partition. Never add host block-device
  writes or automatic device selection; writing an image to a device stays an
  explicit user action.
- **Memory**: one physical allocator over the firmware memory map, with a "below
  4 GiB" option for 32-bit-DMA devices (RTL8139, 3C905, old HDA controllers). RAM is
  identity-mapped; kernel text is read-only/executable, the rest NX; stacks have guard
  pages. MMIO is mapped on demand. Interrupts: local APIC and I/O APIC from ACPI
  tables (tables only, no AML), MSI preferred, legacy INTx through the I/O APIC.
- **Disks**: native drivers only (the kernel never calls BIOS): AHCI, NVMe, USB mass
  storage over xHCI and EHCI (one hub level), brought up during boot. `disk1` is always
  the boot disk (found by the payload partition GUID); other numbers follow discovery
  order and are not stable. FOSFS volumes may carry a label (superblock offset 64);
  `diskN/NAME` and `@LABEL/NAME` name files across volumes. FAT/exFAT volumes are only
  reachable by explicit `diskN`, except that boot-time config (`hostname`, `settings`, `debug`, `start`) falls back to a FAT/exFAT root when no FOSFS volume has the file. Disk-destroying commands (`format`, `install`)
  announce what they would erase and need `... confirm` as the very next command;
  never the boot disk.
  A USB disk that is unplugged fails at once (its EHCI root port says so; no multi-second timeouts) and, when the same stick
  (INQUIRY identity and size) comes back, `usb rescan` gives it its old disk number again. `sector diskN LBA [COUNT]` dumps
  raw sectors read-only; `debug slots` says why the slot metadata cannot be read. The same holds for xHCI disks directly on a root port, for disks behind a hub (the hub's ports are asked, `usb rescan` finds a returning stick) and for the hub itself: an unplugged hub, a USB 3 hub's two halves included, is torn down with its devices and enumerated afresh when it returns (xHCI; an EHCI hub on a root port likewise, checked on the AMD board). A replugged USB keyboard likewise gets a fresh entry: the old one is dropped with its xHCI slot (`usb_hid::release_port`) or EHCI queue head (`make test-usb-kbd-replug`).
- **FOSFS v3** (flat, 64 entries, 64-bit contiguous extents) is byte-compatible
  with the legacy edition. File names: up to 16 of lowercase letters, digits, `.`, `_`,
  `-`; enforced in `kernel/src/fosfs.rs`, the tape file-system calls and
  `tools/build_image.py`, which must be kept in sync.

## Drivers

NICs: e1000 family (82540/82574L/82579/I219/I350), RTL8168, RTL8139, 3C905. Storage:
AHCI, NVMe, xHCI/EHCI + mass storage and hub. Input: PS/2, USB HID (keyboards on xHCI, EHCI and, for the low/full-speed companion ports of AMD/NVIDIA-era boards, OHCI: `kernel/src/ohci.rs`, root ports only, no hubs or disks). Audio: USB audio
(xHCI) and Intel HD Audio, behind `kernel/src/audio.rs`. Not carried over from the
legacy edition (still at `legacy-v1`): UHCI, OHCI disks and hubs, NE2000, AC'97, ES1370, the
parallel port. A machine whose only disk or USB interface is one of those cannot run
this kernel.

Hardware rules that apply to every driver (details in `docs/history/` and the
drivers' own header comments): after a reset, even a register read can hang some
silicon; bounded waits only; every command-register write on the 3C905 waits for
`CmdInProgress`; RTL8168 RDU/FOVW are masked until the ring is drained; the legacy 8259s are
re-initialised to vectors 0x30-0x3F and masked before interrupts are enabled (masking alone lets a real
8259 deliver a spurious IRQ7 as vector 15, which stopped the AMD/nForce board); QEMU accepts
orderings real chips do not, so say in the commit message what only real hardware can
check. Inline documentation is mandatory for hardware-facing code: ABI, register
behaviour, timing assumptions, and why.

## Tape ABI

A tape is a position-independent ELF64 executable whose entry is
`extern "C" fn(api: &TapeApi)`. `abi/src/tape.rs` is the source of truth; rules:

- **Append-only.** Fields are only added at the end of `TapeApi`; a tape checks `version`
  and `size` before using a late field. Version is currently 28 (`gpio_pull_get`: the pull of a pin read back, `tape_rt::gpio_pull_get`, used by `gpiopanel`; `midi_devices`: how many USB-MIDI devices are connected, `tape_rt::midi_devices`; 25: `midi_read`, `midi_send`: USB-MIDI for tapes, `kernel/src/usb_midi.rs`, `tape_rt::midi_event`/`midi_send`; 24: `touch_info`, `touch_read`: USB touch panels, `kernel/src/usb_pointer.rs`, `tape_rt::touch_event`, shell `touch monitor`; 23: `i2c_speed`, `i2c_transfer`: the Pi 4 header I2C bus, BSC1 on GPIO2/3, `tapes/i2cscan`, `docs/aarch64-plan.md` "I2C on the Pi"; 22: `gpio_*`, `DEFERRED.md`; 21: `audio_float`: the active audio stream is 32-bit float, `crates/audio-sample`, `docs/design.md` "Native float audio"; audio-net is hard-float on float libopus 1.6.1, private `bits=32` wire mode; 20: `gfx_screens`, `gfx_*_on`: a screen number for the pixel calls, Pi 4 second HDMI output, `docs/aarch64-plan.md` "Two screens"; 19: `fs_list`: the nth file of a volume, `tape_rt::list_file`, FAT roots on x86 only, free space stays in `disk_info`; 18: `audio_use_output`: playback on a different device than capture, shell `audio output N|same`, `tape_rt::use_audio_output`, ltas `outdev=`; 17: `gfx_*`: `gfx_info/open/close/fill/blit` give a tape the pixel screen, 0x00RRGGBB, the console grid keeps updating unpainted, restored at tape end, `docs/design.md` "Graphics for tapes", `make test-gfx`; 16: `fp_level`: the kernel enables SSE/AVX2 before a tape built for `x86_64-fos-tape-fp` starts, `docs/design.md` "Hard-float tapes"; 15: `nic_counters`: negotiated speed, duplex and the e1000 family's error, missed-packet, PAUSE totals plus tape queue drops, `tape_rt::nic_counters`, used by nettest's remote report; 14: `nic_tx_offload`: the I350 and RTL8168G/H (opts2 `TD1_*` bits from Linux r8169) checksum IPv4 UDP frames sent with checksum 0 (context descriptor + TXSM), `tape_rt::tx_offload`, off at tape end, used by the nettest generator, shell `nic csum`, `tools/test_tx_offload.py`; 13: `nic_vlan_filter`: VLAN id filter (VFTA/RCTL.VFE), on with the first id, off while promiscuous, cleared at tape end, `tape_rt::vlan_filter`, shell `nic vlan`; 12: `nic_multicast_join`: the E1000 family's 4096-bit multicast hash filter, `tape_rt::multicast_group`, cleared when a tape ends, shell `nic mcast`; 11: `nic_ts_all`: the I350 stamps every received frame, `tape_rt::rx_arrival_us(nic)` gives its arrival on the `micros()` clock, used by nettest and audio-net; the 82579 and Skylake I219 have PTP event stamps, clock and steering too, no per-frame stamps; 10: `nic_ts_adjust`, `nic_ts_step`: steer the card's clock, TIMINCA and SYSTIM write; 9: `nic_ts_enable/clock/rx`, `nic_send_stamped`: I350 hardware PTP timestamps, `kernel/src/e1000.rs`; `ptp` uses them, `software` opts out; 8: `console_size`: the console's columns and rows; fterm reports them as its Telnet window and sizes its machine list by them; 7: `serial_*`: COM2-COM4 for the `fterm` tape, `kernel/src/uart.rs`; 6: `wall_clock_set`: `ptp settime` sets the clock and RTC; 5: `peer_info`: the kernel table of announced machine names; `nic_active`: `nic use N` also steers the single-interface network calls of `net-chat`, `ip-chat` and `telnet`).
- **Never add fields to a struct the kernel fills in** (`AudioInfo`, `AudioStats`,
  `DiskInfo`, ...): a tape built earlier passes a smaller struct and the kernel writes
  past it. New information gets a new call and a new struct (`audio_buffer`,
  `audio_devices` are examples).
- **After any ABI change rebuild and redeploy every tape** (`make tapes`, then copy to the
  machines); a running tape also blocks NETFS serving.
- `read_key` and `should_abort` drain the same input: use one or the other.
- Audio tapes (`play`, `record`, `recorder`, `audio-net`) take a leading `device=N` (`tape-rt::take_audio_device_arg`) and show
  the device by name (`tape-rt::AudioDeviceName`).
- Audio: tape calls go to the *active* device (`audio device N|auto`, or a tape's own
  `audio_use_device`, undone when it ends). USB audio wins automatically when present.

## Build and test

Toolchain pinned in `rust-toolchain.toml` (`nightly-2026-09-13`, `rust-src`); GNU Make,
Python 3 (stdlib only for tools), `qemu-system-x86_64` with OVMF
(`/usr/share/OVMF/OVMF_CODE_4M.fd`). All cargo builds share `build/cargo-target`.

```sh
make image            # build/fosfor/fosfor.img: loader + kernel + every tape (default goal)
make all              # both images (x86 and build/fosfor-aarch64/fosfor-pi4.img); tools/check_build_tools.py names what is missing first
make run              # boot it in QEMU + OVMF, serial on the terminal
make test             # host unit tests (abi, crates, the tape libraries via `cargo test --lib`) and the boot/framebuffer test
make test-NAME        # one end-to-end test; `make help`, or the .PHONY list in the Makefile
make test-bios        # 16 of them, booted through BIOS (SeaBIOS), plus kernel placement and the skip-video override
make test-all         # every test-* target (slow, ~30 min); prints PASS/FAIL per target
make test-all TEST_JOBS=8   # the same in parallel (~9 min on a 16-core xeon): tools/run_tests.py, see docs/design.md
```

The BIOS runs reuse the UEFI tests through `tools/qemu_bios_shim.py`, which drops the OVMF drives
from the test's QEMU command line (`FOS_FIRMWARE=bios`); a check that greps loader wording must
use the shared text or `LOADER_BANNER`/`BIOS` from `tools/test_boot.py`. Run `make test` for any kernel or loader change and the matching `make test-NAME` for
the area you touched (`test-debug-mode` for crash/KUP/watchdog, `test-hda` and
`test-usb-audio` for audio, `test-nic` and `test-rtl8139` for NICs, `test-recorder*`
for the recorder, ...). `test-nettapes` (bridge and ip-chat through the
`qemu-i386` shim) passes again. `test-audio-net` and `test-nettest`
have been seen to flake once. A test with a fixed host port wraps it in `testslot.port()`, and a test that boots the image reads
`FOSFOR_IMAGE` (parallel workers each get their own copy; QEMU refuses two writers). `test-bootcode-uefi` and `test-nettapes` have
failed under load and pass alone. The landsbro buildbot (`gb -of fos`, config `fos.yaml`) runs the parallel suite. ARP/DHCP frame
building is shared in `crates/tape-net/src/frames.rs`: do not copy it into a tape. Tests use bounded waits, keep the serial output on
failure and wait for the prompt before typing. **Every new tape needs a focused test
through its documented load/run path**; a green build is not validation. Editing a path
(moving a directory) needs a full `make test-all`: split spellings such as
`ROOT / "kernel"` and `../../..` build-script paths hide from text search.

## Debug mode, KUP, real hardware

**Flashing a stick** (a machine with no working NIC yet cannot take KUP): the user runs
`sudo python3 tools/write_stick.py /dev/sdX` (image defaults to `build/fosfor/fosfor.img`). It refuses mounted, non-removable,
too-small or over-256-GiB devices and makes the user type the device name back; it never picks a device. Never run it
yourself or suggest bare `dd` first; give the user the command.

Per-machine config files on FOSFS: `start` (boot script; the `start` command reruns it), `hostname`, `settings`
(`no-msi`, `deny-term-net`, `keymap sv` (Swedish/Finnish local keyboard, default US; docs/design.md), `serial-shell [PORT] [BAUD]`: the shell also on a serial port, no password, see docs/design.md), `debug` (`reboot-on-crash`, `reboot-delay N`,
`kernel-update`, `watchdog [N]`, `crash-on-boot`; `tools/default-debug.txt` is installed
by default). A fatal exception writes a crash record (read and printed by the next
boot) and, with `reboot-on-crash`, resets; five pre-milestone crashes in a row stop the
loop. The loader runs the A/B slot state machine: a pushed kernel gets one try, and
falls back automatically. **KUP** (`tools/send_kernel_update.py`) pushes a kernel over
Ethernet into the inactive slot; the wire formats (KUP, NETFS, `term net`, chat) are
shared with the legacy edition so the host tools work against both.

The BIOS boot code (stage one in the MBR, stage two at LBA 64) is not in a kernel slot, so KUP cannot update it. `bootcode`
shows what a disk has; with `stage1.bin` and `stage2.bin` (from `build/bios/`) on the boot volume (`foscopy.py FILE
:disk1/stage2.bin`), `bootcode update` announces and `bootcode update confirm` writes stage two, then stage one's code, reading
both back (fos disks only). The same command replaces the UEFI loader (`\EFI\BOOT\BOOTX64.EFI` on the ESP) when `loader.efi` (`fosfor-loader.efi`)
is on the boot volume: written beside the old one as `BOOTX64.NEW`, read back, then switched in with one directory-sector write
(`fat::Fs::replace`), so a power cut leaves the old loader or the new one. `make test-bios-bootcode` and `make test-bootcode-uefi` run it for real.

BIOS boots only: a video BIOS that hangs in the VBE mode switch can be skipped once by holding Shift during the
boot, or for good with `debug video skip` (a flag in the slot-metadata sector, byte 16; `debug video auto` undoes it). `videomodes` lists the modes stage two saw and the EDID result; `debug video size WxH` gives it a monitor size when the card has no EDID.
The kernel then writes to the BIOS's 80x25 VGA text screen (`framebuffer::init_text`) and the serial port.

Talking to a real machine from a sandbox needs `AF_PACKET` raw sockets; a human grants
the capability once to a *dedicated copy* of Python (an agent must never do this
silently):

```sh
cp "$(command -v python3)" ~/.local/bin/python3-netraw
sudo setcap cap_net_raw+eip ~/.local/bin/python3-netraw   # verify: getcap
~/.local/bin/python3-netraw tools/devloop.py --iface IF --mac MAC run "cmd"
```

Always pass `--iface` explicitly (the default may be a VPN). Prefer `tools/devloop.py`
(`wait`, `run`, `push KERNEL`, `put FILE NAME`, `tape-into`) over ad-hoc scripts: it
encodes the traps that gave false results. Deploy path: `devloop push KERNEL`, then
`foscopy.py --mac N FILE :disk1/NAME`. Test machines and their quirks are in the
maintainer's memory notes, not in this repository.

## Things a new session should know (2026-10-09)

- **Tests added for hardware-shaped behaviour**: `test-usb-kbd-replug`, `test-usb-replug` (disks, hubs, the hub itself),
  `test-xhci-abort`, `test-release-loops`, `test-idle-cpu`, `test-net-chat`, `test-telnet`, `test-nettest-lab`, `test-bootcode-uefi`, `test-console-utf8`, `test-console-colors`, `test-hostnames`, `test-config-fat`.
  `make test-all` runs every `test-*` target (70 of them; none is a known failure).
- **`term net` data frames carry no length**: the kernel drops trailing zero bytes (Ethernet padding) before a tape or the shell sees
  the frame; a NUL typed on purpose is lost. A tape's single-interface network calls follow `nic use N` (ABI 4, `nic_active`).
- **Hubs**: a hub's ports are only visible by asking the hub, so `usb rescan` is what notices a device that went away behind one;
  the storage driver asks the hub before every transfer (`child_vanished`, `hub_child_vanished`) so a pulled disk fails at once. A
  hub's first class request after SET_CONFIGURATION may stall once (a Realtek 0bda:0401 does): `usb_hub::enumerate` waits and retries.
- **Real-hardware workflow**: `tools/devloop.py` as described above; a pushed kernel that comes back on the *other* slot with
  "no FOSFS data partition" in `dmesg` means the A/B fallback reverted it. `bootcode` can replace the BIOS boot code and the UEFI
  loader without a stick rewrite (both verified on real machines). `debug video skip` plus `reboot` shows the VGA text console.
- **Tape sync**: `tapesync [HOST] [apply]` pulls newer tapes (from HOST or every peer) between machines that share a `synckey` file; tapes carry a `.fos_build` stamp added by `tools/stamp_tape.py`, so deploy `build/fosfor/tapes/NAME`, not the cargo output (`docs/design.md`, `make test-tapesync`).
- **`audio-net bench`** (`benchserve` on the far machine; panel key T) finds the lowest ptime/target a link carries: `crates/audio-bench`, `tapes/audio-net/src/bench.rs`, `make test-audio-bench`; method in `docs/design.md`.
- **Open work** is in `DEFERRED.md` ("Open after the 2026-10-09 session").

- **Hostnames**: every machine broadcasts its name (`hostname` file + NIC number, else `fos-` + last 3 MAC bytes) at boot and every
  10 s (`term.rs` `announce_service`, also while a tape runs); `handle_frame` files them in the peer table (`nic peers`, ABI `peer_info`).
  `audio-net` uses that name as its session name and resolves `proto=raw|avtp to=NAME` from the table; IP `to=NAME` still needs SAP.
- **Console text is UTF-8** (`framebuffer.rs`: Latin-9 glyphs, CP437 in BIOS text mode; `tape.rs` `api_putc` assembles sequences). Input
  and file names are still ASCII.

## Conventions

- A tape with arguments carries its usage as `tape_rt::tape_help!(br#"..."#);` (lines of at most 79 columns): the text
  goes into the `.fos_help` section of its ELF, and the kernel prints it for `help NAME`, `NAME help` and `NAME --help` without
  running the tape (`tape.rs` `show_help`, `abi::elf::section`). Tapes done so far: ptp, audio-net, record, recorder, play, router,
  bridge, nettest, ip-chat, telnet, net-chat, memtest, fterm. A new tape gets one; `ed`, `calc` and `hello` have none yet.
- Every shell command's output fits an 80-column text grid; keep each print line to at
  most 79 columns and split long messages rather than relying on wrapping.
- Lower-case names throughout (FOSFS entries, files, tape names).
- Docs that move with code: README's tape table, `docs/design.md` (a
  paragraph per feature, milestone status), this file's pointers.
- **Never print between `audio_start` and the send loop, or inside it.** A console line costs ~10 ms on real hardware; the capture
  backlog then goes out as a burst (underruns, discarded frames, a bogus clock offset). Drain it (`audio_read` until 0) right
  before the loop, after the last print, and collect results to print at the end (`LatencyProbe::summary`). Fixed in audio-net
  raw/AVTP/RTP, Opus send and `serve`.
- Commit messages say what was verified and what only real hardware can verify. Do not
  commit or push unless asked.
