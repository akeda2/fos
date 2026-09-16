# fos

`fos` is a deliberately small x86 operating system for learning how a machine
boots. It starts via **legacy BIOS**, enters **32-bit protected mode**, then
runs a Rust kernel without a hosted operating system or standard library.

It is not a general-purpose OS and is intentionally incomplete. The current
milestone demonstrates a custom BIOS loader, memory-map discovery, a
Rust/assembly handoff, VGA and serial consoles, an interrupt-driven PS/2
keyboard and PIT timer, a tiny physical-frame allocator, a small FOSFS
filesystem, and a small command prompt -- all deliberately narrated with
verbose status output at every step, since this project exists to be read
and explained, not just to run.

## What happens at boot

1. The BIOS loads the MBR's 512-byte stage-one loader at physical address
   `0x7c00` and jumps to it. Stage one prints its own narration to the VGA
   text console (see `boot/stage1.S` for why it doesn't also touch serial).
2. Stage one normalizes the real-mode environment and uses BIOS EDD/LBA disk
   services (`INT 13h`, `AH=42h`) to load stage two from the raw payload
   partition at LBA 2048, then jumps to it at `0x8000`.
3. Stage two initializes the COM1 serial port and, from here on, mirrors
   every status message to both VGA text mode and COM1. It records the BIOS
   E820 memory map (`INT 15h`, `EAX=E820h`), enables the A20 address line,
   loads the Rust kernel to physical `0x10000`, masks the legacy 8259 PIC,
   and installs a flat GDT.
4. The loader enters 32-bit protected mode (`CR0.PE=1`) and far-jumps into
   the Rust entry point, passing a versioned boot-information record at
   physical address `0x5000`.
5. The Rust kernel clears BSS, validates that record, initializes its VGA/COM1
   console, builds its physical-frame bitmap from the E820 map, sets up its
   own IDT and remaps the PIC (arming the PIT timer on IRQ0 and the PS/2
   keyboard on IRQ1), then runs the prompt. Each of these steps prints what
   it just configured and why.

The message `BIOS services end now` marks the important transition: code after
that point must not call BIOS interrupts directly from protected mode. The one
narrow exception is `kernel/src/disk.rs`'s trampoline, which briefly restores
the real-mode IVT and BIOS-conventional PIC mapping around each individual
disk sector transfer -- see "Disk access and FOSFS" below.

## Requirements

- Linux or another Unix-like host with GNU Make, GNU binutils (`as` and
  `objcopy`), Python 3, and QEMU's `qemu-system-i386`.
- `rustup`, with the pinned toolchain and `rust-src` component declared in
  `rust-toolchain.toml`. `rustup` installs the pinned toolchain automatically
  on the first build when network access is available.

Build and run:

```sh
make image
make run
```

`make test` builds the image and performs a bounded serial-driven QEMU smoke
test. It checks boot milestones (including interrupt/allocator setup), line
editing, shell error handling, system inspection, interrupt/memory
introspection, FOSFS formatting, file creation, reading, removal, and halt.
`cargo test` does not work for this crate (a `no_std`, custom-target,
freestanding binary); `make test` is the sole automated validation path.

## Prompt

The prompt accepts fixed-size, allocation-free command lines. Enter and
Backspace work on both serial and the US-layout PS/2 keyboard subset. Both
input sources are interrupt-driven (IRQ1 for the keyboard) rather than
polled, alongside a free-running PIT timer on IRQ0. Writing characters into
the 0xb8000 text framebuffer does not move the blinking hardware cursor by
itself -- that is separate CRTC state (ports `0x3d4`/`0x3d5`) driven
explicitly after every VGA write so the caret always tracks the next write
position, matching what real hardware (verified on a Pentium 4 machine
booted from USB) requires. The screen is never cleared at boot, so that
VGA-only viewers keep seeing the full real-mode narration; the kernel
instead reads the cursor position the real-mode loader's own BIOS-teletype
printing already left behind and continues writing from there, rather than
resetting to row 0 and overwriting the top of that narration.

| Command | Description |
| --- | --- |
| `help` | Lists available commands. |
| `clear` | Clears VGA text and sends an ANSI clear request to serial terminals. |
| `about` | Describes this educational OS. |
| `bootinfo` | Shows boot drive, serial status, and the reserved kernel memory range. |
| `memmap` | Prints the BIOS E820 memory-map entries, decoded (usable RAM, reserved, ACPI, bad memory). |
| `memory` | Summarizes E820 usable and non-usable memory. |
| `cpuinfo` | Shows the CPUID vendor and raw feature bitfields. |
| `hexdump ADDRESS LENGTH` | Dumps up to 128 bytes inside the reserved kernel range. Addresses accept decimal or `0x` hexadecimal notation. |
| `ticks` | Shows the free-running PIT timer tick count and its configured frequency. |
| `uptime` | Shows elapsed time since boot, derived from the timer tick count. |
| `irqstat` | Shows how many times the timer (IRQ0) and keyboard (IRQ1) interrupts have fired. |
| `pmem` | Summarizes the physical-frame allocator's total/free/used frame counts. |
| `palloc` | Allocates one 4 KiB physical frame and prints its base address. |
| `pfree ADDRESS` | Returns a previously allocated frame to the free pool. |
| `diskinfo` | Shows the total image size and FOSFS partition bounds. |
| `readsector LBA` | Read-only hexadecimal/ASCII dump of one 512-byte image sector. |
| `format` | Initializes the dedicated FOSFS partition, permanently removing its files. |
| `ls` | Lists FOSFS files. |
| `cat NAME` | Displays a FOSFS text file. |
| `write NAME TEXT` | Creates or replaces a one-sector FOSFS text file. Names use up to 16 uppercase letters, digits, `.`, `_`, or `-`; oversized content, a full directory, or a full data region are each reported distinctly. |
| `rm NAME` | Removes a FOSFS file entry. |
| `serial` | Lists COM1..COM4 as detected by the BIOS at boot (base I/O port, or "not detected"), plus any serial controllers found on PCI/PCIe add-on cards (as `PCI0`, `PCI1`, ...), noting which is the local console. |
| `term [COM1\|COM2\|COM3\|COM4\|PCI0\|PCI1\|...] [BAUD]` | Enters raw terminal mode: forwards PS/2 keystrokes byte-for-byte to a remote machine over the chosen port (default COM1), and displays incoming bytes locally (VGA only, not re-echoed to serial). Press F12 to return to the shell. COM1's rate is fixed at 115200 (shared with the local console); COM2/3/4 and PCI ports accept an optional custom `BAUD` that must evenly divide 115200. |
| `halt` | Stops the CPU. Reset or power-cycle to restart. |

Argument-taking commands show a usage message for missing or extra arguments.
Unknown commands and oversized input are reported rather than ignored.

## Interrupts and the physical-frame allocator

The kernel builds a 256-entry IDT with only two populated gates (the PIT
timer on IRQ0 and the PS/2 keyboard on IRQ1), remaps the 8259 PIC's vectors
clear of the CPU's reserved exception range, and enables interrupts. Scancodes
and timer ticks are captured by minimal ISRs and drained by the shell loop
between prompts; see `kernel/src/interrupts.rs`. A small bitmap-based
physical-frame allocator (`kernel/src/pmem.rs`) tracks which 4 KiB frames the
E820 map reports as usable, reserving everything below the kernel's own
low-memory region. There is no paging yet -- this only tracks free/used
frames, exposed through `pmem`/`palloc`/`pfree`.

## Disk access and FOSFS

The kernel has no direct access to BIOS interrupts while in protected mode. For
each validated disk operation, it calls the stage-two real-mode trampoline. The
trampoline saves the protected-mode stack, transitions through a 16-bit
protected-mode GDT descriptor into real mode, invokes BIOS EDD (`INT 13h`,
extended read/write) for **exactly one sector**, then restores protected mode
and returns an explicit status. Because the kernel's own IDT and PIC remap
would otherwise break genuine BIOS real-mode interrupt handling (the BDA tick
counter BIOS's disk-timeout logic depends on stops advancing), the kernel
briefly restores the traditional real-mode IVT and BIOS-conventional PIC
mapping around each individual BIOS call, then switches back immediately
after. The kernel rejects raw LBAs beyond the image size. This is
intentionally a teaching bridge, not a general disk driver.

FOSFS is a small custom filesystem in the dedicated data partition. It has a
single superblock, four fixed directory sectors (64 entries), and contiguous
one-sector files. It does not reuse file data after deletion; `format` resets
the directory and allocation frontier. The generated image starts preformatted
with `README.TXT`. Raw `readsector` is read-only. Only FOSFS commands issue
writes, and their LBAs are constrained to the data partition.

`term` repurposes COM1 as a raw link to a remote machine by default, matching
real hardware where most physical machines expose only one serial port.
While `term` is active on a given port, that port stops being available for
anything else and becomes the remote wire exclusively: outgoing PS/2
keystrokes are sent byte-for-byte with no local echo, and incoming bytes are
shown via VGA only (never re-echoed back out over the same port, which would
corrupt the link). Exit is bound to F12 rather than a Ctrl or Shift/AltGr
combination, since F12 occupies the same physical key and produces the same
scancode regardless of keyboard layout (e.g. Nordic vs. US) -- unlike
letter/punctuation keys, whose legends and shifted symbols vary by layout. A
caveat: if serial is your only connection to fos over COM1 (no VGA in use),
entering `term` on COM1 gives up your control channel until the remote end
sends something recognizable, or you regain local VGA access -- an explicit,
documented trade-off rather than a bug.

`term` can also drive COM2, COM3, or COM4 if the BIOS detected one (`serial`
lists what was found, reading the same BIOS Data Area at physical `0x400`
that DOS and other real-mode software have always used, rather than probing
I/O space directly). Those ports are never shared with the interactive
shell, so unlike COM1 -- whose 115200 baud rate must stay fixed to keep
matching whatever terminal program is already attached to the local console
-- their baud rate is a free choice per `term` session, e.g. `term COM2
9600`. A requested rate must evenly divide 115200 (the UART's fixed clock)
so it is reproduced exactly rather than rounded; invalid combinations are
rejected with an explanation instead of silently misconfiguring the link.

Add-on serial cards are also usable, listed as `PCI0`, `PCI1`, ... in
discovery order. Legacy ISA cards jumpered to a standard COM address need no
extra support at all: BIOS POST enumerates ports by address, not by
onboard-vs-add-in, so they land in the same BIOS Data Area lookup as COM1-4.
PCI/PCIe serial cards are found separately, by walking PCI configuration
space (`kernel/src/pci.rs`) for "simple communication controller / serial"
devices (class `0x07`, subclass `0x00`) and reading each one's first I/O-space
Base Address Register -- the same technique that finds any PCI device, using
Configuration Mechanism #1 (`0xcf8`/`0xcfc`), which even PCIe chipsets still
answer for backward compatibility. The underlying UART register interface on
essentially all such cards is 16550-compatible, so no new register-level
code is needed once a card's I/O address is known; a `PCI` port behaves
exactly like a `COM2`/`COM3`/`COM4` one, including a freely chosen baud
rate. Cards that pack more than one port behind a single BAR at internal
byte offsets are not specially decoded (only cards exposing one PCI function
per port are recognized as multiple ports); genuine USB-to-serial adapters
are out of scope entirely, since they need a full USB host-controller and
device stack rather than simple port I/O (see "Current boundaries").


## Supported platform

The supported target has all of the following:

- A 32-bit-capable x86 CPU and **legacy BIOS** boot.
- BIOS EDD/LBA disk reads and the E820 memory-map service.
- VGA text mode.
- COM1 for serial output and optional input.
- A PS/2-compatible keyboard/controller for local interactive input.

QEMU is the primary supported environment. A physical USB boot requires a
legacy-BIOS-compatible machine. Native USB keyboard support is not implemented;
USB keyboards work only where firmware provides PS/2-style legacy emulation.
UEFI-only machines and CHS-only BIOS disk services are unsupported.

## Disk image layout

`build/fos.img` is a 4 MiB raw disk image:

- Sector 0: MBR and stage-one loader.
- LBA 2048: active raw payload partition.
- First 32 payload sectors: stage-two loader.
- Remaining payload sectors: flat kernel image at physical `0x10000`.
- LBA 4096: a separate 4096-sector FOSFS data partition.

`tools/write_layout.py` generates the loader constants from the actual kernel
size. The image builder rejects invalid stage-one signatures, oversized stage
two payloads, and kernels exceeding the current BIOS disk-read limit. It
preformats only the image's own FOSFS data partition; it does not touch host
storage.

## Physical media warning

Creating the image is safe: `make image` writes only inside `build/`. Copying
the image to a block device destroys that device's partition table and data.
Only use a dedicated disposable drive, independently verify its device path,
and run a write command yourself after unmounting it. This project never
automatically selects or writes a host block device.

## Current boundaries

This milestone uses one CPU, no paging, heap allocation, userspace, or
scheduler. It has FOSFS (a deliberately tiny file-per-sector filesystem, not a
general-purpose one) and interrupt-driven IRQ0/IRQ1 handling for the timer and
keyboard, but no other hardware IRQ drivers (no native disk, network, or USB
interrupt handling). The architecture keeps x86/driver code separate from
console and shell policy so later work can add those facilities without
changing the BIOS-to-kernel contract.
