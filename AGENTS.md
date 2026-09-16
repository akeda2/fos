# fos development guide

## Project purpose

`fos` is a minimal educational operating system. The current supported path is
legacy BIOS -> 16-bit real mode -> 32-bit protected mode -> a `no_std` Rust
kernel. Boot stages emit explanatory messages to VGA text memory and COM1.
Keep changes small and explainable; avoid general frameworks or abstractions
that obscure hardware behavior.

## Current architecture

- `boot/stage1.S` is the 512-byte MBR loader. It loads stage two from LBA 2048.
  It must retain the partition table area at bytes 446-509 and the `0x55aa`
  signature at bytes 510-511.
- `boot/stage2.S` gathers E820 memory information, verifies A20, loads the
  kernel at physical `0x10000`, and enters flat 32-bit protected mode.
- `kernel/` is a freestanding Rust binary. `linker.ld` reserves the initial
  low-memory kernel region through `0x20000`; do not place loader, boot-info,
  or disk-trampoline data there.
- The packed version-2 `BootInfo` at physical `0x5000` is the loader/kernel ABI.
  Validate its magic, version, size, and count before using it. Use unaligned
  reads for packed multi-byte fields.
- `kernel/src/disk.rs` invokes the stage-two BIOS EDD trampoline. It performs
  exactly one sector transfer and only after validating the LBA and buffer
  constraints. Do not call BIOS interrupts directly from protected mode.
  Because the trampoline drops into real mode, and the kernel's own IDT/PIC
  remap would otherwise break BIOS's real-mode interrupt handling, `transfer`
  briefly restores the real-mode IVT (`IDTR` base 0) and the BIOS-conventional
  PIC vector mapping around each individual BIOS call, then switches back.
- `kernel/src/interrupts.rs` builds a 256-entry IDT (only IRQ0/IRQ1 gates
  populated), remaps the 8259 PIC clear of the CPU exception range, arms the
  PIT at 100 Hz, and services the timer/keyboard IRQs with minimal
  `x86-interrupt` ABI handlers. Input is interrupt-driven, not polled.
- `kernel/src/pmem.rs` is a small bitmap-based physical-frame allocator (4 KiB
  frames) built from the E820 map; it reserves everything below the kernel's
  own low-memory region. There is no paging.
- `kernel/src/fosfs.rs` implements FOSFS, a deliberately small custom
  filesystem: one superblock, four directory sectors, 64 fixed entries, and
  contiguous one-sector files.

## Educational verbosity

This project exists to be read and explained, not just to run. Boot stages
and kernel subsystems should narrate what they are doing and why in their
status messages (e.g. which BIOS interrupt is being called, what a PIC remap
or IDT load accomplishes), not just announce bare completion. When adding a
boot step or kernel subsystem, prefer an explanatory message over a terse one,
and keep `README.md`'s command table and "What happens at boot" walkthrough in
sync with the actual command surface and boot sequence.

## Disk image and safety

`tools/build_image.py` creates `build/fos.img`, a 4 MiB raw image:

| Location | Contents |
| --- | --- |
| LBA 0 | MBR / stage-one loader |
| LBA 2048 | Active raw boot payload partition |
| First 32 payload sectors | Stage-two loader |
| Following payload sectors | Flat 32-bit Rust kernel |
| LBA 4096 | Dedicated 4096-sector FOSFS data partition |

The image builder is the single source of truth for partition placement.
`tools/write_layout.py` emits matching stage-loader constants from the actual
kernel size. Never add host block-device writes, automatic device selection,
or commands that let FOSFS write outside its own partition. Copying an image
to a device is destructive and must remain an explicit user action.

## Build and validation

The pinned toolchain is `nightly-2026-09-13`, with `rust-src`. Required host
tools are GNU Make, GNU `as`/`objcopy`, Python 3, and `qemu-system-i386`.

```sh
make image    # Produces build/fos.img only.
make run      # Boots it interactively in QEMU.
make test     # Runs the serial-driven QEMU smoke test.
```

Always run `make test` for boot, loader, kernel, shell, disk, or FOSFS changes.
The smoke harness must use bounded waits, preserve serial output on failure,
and wait for the prompt before sending input.

## Current command surface

System commands: `help`, `clear`, `about`, `bootinfo`, `memmap`, `memory`,
`cpuinfo`, `hexdump ADDRESS LENGTH`, and `halt`.

Interrupt/memory commands: `ticks`, `uptime`, `irqstat`, `pmem`, `palloc`, and
`pfree ADDRESS`.

Storage commands: `diskinfo`, `readsector LBA`, `format`, `ls`, `cat NAME`,
`write NAME TEXT`, and `rm NAME`.

Serial commands: `serial` (lists COM1..COM4 as detected via the BIOS Data
Area) and `term [COM1|COM2|COM3|COM4] [BAUD]` (raw terminal pass-through;
F12 exits).

`readsector` is read-only. `hexdump` is limited to the reserved kernel range
and at most 128 bytes. FOSFS names are limited to 16 uppercase alphanumeric or
`.`, `_`, and `-` characters. The fixed command buffer and single-sector
filesystem are intentional teaching constraints.

`term` defaults to COM1, reusing the same port most physical machines
already have rather than requiring a second UART. While active on a given
port, that port is exclusively the remote link: PS/2 keystrokes go out with
no local echo, and incoming bytes are shown via VGA only
(`console::write_vga_only`, which intentionally skips serial mirroring to
avoid retransmitting incoming bytes back over the same wire). The escape
key is F12 (scan code `0x58`) -- not a Shift/AltGr/Ctrl combination --
because function keys are layout-independent, satisfying keyboards (e.g.
Nordic) where such combinations are ambiguous or reserved. A serial-only fos
session loses its control channel upon entering `term` on COM1; this is a
documented, intentional trade-off.

`term COM2`/`COM3`/`COM4` (`console::Uart`, detected via `console::detect_ports`
reading the BIOS Data Area at physical `0x400`) accept an optional custom
`BAUD` argument, since those ports are never shared with the interactive
shell. COM1's baud rate is deliberately kept fixed at 115200 -- it is the
same port the shell itself may already be driven over, so changing its rate
mid-session would desync any attached terminal program. A requested `BAUD`
must evenly divide `console::UART_CLOCK` (115200) so the exact rate is
programmed rather than rounded; `term` reports and rejects invalid rates or
undetected ports rather than silently misconfiguring the link.

## Deliberately deferred work

- Paging, heap allocation, filesystems beyond FOSFS, userspace, scheduling,
  native USB, further hardware IRQ drivers beyond the timer/keyboard, and
  robust fault recovery.
- A native ATA/AHCI disk driver. The current protected-mode-to-real-mode BIOS
  trampoline is educational and intentionally narrow, not a scalable disk
  architecture.
- Broad physical-machine support. The documented target requires legacy BIOS,
  BIOS EDD/LBA, E820, VGA text, COM1, and PS/2-compatible input. QEMU is the
  primary validation environment.

## Deferred optional x86-64 path

Do not treat x86-64 as a compiler-target switch. BIOS begins in real mode; the
64-bit path needs CPUID long-mode detection, a 32-bit transition, PAE, identity
mapped early page tables (a 1 GiB 2 MiB-page map is a good initial teaching
layout), EFER.LME, paging enablement, and a far transfer to long mode.

Implement it as a parallel, optional boot path that retains the working
32-bit kernel as fallback. Add a separate `x86_64-unknown-none` target,
64-bit linker/entry code, and an explicitly versioned or architecture-neutral
boot ABI. Keep VGA, serial, PS/2, shell, FOSFS format, and command policy
portable while separating port I/O, CPU setup, interrupt entry, and pointer
conversions into architecture modules.

The 32-bit BIOS trampoline cannot be called unchanged by a long-mode kernel.
For an initial x86-64 milestone, make post-boot FOSFS access read-only or defer
it; prefer a native disk driver before attempting a 64-bit-to-real-mode
transition that preserves full 64-bit state.
