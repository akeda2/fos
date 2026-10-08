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
plan and status are in `docs/shared-codebase.md`. Some names still say `v2` (`build/v2/`,
crate names `fos-v2-*`, `FOS_V2_IMAGE`); renaming them to Fosfor is a separate mechanical step
(that doc, section 7).

## Source map

| Path | What |
| --- | --- |
| `boot/uefi/` | The UEFI loader (PE/COFF, `x86_64-unknown-uefi`): narrates, runs the A/B slot state machine, loads the kernel ELF, builds `BootInfo`, `ExitBootServices`. |
| `boot/bios/` | The BIOS boot chain, plain GNU as/ld: `stage1.S` (the MBR) and `stage2.S` (real mode -> long mode: A20, E820, ACPI RSDP, GPT, the same A/B slot logic, the kernel read above 1 MiB through unreal mode, a VBE framebuffer, ELF placement, `BootInfo`). Lives in the MBR and the GPT gap at LBA 64, so one disk image boots under both firmwares. |
| `kernel/` | The one 64-bit kernel (`x86_64-fos-kernel` target, no SSE, `panic = abort`). Shell is `term.rs`; drivers, `fosfs.rs`, `fatvol.rs`, `tape.rs` (the tape ABI implementation), `debug.rs`, `audio.rs` (backend facade over `usb_audio.rs` and `hda.rs`). |
| `abi/` | `no_std` crate shared by loader, kernel and tapes: `BootInfo`, slot metadata + `select_slot`, the `TapeApi` table (`abi/src/tape.rs`), ELF helpers, keymaps. |
| `fat/` | FAT32/exFAT crate with host tests (root directory only, contiguous-run writer, `format_exfat`). |
| `tapes/` | One crate per tape (library and binary together) plus shared crates `tape-rt`, `tape-panel`, `tape-net`, `audio-wire`, `audio-rec`, `opus-codec` (vendored libopus). `tapes/tape.ld` is the shared linker script, `tapes/.cargo/config.toml` the shared target setup. |
| `tools/` | Image builder (`build_image.py`), host tools (`devloop.py`, `term_net.py`, `send_tape.py`, `send_kernel_update.py`, `foscopy.py`, `peers.py`, `send_frame.py`), and the end-to-end test scripts (`test_*.py`). |
| `targets/` | Rust target specs. `x86_64-fos-tape.json` (soft-float) is what tapes really build with. |
| `docs/` | `shared-codebase.md` (the unification plan and its status), `v2-design.md` (the design and milestone log), `recorder.md`, `nettest.md`, `rust-tapes-for-dummies.md`, `history/` (append-only write-ups). |

Also read on demand: `NETWORKING.md`, `AUDIO.md`, `DEFERRED.md` (things put aside),
`README.md`.

## Boot, memory, disks

- **Boot**: UEFI: firmware -> `\EFI\BOOT\BOOTX64.EFI`. BIOS: MBR -> stage two (see
  `boot/bios/stage2.S`'s header for the steps and its low-memory map; the kernel's crash-record
  page at `0x5000` must stay untouched). Either way the kernel ELF64 is read by LBA from an A/B
  slot and entered in long mode, identity-mapped, interrupts off; it is a position-independent
  image placed wherever the loader chooses (BIOS: 4 MiB, staging at 16 MiB, so about 24 MiB of
  RAM is needed). `BootInfo` (`abi/src/lib.rs`, version 5) is the loader/kernel ABI, passed by
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
  reachable by explicit `diskN`. Disk-destroying commands (`format`, `install`)
  announce what they would erase and need `... confirm` as the very next command;
  never the boot disk.
- **FOSFS v3** (flat, 64 entries, 64-bit contiguous extents) is byte-compatible
  with the legacy edition. File names: up to 16 of lowercase letters, digits, `.`, `_`,
  `-`; enforced in `kernel/src/fosfs.rs`, the tape file-system calls and
  `tools/build_image.py`, which must be kept in sync.

## Drivers

NICs: e1000 family (82540/82574L/82579/I219/I350), RTL8168, RTL8139, 3C905. Storage:
AHCI, NVMe, xHCI/EHCI + mass storage and hub. Input: PS/2, USB HID. Audio: USB audio
(xHCI) and Intel HD Audio, behind `kernel/src/audio.rs`. Not carried over from the
legacy edition (still at `legacy-v1`): UHCI/OHCI USB, NE2000, AC'97, ES1370, the
parallel port. A machine whose only disk or USB interface is one of those cannot run
this kernel.

Hardware rules that apply to every driver (details in `docs/history/` and the
drivers' own header comments): after a reset, even a register read can hang some
silicon; bounded waits only; every command-register write on the 3C905 waits for
`CmdInProgress`; RTL8168 RDU/FOVW are masked until the ring is drained; QEMU accepts
orderings real chips do not, so say in the commit message what only real hardware can
check. Inline documentation is mandatory for hardware-facing code: ABI, register
behaviour, timing assumptions, and why.

## Tape ABI

A tape is a position-independent ELF64 executable whose entry is
`extern "C" fn(api: &TapeApi)`. `abi/src/tape.rs` is the source of truth; rules:

- **Append-only.** Fields are only added at the end of `TapeApi`; a tape checks `version`
  and `size` before using a late field. Version is currently 3.
- **Never add fields to a struct the kernel fills in** (`AudioInfo`, `AudioStats`,
  `DiskInfo`, ...): a tape built earlier passes a smaller struct and the kernel writes
  past it. New information gets a new call and a new struct (`audio_buffer`,
  `audio_devices` are examples).
- **After any ABI change rebuild and redeploy every tape** (`make tapes`, then copy to the
  machines); a running tape also blocks NETFS serving.
- `read_key` and `should_abort` drain the same input: use one or the other.
- Audio: tape calls go to the *active* device (`audio device N|auto`, or a tape's own
  `audio_use_device`, undone when it ends). USB audio wins automatically when present.

## Build and test

Toolchain pinned in `rust-toolchain.toml` (`nightly-2026-09-13`, `rust-src`); GNU Make,
Python 3 (stdlib only for tools), `qemu-system-x86_64` with OVMF
(`/usr/share/OVMF/OVMF_CODE_4M.fd`). All cargo builds share `build/cargo-target`.

```sh
make image            # build/v2/v2.img: loader + kernel + every tape (default goal)
make run              # boot it in QEMU + OVMF, serial on the terminal
make test             # unit tests (abi, fat, audio-rec) and the boot/framebuffer test
make test-NAME        # one end-to-end test; `make help`, or the .PHONY list in the Makefile
make test-bios        # 15 of them again, booted through BIOS (SeaBIOS); test-bios-NAME for one
make test-all         # every test-* target (slow, ~30 min); prints PASS/FAIL per target
```

The BIOS runs reuse the UEFI tests through `tools/qemu_bios_shim.py`, which drops the OVMF drives
from the test's QEMU command line (`FOS_FIRMWARE=bios`); a check that greps loader wording must
use the shared text or `LOADER_BANNER`/`BIOS` from `tools/test_boot.py`. Run `make test` for any kernel or loader change and the matching `make test-NAME` for
the area you touched (`test-debug-mode` for crash/KUP/watchdog, `test-hda` and
`test-usb-audio` for audio, `test-nic` and `test-rtl8139` for NICs, `test-recorder*`
for the recorder, ...). Known failing: `test-nettapes` (the bridge test through the
`qemu-i386` shim; failed before the unification). `test-audio-net` and `test-nettest`
have been seen to flake once. Tests use bounded waits, keep the serial output on
failure and wait for the prompt before typing. **Every new tape needs a focused test
through its documented load/run path**; a green build is not validation. Editing a path
(moving a directory) needs a full `make test-all`: split spellings such as
`ROOT / "kernel"` and `../../..` build-script paths hide from text search.

## Debug mode, KUP, real hardware

Per-machine config files on FOSFS: `start` (boot script), `hostname`, `settings`
(`no-msi`, `deny-term-net`), `debug` (`reboot-on-crash`, `reboot-delay N`,
`kernel-update`, `watchdog [N]`, `crash-on-boot`; `tools/default-debug.txt` is installed
by default). A fatal exception writes a crash record (read and printed by the next
boot) and, with `reboot-on-crash`, resets; five pre-milestone crashes in a row stop the
loop. The loader runs the A/B slot state machine: a pushed kernel gets one try, and
falls back automatically. **KUP** (`tools/send_kernel_update.py`) pushes a kernel over
Ethernet into the inactive slot; the wire formats (KUP, NETFS, `term net`, chat) are
shared with the legacy edition so the host tools work against both.

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

## Conventions

- Every shell command's output fits an 80-column text grid; keep each print line to at
  most 79 columns and split long messages rather than relying on wrapping.
- Lower-case names throughout (FOSFS entries, files, tape names).
- Docs that move with code: README's command/tape tables, `docs/v2-design.md` (a
  paragraph per feature, milestone status), this file's pointers.
- Commit messages say what was verified and what only real hardware can verify. Do not
  commit or push unless asked.
