# fos v2: design note

Status: draft, decisions marked **Decided**. Milestone M0 is under way. It records what we
decided in conversation, what the v1 work taught us, and the open questions that
should be settled before the first line of v2 code.

## 1. Why a v2

fos v1 is BIOS -> real mode -> 32-bit protected mode -> a `no_std` Rust kernel.
It works, it is used for network testing in production, and it teaches the
x86 boot path well. Its limits are the same few things over and over:

- **32-bit address space.** The kernel cannot address RAM above 4 GiB. The
  Skylake test machine has 16 GiB. `memtest` needs a hand-written long-mode
  excursion to reach most of it, and the pools (4 KiB frame pool, 1 MiB big
  pool, tape `alloc` vs `alloc_frames`) exist only to work around that and the
  low-memory budget.
- **The low-memory region.** The whole kernel, its `.bss`, its stack and all
  static buffers must fit below `0x9f000`. We hit that wall three times this
  year (xHCI, audio rings, page tables) and fixed it by moving 160 KiB of audio
  buffers out of `.bss`. v2 has no such region.
- **BIOS-only boot and disk access.** Every disk is reached through INT13h in
  real mode (one round trip per call) or through a native driver we had to write
  *because* the BIOS emulation breaks (USB). Under UEFI there is no INT13h at all.
- **Legacy hardware assumptions.** 8259 PIC, PIT, VGA text mode, PS/2. Newer
  boards drop some of them.

A 64-bit UEFI kernel removes the first three outright and makes the fourth a
decision instead of an accident.

## 2. Goals and non-goals

Goals:

1. Boot on the machines we own: the Skylake (UEFI-capable, 16 GiB), and in
   principle any x86-64 UEFI machine, with QEMU + OVMF as the test bed.
2. One flat 64-bit address space with all RAM identity-mapped; no pools split by
   address range, no long-mode excursions, no `u32` physical addresses.
3. Keep what makes fos *fos*: small, readable, narrated boot, ring-0 trusted
   tapes, remote-first development (`term net`, KUP, `devloop`), a flat FOSFS.
4. Stay usable day to day as a network lab OS: the NIC drivers, `nettest`,
   `router`, `bridge`, `memtest` and friends all have to come across.

Non-goals (for v2.0):

- SMP, preemptive scheduling, user mode / process isolation. Tapes stay ring 0.
- Secure Boot, a bootloader menu, network boot, graphics beyond a text console.
- Pentium 4 / 32-bit-only CPUs. They stay on v1 (see section 12).
- Wi-Fi, NVMe-class performance work, USB hubs beyond what we already have.

## 3. Repository layout and relationship to v1

**Decided: v2 is a `v2/` tree in this repository.** v1 stays exactly where it is
at the top level, untouched, and keeps working for the Pentium 4 and as the
"real mode to protected mode" teaching edition. Nothing moves or is renamed.

v2 is a new top-level tree in the same repository:

```
v2/
  loader/     UEFI application (x86_64-unknown-uefi): the "stage two" of v2
  kernel/     x86_64 kernel (custom no-std target), identity-mapped
  tapes/      64-bit tapes (the shared tape-rt/tape-net crates, widened)
  tools/      image builder (GPT + FAT32 ESP + payload + FOSFS), test harness
```

Drivers are **copied and widened**, not shared, at first: the v1 drivers are
33 K lines of kernel code that assume `u32` physical addresses in a hundred
places, and a shared crate would force v1 and v2 to move in lock step. Once v2
has settled, the pure-protocol and register-level parts can be factored into
crates both trees depend on. FOSFS, the KUP wire protocol, NETFS, `term net` and
the peer address book are format/wire contracts and stay byte-compatible so the
existing host tools (`devloop.py`, `send_kernel_update.py`, `foscopy.py`,
`term_net.py`) work unchanged against both.

## 4. Boot path

```
firmware (UEFI) -> loader (PE/COFF app, \EFI\BOOT\BOOTX64.EFI on the ESP)
                -> kernel (ELF64, read from an A/B slot by LBA)
```

The loader is the narrated "stage two": it prints what it is doing to the
firmware console, GOP and serial. It is written in Rust for the
`x86_64-unknown-uefi` target (the compiler already has it; no `std`), with a
hand-rolled minimal binding to the handful of boot services it needs rather than
a dependency: `OutputString`, `AllocatePages`, `GetMemoryMap`, `LocateProtocol`
(GOP, Block I/O, Loaded Image), `OpenProtocol`, `ExitBootServices`, and the
configuration table lookup for the ACPI RSDP. That is roughly 600 lines.

Loader steps:

1. Find its own disk through Loaded Image -> Device Handle -> Block I/O.
2. Read the A/B slot metadata block and run `select_kernel_slot`'s state machine
   (a port of the logic now in `boot/stage2.S`), exactly as v1: one try for a
   pending slot, automatic fallback, same on-disk metadata bytes.
3. Read the chosen slot's kernel (raw, by LBA, through Block I/O) and load it as a
   **position-independent** ELF64 image: the loader allocates whatever physical
   pages the firmware has free, copies the `PT_LOAD` segments there and applies
   the `R_X86_64_RELATIVE` relocations. (The first M1 attempt linked the kernel
   at a fixed address, 16 MiB, and the very first firmware tested, OVMF, refused:
   its own boot-services data lives there. Real firmware memory maps differ even
   more, so a fixed address was never going to be robust.)
4. Collect the memory map, GOP framebuffer description, RSDP pointer, the boot
   disk's GPT partition GUID, and the UEFI runtime-services pointer into a
   `BootInfo` v4.
5. `ExitBootServices` (with the standard retry loop when the map key goes stale),
   then jump to the kernel entry in the mode UEFI left us in: long mode,
   identity-mapped, interrupts off.

The kernel immediately replaces the firmware's page tables and GDT with its own.

`BootInfo` v4 is the loader/kernel ABI, passed by pointer in a register (not at a
magic physical address): magic, version, size; memory map as 64-bit
`(base, length, type)` triples (UEFI types folded into the E820 numbering the
kernel already understands); framebuffer base/size/geometry/pixel format; RSDP;
runtime services pointer and runtime-region list; boot slot number and slot
metadata; the boot disk GUID; and the loader's own reserved ranges. Validate
magic, version and size first, as v1 does.

## 5. Image layout

A GPT disk with three partitions:

| Partition | Contents |
| --- | --- |
| ESP (FAT32, ~32 MiB) | `\EFI\BOOT\BOOTX64.EFI` (the loader) and nothing else the kernel depends on |
| Payload (raw, GPT type `f05b2a01-0000-4f05-8000-0000000000a1`) | slot metadata block in v1's byte layout, then kernel slot 0 and slot 1 (A/B, 8 MiB each) |
| FOSFS | the data partition (tapes, `debug`, `hostname`, `start`, `router.cfg`...) |

The kernel is deliberately **not** a file on the ESP: KUP writes the inactive
slot over the network, and writing raw slots needs no FAT writer in the kernel.
The loader reads slots by LBA through Block I/O (it has the firmware's drivers
there). `tools/build_image.py` gains a small pure-Python FAT32 writer for the ESP
(stdlib only, in keeping with the project's tool list) instead of depending on
`mtools`/`mkfs.fat`.

## 6. The kernel: mode, memory, interrupts

- **Target**: a custom `x86_64-fos` target (no red zone, no SSE in the kernel,
  `panic = abort`), built with `-Z build-std` like v1. Kernel code never uses
  SSE; tapes may enable it for themselves.
- **Memory map**: the kernel is a position-independent image the loader places
  wherever the firmware has room, and is no longer size-constrained. One physical allocator over the whole
  memory map replaces `pmem`'s two pools; its bitmap is allocated at boot from
  the first usable region large enough, not baked into `.bss`. Allocation takes a
  "below 4 GiB" flag for DMA devices that need it (RTL8139, EHCI/UHCI, and anything
  that cannot do 64-bit descriptors).
- **Paging**: identity-map all RAM with 2 MiB (and 1 GiB where possible) pages,
  kernel text read-only + executable, everything else NX. Unmapped guard pages
  for stacks. MMIO is mapped on demand (`map_mmio` keeps its role, but with a real
  allocator behind it instead of a fixed pool of eight page tables). This keeps
  the property v1 argues for: wild pointers fault legibly.
- **Interrupts**: 64-bit IDT; the 100 Hz tick comes from the local APIC timer
  calibrated against the TSC and the HPET/ACPI PM timer (the PIT is not
  guaranteed on UEFI machines). Legacy INTx drivers need the I/O APIC; MSI/MSI-X
  stays the preferred path and is extended to more drivers. A double-fault IST
  stack replaces v1's task gate.
- **ACPI**: only what is needed: RSDP -> XSDT -> MADT (APIC IDs, I/O APIC),
  FADT (reset register, PM timer), MCFG (PCIe config space), HPET. No AML
  interpreter.
- **Reboot and power**: `reset`/`reboot` use the UEFI runtime `ResetSystem`
  (warm or cold); `reboot soft` stops being a special case.
- **Crash evidence**: v1 stores the crash record in RAM, which some machines wipe.
  v2 can also write a small record into a UEFI NVRAM variable through runtime
  services, which survives every reset (rate-limited to spare the flash).

## 7. Console and input

- **Output**: the GOP linear framebuffer, drawn by a small text console with an
  embedded 8x16 bitmap font and the ANSI subset v1 already honours. Scrolling
  uses a shadow buffer and one blit; the framebuffer is mapped write-combining
  (PAT) so it is not painfully slow. COM1 mirroring and the `term net`/printer
  relay queues stay exactly as they are (the single `console::write_bytes`
  choke point survives the port). The "every command fits 80x25" rule becomes
  "fits an 80-column text grid", which is what every remote session assumes
  anyway.
- **Input**: **Decided: a local keyboard is required**, not optional. PS/2 where
  the board has it (a legacy 8042 path, easy), and USB HID keyboards through the
  xHCI/EHCI drivers we already have: boot-protocol HID needs interrupt-IN
  endpoints, which neither driver implements yet, and USB enumeration has to be up
  early enough to type at the first prompt. Because the Skylake's only USB
  controller is xHCI and its firmware may no longer emulate PS/2 for a USB
  keyboard once we take the controller over, USB HID is the critical path for
  local input on that machine and gets its own milestone early (M3b, section 11)
  instead of being left until the end. The firmware's own `SimpleTextInput`
  covers the loader and the first moments of the kernel until then.
  **Decided: a machine must also boot with no local keyboard at all** (headless
  lab boxes, a stick unplugged, a keyboard that never enumerates). Nothing in the
  boot path may wait for, or fail because of, a missing or silent keyboard: the
  kernel reaches the `term net` wait and the shell loop whether or not any key
  source exists, and keyboard bring-up failures are reported and ignored. The
  keyboard is an additional input, never a boot dependency.

## 8. Storage and the boot disk

v1's lesson: native drivers beat BIOS emulation, and a UEFI machine has no
BIOS emulation. v2 therefore needs native drivers for the boot media from the
start: **xHCI** (the Skylake's only USB controller), **AHCI** and **EHCI** (the
older boards), all of which exist in v1 and port directly; NVMe later.

The new problem is **finding the boot disk without the firmware**. After
`ExitBootServices` there is no Block I/O. The loader passes the boot disk's GPT
partition GUID (and the ESP's); the kernel scans its native controllers, reads
each disk's GPT and picks the one carrying that GUID. For a USB stick that means
the kernel brings up the xHCI/EHCI controller *during boot* rather than on an
explicit `usb native` command, and does so before the first FOSFS access (the
`start` file, `hostname`, `debug`). The controller handoff, which v1 learned to do
carefully, becomes a normal boot step.

KUP keeps its design: write the inactive slot through the native driver and
update the metadata block, A/B fallback in the loader. `reboot` goes through
firmware, so a pending slot is tried on the next boot.

### Disk numbers, volume labels and stick GUIDs

* **`disk1` is always the boot disk.** The drivers number disks in the order they find them
  (USB sticks by root port, then SATA, then NVMe), which changes when a stick is plugged into
  another port. Once the boot disk is known (by the payload-partition GUID the loader passes),
  `block::move_to_front` makes it `disk1`; the others keep their discovery order behind it and
  anything attached later goes at the end. The other numbers are *not* stable.
* **Volume labels name a volume independent of numbering.** A FOSFS volume has an optional label
  (a file-name-style word, up to 16 characters) in 16 spare bytes of the superblock (offset 64;
  v1 ignores them and a v1 `format` clears them). `label` lists them, `label diskN|@LABEL NAME`
  sets one, `label diskN|@LABEL -` clears it, a label may be on one volume only, and `ls`, `cat`,
  `write` and `rm` take `diskN` or `@LABEL` as their first word to pick a volume. `build_image.py
  --label NAME` sets one at build time.
* **`format diskN [NAME] [force]`** creates a FOSFS volume. It announces what it would erase and
  does nothing until the very next command is `format diskN confirm`. Never the boot disk. A disk
  that already has a FOSFS partition gets just that volume cleared (other partitions untouched); a
  blank disk gets a fresh GPT (random GUIDs, both copies, CRCs) with one FOSFS partition covering
  it; any other disk (an MBR, a file system, a GPT with only foreign partitions) needs `force`
  because its partition table is replaced. The data area is not wiped, only the superblock and
  directory. Test: `make test-format`.
* **`install diskN [LABEL] [bare] [force]`** (DOS's `sys`, plus the files) makes another disk bootable from the running
  system: a new GPT with fresh GUIDs (so boot-disk discovery can tell the copy from the original), the boot disk's ESP
  copied sector for sector (the firmware boots it through the removable-media fallback path, so no NVRAM entry is
  written), the *running* kernel (the slot named in BootInfo) as slot 0 with fresh slot metadata, and a FOSFS volume
  filling the rest of the disk holding every file of the boot volume (`bare` skips the files; a label is optional and
  not copied, since a label may be on one volume only). Same discipline as `format`: announce, then
  `install diskN confirm` as the very next command; never the boot disk; a disk that is neither blank nor an earlier
  fos disk needs `force`. Code: `kernel/src/install.rs`, `gpt::write_table`. Test: `make test-install` (installs
  onto a blank disk, checks the GPT with `sgdisk`, boots the copy alone). Not tested on hardware yet (the Skylake's
  SATA SSD is the intended first target).
* **`ed`, `copy` and the diagnostic commands (ported from v1).** `ed` (`run ed NAME`, also
  `diskN/NAME` and `@LABEL/NAME`) is the v1 line editor; its cursor-key line editor needs the
  arrow/Home/End/Delete keys, which both keyboard decoders now produce as private codes
  (`keymap::KEY_*`) and a running tape receives as the ANSI sequences a terminal would send, and
  the framebuffer console now understands `ESC [ n C`, `ESC [ n D`, `ESC [ K` and `ESC [ 2 J`.
  The tape file callbacks (`fs_read`/`fs_write`/`fs_remove`) accept the same `diskN/NAME` and
  `@LABEL/NAME` references. `copy SRC NAME DST [NEWNAME]` moves a file between volumes through a
  page-allocator buffer (no 64 KiB limit). `cpuinfo`, `bootinfo`, `memmap`, `irqstat` (the I/O APIC
  pins), `pci read|write B:D.F`, `io r8|w8..` and `mmio r32|w32..` (mapped addresses only, never the
  kernel image) and `clear` report or poke the machine from the shell. Tests: `make test-ed`,
  `test-v2-disks`, `test-v2-diag`.
* **RTL8168/RTL8111 NICs** (`rtl8168.rs`, PCI 10ec:8168 and 10ec:8161, ported from v1) are driven
  polled, through the I/O-port BAR: 64 receive and 32 transmit descriptors, the 8168G-and-later
  register order, a frame sent with no link dropped like on the e1000. The terminal, tapes and
  `nic` reach every NIC family through the `nic::Nic` enum. QEMU has no RTL8168 model, so nothing
  automated covers the chip itself (as in v1); on the Skylake (NIC 6, an RTL8168H cabled to I350 port 2)
  `memtest soak net=2,6` ran clean at 2,000 frames/s (polled) and, once MSI worked, at 8,000 frames/s each
  way for 60 s: 539,409 frames each way, none lost or corrupt, 532,581 MSI interrupts, 0 overruns. Interrupts: MSI, with a self-test (forced interrupt plus one transmitted frame, since the 8168G/H ignore
  the forced bit) and v1's rule that receive-descriptor-unavailable/FIFO-overflow are masked until the ring is
  drained, never answered with a reset; polled when MSI fails or `no-msi` is set. Legacy INTx is the fallback
  when MSI fails (tested on the Skylake by forcing MSI off: pin 11, 475,729 frames each way at 8,000/s, no loss,
  847,604 interrupts, no storms). `nic` shows the chip version, link speed/duplex and recoveries; `nic regs [N]`
  dumps the chip's registers and which descriptors the card owns. Not done: a periodic watchdog (a stuck
  transmit descriptor or a PCI system error resets the chip inline, counted as a recovery).
* **USB audio (xHCI isochronous, Audio Class 1.0 and 2.0).** `usb_audio.rs` recognises Audio Class devices,
  prints their streaming alternate settings (channels, bit depth, endpoint, sync type, rates, feedback
  endpoint), keeps the device, and streams: it selects the sample rate (class 1.0: SET_CUR on the endpoint;
  class 2.0: SET_CUR on the clock source, and the offered rates come from its GET RANGE), selects the alternate
  setting, configures the isochronous endpoint (`xhci::configure_isoc_endpoint`) and keeps a window of 192
  one-packet transfers queued (`queue_isoc`, start-ASAP). Each completion event refills (playback) or drains
  (capture) a packet slot through a 1 MiB PCM ring; the events come in through the normal xHCI event draining
  (a 1024-entry event ring: a high-speed endpoint completes 8,000 transfers/s). Shell commands: `audio`
  (status), `audio start|stop`, `audio capture SECONDS [RATE]` (per-second frame counts and per-channel
  peaks), `audio tone [HZ] [SECONDS] [RATE]`. `make test-usb-audio` plays a tone through QEMU's class 1.0
  `usb-audio` and checks the WAV it records (439 Hz for 440). Confirmed on a Zoom H6 in multichannel mode on
  the Skylake (class 2.0, high speed, rates 44.1/48/88.2/96 kHz): capture of 6 channels 24-bit at 48 kHz gave
  47,984-48,016 frames/s with no errors, playback of 2 channels 24-bit streamed 8,000 packets/s with no errors.
  Tapes reach it through six appended `TapeApi` callbacks (`audio_info`, `audio_start`, `audio_stop`,
  `audio_read`, `audio_write`, `audio_stats`; `audio_read/write` also drain the USB events, and the kernel stops
  any stream a tape leaves running): `record NAME [SECONDS] [RATE]` captures every input channel into RAM and
  writes NAME.wav (hardware only: QEMU's `usb-audio` has no input), `play NAME.wav` streams a PCM WAV file,
  converting width and channel count (tested in QEMU: 16-bit stereo and 24-bit mono files, 439/879 Hz measured).
  An asynchronous playback stream also runs its feedback endpoint (3-byte 10.14 at full speed, 4-byte 16.16 at
  high speed): reports within 10% of nominal set the frames-per-packet step, so playback follows the device's own
  clock; `audio` shows the report count and the device's clock against nominal. On the H6: 313 reports in 5 s,
  none rejected, exactly nominal at 48 kHz (QEMU's `usb-audio` has no feedback endpoint, so it is hardware-only).
  Capture and playback together (`audio duplex 10`) ran 10 s on the H6 with 80,016 + 80,007 packets (about 16,000
  transfer events/s) and no errors.
  **xHCI events by interrupt.** After the settings file is read (`no-msi` decides), `xhci::enable_interrupts_all`
  moves each controller's events to MSI (MSI-X table entry 0 when only that exists, as on QEMU's xHCI), with a
  self-test (a no-op command whose completion must raise the interrupt) and 1 ms interrupt moderation; a
  controller that fails stays polled. The event ring is consumed by one function, `xhci::drain`, run by the
  interrupt handler or, otherwise, by the foreground with interrupts off: audio transfer events are handled
  inline there (`usb_audio::irq_event`: refill a packet, copy captured bytes, requeue) and everything else goes
  to a soft queue the foreground reads (`Controller::next_event`). So audio no longer depends on the foreground:
  `audio start 48000 both`, `sleep 8` (a shell command that blocks without polling) and `audio` on the H6 showed
  68,436 capture and 68,520 playback packets, no errors, while the foreground did nothing. Audio objects shared
  with the handler are atomics or published/unpublished with interrupts off (see the comment in `usb_audio.rs`).
  **RME Fireface UFX** (class compliant, Audio Class 2.0, high speed, on the Sandy Bridge machine's NEC
  uPD720200 xHCI): 22-channel 24-bit capture (a 12-channel alternate too) and 22-channel playback (a stereo
  alternate), 1,024-byte endpoints (no high-bandwidth endpoints needed), async with feedback, rates 44.1 to
  192 kHz. Capture at 48 kHz, duplex at 48 and 96 kHz (96 kHz: 38 MB of capture in 6 s) and an 8 s blocked-foreground
  run all gave no errors; its feedback shows its clock a few hundred ppm off nominal and the playback follows it.
  The NEC controller needed the MSI self-test not to drain the ring before the interrupt (it drops a moderated
  interrupt once the ring is empty); `enable_interrupts` also falls back to MSI-X if plain MSI fails. The Zoom
  H6 on the Skylake shows signal from a microphone on input 1 on the third of its six capture channels.
  Not done: streaming a long take to disk (the `record` tape still captures into RAM first, up to 64 MiB).
  **Modes:** each alternate setting of a streaming interface is a mode (channels, bit depth, rates). `audio modes` lists
  them per direction; `audio use capture|playback N|auto` picks one (applies to the next start; the chosen mode must offer the
  rate, otherwise the best one that does is used); tapes use `audio_select` and audio-net's `inmode=`/`outmode=`.
* **Intel HD Audio, behind the same tape calls.** `audio.rs` is a small facade (`enum Backend { Usb, Hda(index) }`): the
  `audio_*` calls of the tape ABI go to *the active device*, which is the one `audio device N` picked, else USB audio if
  present (so nothing changes for a machine that had one), else the best HD Audio controller. No tape-ABI change was
  needed: `AudioInfo` reports class version 0 for HD Audio, slot bytes 2 (16-bit) or 4 (24 bits left-justified in 32, as
  the hardware carries them), and the rates the converters offer; `play`, `record`, `recorder` and `audio-net` run on it
  unchanged. `hda.rs` (ported from v1's QEMU-only driver, with what real codecs need): reset with a settle hold, codec
  discovery, CORB/RIRB of 2/16/256 entries, a walk of every widget (capabilities, PCM rates and sizes, pin default
  configuration, connection lists with ranges and long form), a **path search** through mixers and selectors (playback:
  output pin down to a DAC; capture: ADC down to an input pin) scored by what the pin is (line out/in over speaker/mic, no
  connection skipped, plugged-jack bonus where the pin can sense one), then at `audio_start` the stream descriptor, BDL
  (8 periods of 512 frames, interrupt on each), format word, converter stream tag, connection selects, amplifier unmuting
  at 0 dB along the path, power states, EAPD, pin control (microphone bias for mic pins). The interrupt handler services
  every period the hardware has finished since last time, found from the DMA position buffer (LPIB if that does not
  work), so a late or merged interrupt loses nothing; PCM moves through the same `PcmRing` as USB audio. Interrupts: MSI
  with a self-test that needs no audio (one verb with the RIRB response interrupt on), else legacy INTx probed through
  the I/O APIC, else polled (`no-msi`, or nothing worked; the foreground then services from `audio_read/write`).
  DMA memory is below 4 GiB unless `GCAP.64OK`. `hda` dumps the controllers, widgets and chosen paths (what to read on an
  unknown machine); `audio device` lists/selects among USB audio and HD Audio controllers. `make test-hda` covers MSI,
  INTx and polled delivery, a 440 Hz tone through QEMU's WAV backend, the `play` and `record` tapes, capture (QEMU's WAV
  backend has no input, so silence from the `none` backend; the capture buffer is pre-filled with a pattern, so a
  peak of 0 shows the controller's DMA wrote it), and USB audio next to HD Audio. **Unverified on real hardware:**
  everything QEMU's two-widget synthetic codec cannot exercise: mixers/selectors, pin sensing, EAPD, mic bias,
  24-bit containers, multi-codec buses, the chipset snoop/traffic-class quirks, HDMI codecs. A tape picks the device with
  `audio_use_device(N)` (API version 3, with `audio_devices` to list them; `play device=N NAME` is the example); the
  choice is undone when the tape ends, so the operator's `audio device N` is never overridden.
* **`audio-net` (network audio, stage A: raw Ethernet).** One tape, `audio-net send|recv`, with an `audio-wire`
  crate (host-tested) for the formats. Raw mode (ethertype 0x88ba): an 18-byte header (sequence, frame
  timestamp, stream id, channels, bit depth, frames, rate) and big-endian 16/24-bit interleaved samples,
  1 ms packets for a stereo pair and shorter for many channels so a packet fits one frame. `send` takes the
  capture side of the USB audio device (`ch=9-10,1` picks channels, `bits=`, `to=MAC`) or a test tone
  (`src=tone=440`, no audio hardware needed); `recv` locks onto the first sender, plays on the device
  (`ch=` places the stream's channels on output channels), bridges short sequence gaps with silence, resyncs
  after long ones, and keeps the playback ring near `target=` ms by dropping or repeating one frame at a
  time (a resampler replaces that in a later stage). Playback's transfer window is now ~3 ms (was 6; measured on Skylake -> laptop over USB, the click-to-click latency fell from 17 to 15 ms at `target=6`, underruns unchanged) (a packet plays a
  whole window after it is filled, so the window is the latency). `micros()` no longer goes backwards when
  the timer interrupt is late (a pending timer interrupt counts as an elapsed tick, and the value is clamped).
  Tested between two QEMU guests (`make test-audio-net`: stereo 24-bit and mono 16-bit tones, 0 lost,
  measured 438/877 Hz). **Stage B, RTP over UDP:** `proto=rtp` sends and receives standard RTP (RFC 3550) over
  UDP over IPv4 with `L16`/`L24` big-endian samples, dynamic payload type 96 (`pt=`), packet times of 125 us to
  1 ms chosen so a packet is at most 1440 bytes (AES67's limit), correct IP and UDP checksums, a multicast
  destination mapped to its 01:00:5e MAC (no ARP; no IGMP join yet), and an SDP description printed by `send`
  (`rtpmap L24/rate/channels`, `ptime`, `ts-refclk:localmac=...` since there is no PTP, `mediaclk:direct=0`) for the
  other side. Addressing: `ip=A.B.C.D/N` with `gw=`, or `ip=dhcp` (the client shared with `ip-chat`/`router`);
  ARP is answered and resolved. `recv` takes `chan=` and `bits=` (RTP does not carry the channel count). The
  two-guest test sniffs the segment and checks the packets (version, payload type, single SSRC, 288-byte payloads,
  sequence +1, timestamp +48, valid checksums) and measures the played tone; `ip=dhcp` is tested against QEMU's
  slirp DHCP. Interoperability confirmed with `ffmpeg` both ways (it decodes our RTP from the printed SDP; we
  play its `L24` RTP with 5 ms and 1 ms packets) on the lab network.
  **Stage D, resampler:** the receiver no longer drops or repeats frames for clock drift. `audio_wire::Resampler`
  is a 4-point Catmull-Rom interpolator in integer arithmetic (bit-exact one-frame delay at step 1.0; accurate to
  about -60 dB at 1 kHz otherwise) and `Servo` a PI controller on the playback ring level (second order,
  w = 0.14 rad/s, z = 0.87, limited to +-2000 ppm) that learns the steady clock ratio; `drift=slip` keeps the old
  behaviour. Host unit tests cover the filter, the servo's convergence on a 400 ppm offset and the formats.
  **`drift=ptp` (prototype, AVTP, locked PTP clock):** fits the sender's frames against its capture stamps and the
  sound card's played frames (written minus ring level) against PTP time (`audio_wire::RateFit`, least squares, since the
  level moves in whole packets), and starts the servo at the ratio after 8 s (`Servo::seed_ppm`). Measured on the lab
  (clock1 steered master, rack2 tone sender, skylake2 USB audio): the fit said -171 ppm where the servo learned -172, so
  the two independent measurements agree; seeding did not shorten settling, because the ring level seen at arrival stays
  near 60 frames against a 288 target in every run (with or without it), i.e. the servo's error is mostly not
  controllable (suspected: the USB driver pulls data into transfers ahead of the visible ring). Not investigated further.
  Hardware (H6 on the Skylake to the UFX on the Sandy Bridge machine, raw Ethernet, 100 s, 100,045 packets): 0 lost,
  0 frames dropped or repeated, learned offset 8 ppm (so the two interfaces' clocks agree to ~10 ppm). With the
  sender deliberately skewed (`skew=` on a tone source) the learned offset moves by exactly the skew (+400 then
  -400 ppm gave +64 and -737 ppm, a difference of 801), again with nothing dropped. A caution: console output
  is slow (about 10 ms a line over serial), so any per-packet or per-loss message makes a receiver lose the next
  packets; `verbose` therefore prints only a few gap lines and one status line per 10 s.
  **Stage C, Opus:** `codec=opus` (with `proto=rtp`, 48 kHz) sends and receives Opus (RFC 7587, `opus/48000/2`) from the
  vendored floating-point libopus 1.6.1 (`build.rs` links `build/fosfor/opus-float/libopus_float.a`; audio-net is a hard-float tape). One RTP stream per stereo
  pair (an odd last channel is a mono stream) on UDP ports `port`, `port+2`, ..., each with its own SSRC and the
  same RTP timestamps; `send` prints an SDP with one media line per stream. Options: `bitrate=` (kbit/s per pair,
  default 128), `frame=` (2.5, 5, 10, 20, 40, 60 ms; default 20), `complexity=` (0-10, default 5). The receiver
  decodes every stream, conceals a short gap with libopus's own loss concealment, writes the decoded frames on
  a shared timeline at their RTP timestamps (so pairs stay aligned), and plays the frames all streams have reached
  through the same resampler and servo. QEMU test: a stereo 770 Hz tone (150 packets, 0 lost, measured 765 Hz,
  timestamp step 960) and three channels as two multicast streams with equal timestamps.
  Hardware (H6 on the Skylake, NIC 3, to the UFX on the Sandy Bridge machine, 20 ms frames, 128 kbit/s, 60 s): 3003
  of 3003 packets, 0 lost, no underruns while streaming, learned clock offset -4 ppm (it takes ~30 s to settle from
  the first guess because Opus frames arrive in 20 ms bursts); encoding takes 233 us per frame on the Skylake (459 us
  worst). The receiver's underrun counter also counts the idle time before and after the stream, so compare it
  with the stream length. The ring level seen by the servo is the trough between bursts, so `target=` is the
  margin before an underrun, not the average latency (that is about a frame longer).
  **Multicast and IGMP:** `recv group=239.1.2.3` joins the group (an IGMPv2 report, sent twice), answers general and
  group-specific queries after a random delay (a v2 report for a v2 query, v3 for a v3 one), and sends a leave when
  the tape ends (`igmp.rs`); `send to=239.x.y.z` already worked. A new tape API call, `nic_set_multicast`, makes the
  e1000 family accept all multicast (RCTL.MPE; its multicast table is empty, so it dropped everything before; the
  RTL8168 already accepts all). The QEMU test injects a query and checks the report, the leave and a tone through a
  group. Hardware (Skylake to Sandy Bridge via the lab switch, 30 s): 1502 of 1502 packets, 0 lost; the lab switch
  sent no queries (0 answered).
  **Duplex:** `audio-net duplex` (options of both sides, `in=` for the capture channels to send and `out=` for the playback
  channels, both PCM and Opus, raw and RTP) sends the capture side and plays a received stream at once. The send and
  receive loops run unchanged as two cooperative tasks: `duplex.rs` switches stacks (a dozen instructions of
  assembly) inside `tape_rt::idle()` (a new idle hook), and gives the loops a copy of the tape API whose `should_abort`
  is sticky and whose `audio_start`/`audio_stop` merge the two directions into one device start. QEMU test: two
  guests exchange 440 and 880 Hz tones (QEMU's USB audio has no capture side, so the capture merge is hardware-only).
  Hardware (H6 on the Skylake and UFX on the Sandy Bridge sending to each other over RTP, 24-bit, 20 s): each side sent
  ~20,060 packets and received ~20,040 (7 lost), no transfer errors. A shell command line is limited to about 100
  characters, so a long option list is cut off (`secs=` then fails to parse): leave out options at their defaults.
  **AVTP (AVB layer 2):** `proto=avtp` sends and receives IEEE 1722 AAF (ethertype 0x22f0): the 24-byte AVTPDU header
  (`audio_wire::avtp`, host-tested) and big-endian 16/24-bit samples, the sample rate as the AAF `nsr` code, an 8-bit
  sequence number (the receiver widens it so its gap logic sees no false jump at the wrap), a stream id made of our MAC
  and a number, default destination 91:e0:f0:00:fe:01 (`to=MAC` overrides). The NIC is told to accept multicast so a
  listener hears a multicast stream address. fos has no gPTP clock and no MSRP stream reservation, so the presentation
  time is marked not valid (tv = 0) and the listener plays on arrival: this interoperates with AVTP listeners that
  accept that, not with a full Milan device. The header layout is written from the standard and checked only against
  our own parser and by eye (no AVB device or dissector at hand; tcpdump has none). QEMU test: header fields, 8-bit
  sequence stepping and a played tone; hardware (H6 on the Skylake to the UFX, 30 s, 1 ms packets): 30,019 of 30,019
  packets, 0 lost.
  **`record` streams to disk:** the take is written to the volume as it arrives, in 256 KiB blocks, so its length is
  limited by free space (and WAV's 4 GiB), not by RAM. A FOSFS file is one contiguous extent, so the tape reserves
  the space first (`fs_stream_open`: `SECONDS` worth, or the largest free run for an open-ended take),
  writes at sector-aligned offsets (`fs_stream_write`) and gives the unused part back at the end (`fs_stream_close`).
  The directory length and the WAV header move forward together every two seconds, so a power cut keeps the take up to
  that point with a consistent header (tested by killing QEMU). `record NAME 0` records until Escape or until the
  reservation is full; `diskN/NAME` and `@LABEL/NAME` pick another volume; `tone=HZ` records a test tone through the
  same path (QEMU's USB audio has no capture side). The capture ring is 16 MiB now (playback stays 1 MiB) so a stick
  that stalls while writing does not overrun the take at once. Tests: `make test-record` parses the image on the
  host (timed take exact, Escape take, power-cut take). Hardware: the H6 on the Skylake at 96 kHz, 6 channels
  (1.7 MB/s) filled the volume's 31 MiB in 19 s with no overruns; the UFX's 22 channels (3.2 MB/s) onto the Sandy
  Bridge's boot stick (Kingston DataTraveler on EHCI, 2.6 MB/s while writing) failed after ~21 MiB each time, a
  block taking ~5 s and then returning a disk error (which leaves the stick unusable until a hardware reset, not
  a KUP restart): that stick, or our EHCI write path, cannot sustain it. A real take needs a bigger, faster volume
  (the Skylake's SATA SSD has no FOSFS partition yet).
  **Configuration file and autostart:** `audio-net @audio.cfg` reads its options from a FOSFS file (`@diskN/name`
  works too) and the words after it override the file. A line is `key value` (becoming `key=value`) or ready-made
  words, `#` starts a comment, and the mode (`send`, `recv` or `duplex`) is a line of its own:

  ```
  # audio.cfg: send the UFX's channels 9-10 to the studio over RTP, Opus
  mode send
  proto rtp
  codec opus
  bitrate 96
  ip 192.168.1.20/24
  gw 192.168.1.1
  to 192.168.1.50
  ch 9-10
  ```

  **Choosing the NIC:** `nic=N` (one-based) or `nic=MAC` (or `nic 00:11:22:33:44:55` in the file) picks the network
  interface. The panel saves the MAC, so the setting follows the card if the NICs change places; a MAC no NIC has is an
  error when running, and the panel falls back to NIC 1.

  A boot `start` file containing `run audio-net @audio.cfg` brings the stream up with nobody at the machine. The
  tape supervises: unless Escape was pressed (or `secs=` ran out) a run that ends is an error that is retried every
  5 s (the peer is not up yet, the cable was pulled, the device was unplugged), so a headless machine converges on
  its own. A `term net` client can watch it and press Escape (the remote abort) to stop it. The first line says
  what is running, and the identity lines give the user what they need to troubleshoot: `Network: 192.168.1.20 on NIC
  1 (MAC xx:xx:...)` (or the MAC and "layer 2 only" for raw/AVTP) and `Sending to 192.168.1.50, MAC ...` once ARP has
  resolved the peer. The QEMU test boots a receiver from a `start` file and cfg, sends from a cfg, and checks the
  retry loop and that Escape ends it. The shell `write NAME text` writes a one-line file; longer ones come from `ed`
  or `devloop put`.
  **Announcements and the peer list:** every `audio-net` that streams over RTP announces itself on the SAP group
  (239.255.255.255:9875, RFC 2974) every 5 s, a sender with the SDP of its stream (`a=sendonly`), a receiver with an SDP
  of what it accepts (`a=recvonly`), under `name=` (default `fos-` and the last three MAC bytes), and withdraws the
  announcement (SAP's deletion flag) when it stops. `audio-net list [secs=N]` joins the group with IGMP, listens and prints
  a table: name, role (sends/listens), the announcer's IP and MAC, codec, channels, rate and address:port (marked when the
  address is this machine's). No address has to be typed for multicast: a sender with no `to=` (panel: Peer empty) picks
  239.192.x.y from a hash of its name, avoiding groups it hears in use (2 s); a receiver with `source=auto` (panel: Peer
  empty) joins the only announced multicast stream, or says that none or several are announced; a bad Group in the
  panel is refused with a message. A receiver can say `source=NAME`: it waits (15 s) for the sender's announcement and takes
  codec, channel count, rate, port and multicast group from the SDP, so the cfg of a receiver can be just `mode recv`,
  `ip ...` and `source studio-a`; a sender can say `to=NAME` and sends to the listener's address and port with the
  codec it announced. Names are looked up again on every retry. The SDP parser (`audio_wire::sap`, host-tested) reads
  L16/L24 and Opus sessions, so announcements from other AES67 gear that use these are listed too (not tested: no
  such device here). Raw Ethernet and AVTP have no IP and are not announced. QEMU test: list, `source=`, `to=` and the
  withdrawal; hardware: the Skylake's 'h6' listed on the Sandy Bridge, which received it by name with no other
  parameters, and the Skylake's list showed the Sandy Bridge as a listener.
  **Names on layer 2 (raw Ethernet and AVTP):** the same announcements work without IP. They travel as the same SAP packets
  (an SDP with `a=x-fos-l2:raw|avtp`) straight in Ethernet frames of ethertype `0x88bb` to the broadcast address, every 5 s, and a
  name then stands for the MAC the frame came from. With `proto=raw|avtp` given, `to=NAME` (a sender), `source=NAME` (a receiver)
  and `audio-net list` look names up there, a sender and a receiver announce themselves while they run, and the panel's presence and
  `P` picker work on those networks (the list shows the MAC and the kind of network instead of an IP address). A typed MAC address still
  works: `to=MAC`, `from=MAC` (`from=` takes only a MAC; a name goes in `source=`). Without `proto=`, a name still implies RTP. Test:
  `layer_2_names_test` and `layer_2_panel_test` in `test_audio_net.py`.
  **The settings panel:** `tape run audio-net` with no arguments shows a full-screen form (mode: send, receive or
  duplex; network: RTP over IP, raw Ethernet or AVTP; NIC with its MAC; IP address or `dhcp`, gateway, peer, codec
  preset, send and receive channels, name, multicast group) and the bandwidth the choice needs. Codecs are presets:
  PCM 24-bit, PCM 16-bit, Opus balanced (96 kbit/s per pair), low bandwidth (48) and low delay (128, 5 ms frames); Opus
  is offered only over RTP. Up/Down or Tab move, Left/Right or Enter change (Enter edits a text row), `S` starts, `P`
  listens for announcements and lets the user pick the peer by number (listeners for a sender, senders for a receiver),
  `W` saves the settings as `audio.cfg`, `B` writes (or removes) a `start` file `run audio-net @audio.cfg` so the machine
  streams at boot (it refuses to touch a start file with other commands), `Q` or Esc quits. The panel starts from an
  existing `audio.cfg`. While a stream runs the screen shows the sent/received/lost counters, the ring level against its
  target, the clock offset, the last message the stream printed (`tape_rt::set_quiet` keeps the worker's output off the
  screen and remembers the last line) and a peak meter per channel in 6 dB steps (up to 32 each way); Esc or Q stops and
  returns to the form. It works on the local screen and over `term net` (keys through `read_key`). The console learned
  `ESC [ row ; col H` for it (a kernel change); redraws touch only what changed, because the serial line is slow and a
  serial receiver has a 16-byte FIFO (an early version redrew the whole form per key and dropped typed characters).
  The QEMU test drives two panels with keystrokes over the serial line: edit the address and peer, start, check the
  counters and meters, Escape, `W`, and the saved cfg.
  **Dell Latitude E7270 (I219-LM `8086:156f`):** the NIC is in the e1000e family and works on the real machine (link, MSI,
  term net). (Fixed later, see below.) Its xHCI (Sunrise Point) timed out reading the device descriptor of SuperSpeed devices
  after a successful Address Device (ports 15 and 17, three attempts each), so a USB 3 boot stick plugged straight in is not
  found; through a USB 2.0 hub it enumerates and boots normally. High-speed devices (the UFX) work on the same
  controller. Not investigated further: there is no way to push a debug kernel to a machine that cannot see its boot stick.
  **Duplex by name:** two machines in duplex that know each other only by name used to deadlock, since a machine
  announces its stream only once it runs and each waited for the other's announcement before starting. Now a
  machine in receive or duplex mode announces where it listens (`a=recvonly`) while it looks, so the peer can start sending; a
  duplex machine looks up the peer's `source=` only after its sending half is streaming (the receiving half is a task
  that retries until found), so there is no cycle, and either machine may start first. Frames from our own address are
  ignored (virtual networks echo them). Tested with two QEMU guests, the first started eight seconds before the second.
  **Presence: peers by name before any stream:** with the panel open on the form, a machine is announced (its name,
  address and the settings it would listen with, marked `a=x-fos-idle`) and it keeps a live directory of every other
  announcement, so `P` shows the machines on the network at once (name, IP, MAC, and whether each is idle, sending or
  listening) and the user picks one by number: nobody needs to know a name in advance. The panel's header and Name row
  say what this machine is called (`fos-` and the end of its MAC unless a name is typed). Presence never blocks the
  panel: DHCP is stepped from the idle loop (`net::DhcpTask`), it starts a second after the last change so that typing
  does not thrash it, it is withdrawn when a stream starts (the stream announces itself, keeping the DHCP lease) and when
  the panel closes, and entries expire if not heard for 20 s. It needs the RTP network (the announcements are IP
  multicast). Duplex then works from the pick: both panels open, each picks the other, each presses `S`.
  **Hostname in DHCP:** every audio-net DHCP request carries the machine's name (option 12): the `hostname` file's text plus
  the one-based NIC number (`myhost1`), or `fos2`, the same name discovery announces, so the DHCP/DNS server knows the
  machine by name. The tape reads it through the `hostname(nic, buf, cap)` call at the end of `TapeApi`;
  `tape-net`'s `DhcpClient::set_host_name` adds the option (v1 tapes using the client send none until they call it).
  **`audio-net serve` (Ogg Opus over HTTP):** a test aid for listening from a browser or VLC (a phone, say) when an RTP
  receiver is out of reach. `serve` takes the same source (`src=capture` or `tone=`), `ch=` (at most two channels),
  `bitrate=` and `frame=` as an Opus `send`, takes an address like RTP (`ip=` or `ip=dhcp`) and listens on `http=PORT`
  (default 8000): `GET /live.opus` is an endless `audio/ogg` response with one Opus packet per Ogg page (RFC 7845; the
  muxer is `audio_wire::ogg`, host-tested), `GET /` a page with an `<audio>` player, anything else a 404. The machine
  answers ARP and ping. The TCP is `tape-net`'s (one connection, one segment in flight), so the throughput is one segment
  per round trip; audio waits in a 12 KiB queue and a page that finds it full is dropped (the player conceals it) rather
  than letting the delay grow, a client that stops acknowledging for 10 s is dropped, and a new connection replaces the current one (players open a second one). `prebuffer=MS` (a panel row "Pre-buffer", off by default) sends the last MS of encoded audio to a new client at once so its player starts with a full buffer, at the price of that much delay. Up to three clients stream at once, each with its own queue and Ogg stream (a phone's VLC opens a second connection while the first plays), and the live view's second line summarises them ("1 streaming, 3 connection(s) so far; last: slot 1 gone after 4 s (closed)"). The per-client details (request, Range, User-Agent, why it ended) are printed, but the panel runs quiet, so they reach only the message line; run `serve` from a `term net` session to see them all. The kernel no longer sends the class 1.0 "set sample rate" request (and no longer warns) when the alternate setting offers exactly the one rate asked for, as on a PCM2902. Safari plays Ogg Opus only in
  recent versions; VLC does. The panel offers it as a fourth Mode (the Network row is fixed to IP, peer/name/group/receive rows disappear, Codec offers the Opus choices, an "HTTP port" row defaults to 8000, `audio.cfg` gets `mode serve` and `http N`); its live view names the URL once the address is known and shows the same meters. Test: `make test-audio-serve` (QEMU user networking with a forwarded port; checks every
  page's CRC, the headers, the granule positions, `ffprobe`, and that `ffmpeg` decodes 440 Hz).
  **Real PPM meters:** the live view shows one horizontal meter per channel (labelled with the device input or output
  it is), one cell per dB from -60 dBFS to full scale, on the Nordic scale (IEC 60268-10 type I: TEST = 0 at -18 dBFS,
  maximum permitted level +9 at -9 dBFS, digital full scale at +18) with a number per channel to a tenth of a dB, a
  three-second peak-hold marker, a latched CLIP flag for samples at full scale, `#` above the permitted maximum, and
  `U` to switch the scale and numbers to dBFS. The detector (`audio_wire::meter`, host-tested: a 1 kHz line-up tone at -18
  dBFS reads TEST, a 5 ms burst reads about 2 dB low, the fall is 20 dB in 1.7 s, also at 96 kHz) is a quasi-peak
  integrator that costs one multiply per sample; the panel reads the highest reading since its last look so a short peak
  between redraws still shows. Pages of 12 channels (`N`/`B`) keep it inside a 24-row terminal. Cost control: the drawing
  is spread over many short calls (three rows at a time, only the cells that changed), the counters line shows the
  longest the drawing has held the stream loop up ("UI stall"), and `M` switches the meters off, which also stops the
  per-sample detector work.
  **Whose codec is used: the sender's.** The Codec row is what this machine *sends*. A receiver that picked its sender by
  name (`source=NAME`, which the panel's Peer row sets for Receive and Duplex) follows what the sender announced: codec,
  depth, rate, channels, port, multicast group; its own Codec setting does not matter then. A sender that looks its peer up
  (`to=NAME`) takes only the address and port from the peer's announcement and keeps its own codec; if the peer announced
  that it listens for something else it prints a note (a receiver that is only waiting on a port cannot follow). In duplex
  each direction is therefore its sender's setting. With a plain IP address or MAC nothing is adopted. (An earlier version
  had the sender follow the receiver's announced setting, which surprised the first users: a sender set to PCM 24 sent
  Opus because the other machine's form still said Opus. The live view's second line shows what is actually sent and
  received, "(peer's choice)" marking a codec taken from a peer, because the form shows only this machine's choice.)
  **Screen cost, measured on hardware:** the first readings of the panel's "UI stall" were 5 ms (laptop) and 20 ms (Skylake).
  The cause: a tape's `putc` prints one byte per call, and the framebuffer console repaints its whole text grid for every
  byte (a 1080p screen is about 16,000 cells), so a few hundred bytes of meter drawing cost milliseconds each time. Tape API
  version 2 adds `puts(text, length)`, a whole string in one console call (one repaint, one relay frame); `tape_rt`'s
  `print!` uses it when the kernel is version 2 or later (it falls back to `putc` otherwise, so a new tape still runs on an
  old kernel), and the panel draws through a frame buffer presented once per drawing step. In QEMU the stall fell from 24 ms
  to 4 ms. The counters line now shows `UI recent/worst ms` (the recent figure covers the last ten seconds) and the
  device's playback underruns (`Un`) and capture overruns (`Ov`), so a dropout can be matched to the drawing.
  **The shell's line editor (`lineedit.rs`):** the prompt now edits like a normal one. Insert and delete anywhere in the line
  (Left/Right, Home/End, Delete, Backspace), Ctrl-A/E (start/end), Ctrl-K (delete to the end), Ctrl-U (delete to the
  start), Ctrl-W (delete a word), Ctrl-L (clear the screen), Ctrl-C (abandon the line), and a history of the last sixteen
  commands on Up/Down (no empty entries, no repeats of the previous one; the half-typed line comes back after browsing).
  It serves the local keyboard (arrow keys arrive as single private codes) and `term net` or a serial terminal (ANSI
  sequences, `ESC [ A` and `ESC O A` forms, assembled by a small state machine). Redrawing uses carriage return, cursor
  left/right and erase-to-end-of-line, which the framebuffer console and every terminal do; a change in the middle of the
  line redraws the line from the prompt, and typing at the end just echoes. Tab completion is not done. Tested over the
  serial line (`make test-lineedit`). The history is not kept across reboots.
  **Scripts and bare command names:** a word that is not built in runs `NAME.sh` (or `NAME` as a tape): `NAME.sh` is a file
  of commands, `#` starts a comment, blank lines are skipped, `$1`..`$9` are the arguments and `$*` all of them, each
  command is echoed as `NAME.sh> command`, and scripts may call scripts four deep. Both are looked up on the boot volume,
  then on the other disks (a file with no dot in its name is taken for a tape; a built-in command of the same name always
  wins). The `start` file can use them. Scripts nest shell commands, which have large stack frames (the command function
  holds several 4 KiB buffers), so the kernel stack grew from 64 to 256 KiB; the three-deep recursion test had overflowed
  the old one with a double fault. The same overflow was already crashing `tapes/test_bridge_e2e.py` (`nic recv` inside a
  nested command) on older commits; with the bigger stack that test no longer crashes but still does not see the bridged frame
  (not investigated yet). Tested by `make test-scripts`.
  **`nic` subcommands:** `nic sniff [N]` puts an interface in promiscuous mode and prints one line per frame (addresses,
  then ARP who-has/is-at, IPv4 with TCP/UDP ports and tags for DHCP, DNS, SAP, RTP, AVTP, audio-net, LLDP, fos frames) until
  Escape, at most 40 lines per pass so a busy network does not make the screen the bottleneck (it says if it skipped);
  the terminal's own frames are handled, not shown, and Escape also works from a `term net` client. `nic discover` probes
  every interface for three seconds and lists the fos machines that answer with their host names (`nic peers` shows the
  table again), `nic map` sends a probe from every interface with a link and shows which of this machine's other
  interfaces hear it (cabling; up to three rounds while links come up), `nic use N` picks the interface the others
  default to. `nic regs`, `nic send` and `nic recv` were already there. Tested over the serial line with injected
  frames, a second guest and a guest with two NICs on a QEMU hub (`make test-nic`).
  **NETFS (files between machines):** the v1 wire format on the chat ethertype (tags 0x30-0x39), so `tools/foscopy.py`
  works on v2 machines unchanged: `foscopy.py note.txt MAC:disk1/note.txt`, `foscopy.py :disk1/start ./start-copy --mac MAC`.
  A machine serves GET and PUT at its prompt and in `term net`, never while a tape runs (the staging buffer is the tape's)
  and not when `settings` says `deny-term-net`; files up to 4 MiB; both directions carry the RFC 1071 checksum, verified
  before anything is stored, and the machine answers a refusal with an error code (not found, no such disk, too large or
  empty, bad name, directory or data area full, disk error, checksum mismatch, busy); a gap aborts (no retransmission).
  The shell side is `copy HOST[:diskN]/NAME DST [NEWNAME]` (fetch) and `copy SRC NAME HOST[:diskN] [NEWNAME]` (store), HOST
  being a name from `nic peers` (a host name carries its NIC number: `alpha1`; discovery runs if the name is not known), a
  MAC address or a peer number. Data frames go out one a millisecond, a pace real hardware keeps up with. `send` can now
  carry a full Ethernet frame (it panicked on anything over 1024 bytes). Not done: `chat`, `tape load HOST NAME`,
  `tape send`. Tested (`make test-netfs`): foscopy both ways with a 37-chunk round trip compared byte for byte,
  refusals, and two guests copying to and from each other.
  **RTL8139 driver (`rtl8139.rs`):** ported from v1 (`10ec:8139`), I/O-port BAR0, one 32 KiB receive ring with `WRAP` and four
  transmit slots, all DMA below 4 GiB, polling with interrupts only to wake the CPU. The v1 hardware lessons are kept and
  listed at the top of the file: `CR.TE|RE` before `RCR`/`TCR`, the standard interframe gap, bad-status frames stepped over,
  and a full software reset for a nonsensical ring length, a link down-to-up transition (`MSR.LINKB`), receive overflow or a
  transmit abort. The card has no MSI, so legacy INTx through the I/O APIC is tried (pins probed) and kept only if a self-test
  interrupt arrives; the card has no "interrupt now" command, so the self-test uses its timer (`TimerInt`/`TCTR`, `ISR.TimeOut`).
  Differences from v1: `set_multicast` programs the `MAR` hash filter (v1 left it zero, which dropped multicast frames), and
  promiscuous mode resets and reprograms the card instead of editing `RCR` in place, because QEMU restarts the receive ring on
  any `RCR` write. Tested with `make test-rtl8139` (QEMU `-device rtl8139` as the only NIC: INTx self-test, discovery both
  ways against an e1000 guest, promiscuous sniff of a frame for another MAC, NETFS round trips up to 300 KB). Link-recovery,
  overflow and the real chip's `RCR` behaviour are not covered by QEMU.
  **3Com 3C905 driver (`threecom.rs`):** ported from v1 (3C905, 3C905B, 3C905C: `10b7:9050` to `10b7:9200`), I/O-port BAR1 (or
  BAR0), a 64-descriptor upload ring and one download descriptor, all DMA below 4 GiB, polling with interrupts only to wake
  the CPU. The v1 hardware lessons are kept and written at the top of the file: every command-register write waits for
  `CmdInProgress`, the upload engine is un-stalled after the list is posted and after each frame, and an event must be
  enabled in both the status and interrupt masks to raise INT#. The card has no MSI, so legacy INTx through the I/O APIC is
  tried (pins probed) and kept only if the card's `RequestInterrupt` self-test arrives. QEMU has no such card, so nothing
  automated covers it: it needs the real card (a 3C905C-TX is in the Skylake).
  Confirmed on the real card (3C905C-TX in the Skylake, link up, INTx pin 11 delivering): `nic map` sees it hear the I350
  on the same switch, and NETFS round trips of 300 KB and 2 MB through it (foscopy both ways, compared with `cmp`) are
  identical with 0 errors. Two bugs surfaced that QEMU could not: (1) the transmit frame header asked for a `TxIndicate`
  status entry per frame, which pushes an entry on the card's 31-deep TxStatus stack that nothing popped, so after about 30
  frames the stack overflowed and the transmitter stopped (the sender saw the download never complete and lost frames
  from chunk 29 on; v1 carried the same latent bug, hidden by short transfers). The bit is gone, and `send` empties the
  TxStatus stack and re-enables the transmitter if the card reports `TxComplete` (error entries: jabber and underrun get a
  transmit reset first). (2) `send` also waits (bounded, 50 ms) for the download list pointer to clear instead of dropping
  a frame when the card is still busy, and `nic` shows the driver's error counters (host errors, send timeouts, drops,
  transmit errors).
  A third bug was not the card's: `internet_checksum` summed into 32 bits, which overflows on about 260 KB of high-entropy
  data, so large NETFS files were rejected as "damaged" (KUP's mostly-zero kernel images never got near it); it uses 64 bits now.
  **Fixed (2026-10-05): the E7270's SuperSpeed enumeration failure was a driver bug, not the controller.** A SuperSpeed device
  reports `bMaxPacketSize0` as an exponent (always 9, meaning 2^9 = 512 bytes), and `probe_device` took the 9 as a size: it
  re-programmed the default pipe to 9 bytes with Evaluate Context, so every later control transfer timed out ("read the full
  device descriptor: Timeout"). SuperSpeed devices were never working in v2; the Skylake desktop's sticks sat on USB 2 ports.
  Found with a USB 3 stick in the laptop's side port while the boot stick stayed on the USB 2 hub (so kernels could still
  be pushed): the stick now enumerates as `USB storage 0951:1666 on port 13 ... (SuperSpeed)` and its GPT is read.
  **The E7270's second SuperSpeed port (port 17) was an internal device that needs a port reset:** a Dell WWAN modem
  (`413c:81b6`, class 0). It trained its link on its own and then ignored SET_ADDRESS (command completion code 4, a
  transaction error) until the port was reset; the enumeration code leaves a SuperSpeed port whose link is already up alone,
  which works for most devices. `open_port` now resets the port (a hot reset) and tries once more when Address Device fails on
  a SuperSpeed port; after that the modem enumerates. A device nothing uses is now named in the boot log (`USB: port 17:
  413c:81b6 (device class 0x00, SuperSpeed): nothing here uses it.`; the other internal devices on this laptop show as a
  Realtek camera `0bda:5686`, a Broadcom fingerprint reader `0a5c:5832` and an Intel Bluetooth `8087:0a2b`). The hidden
  command `usb try PORT [hot|warm]` resets a port by hand and enumerates it once, printing where it stopped, which is how
  this was found.
  Plan: a Wireshark
  capture or a real AVB device to confirm the AAF header; recording onto the Skylake's SSD. The UFX's 12-channel capture alternate is
  not needed: the first alternate (22 channels) works at 48 and 96 kHz, which is what `find_alt` takes. (Dante itself
  is proprietary; AES67 mode is the open way in.)
* **Every stick gets its own GUIDs.** The kernel finds the disk it booted from by the payload
  partition's GUID, so two sticks written from one image would be indistinguishable.
  `write_stick.py` therefore rewrites the disk, payload and FOSFS GUIDs (and both GPT copies with
  their CRCs) with random ones as it writes (`--keep-guids` to opt out; plain `dd` of the image
  keeps the shared GUIDs). Tests: `make test-disks`, `make test-stick-guids`.

  **Delay** (2026-10): the receiver's summary and the panel's `Delay ~N ms` give one packet plus the playback ring
  level (counters it keeps anyway). `send latency` is a loopback test aid: a click every 0.5 s, timed on the capture
  clock until it returns on the input the receiver's output is cabled to. Measured laptop (USB) <- Skylake (USB), 12 ms
  target: 20.6-24 ms click to click, i.e. the ring plus about 8 ms for packet, USB buffers and capture. It grows slowly
  as the servo settles the ring.

**Files on other disks.** Every command that takes a file accepts `NAME` (the boot volume), `diskN NAME` and `diskN/NAME`
(also `@LABEL NAME`, `@LABEL/NAME`), as v1 does: `cat disk2/audio.cfg`, `copy disk2/a.cfg disk1/b.cfg`,
`copy disk2/a.cfg disk1 b.cfg`, `write disk2/note hi`, `rm disk2/note`, `tape load disk2/audio-net`, `tape save disk2/x`,
`disk2/NAME args` (run a tape or script from that disk). The boot-time files (`start`, `hostname`, `settings`, `debug`)
and a bare name that a tape reads (`audio-net @audio.cfg`, `fs_read`) and a bare command word are searched on the boot
volume first, then on every other disk in order (`storage::volume_with_file`); writes with a bare name stay on the boot volume.

**Tapes between machines.** `tape load HOST[:diskN] NAME` fetches a tape from another machine's disk (NETFS GET into the
staging buffer, then loaded as usual); `tape save HOST[:diskN] NAME` stores the loaded tape there (NETFS PUT);
`tape send [diskN] NAME PEER` sends a file from a local disk as a tape to a machine waiting in `tape load net` (the
v1 START/DATA/STOP frames, one chunk per 3 ms, no acknowledgement). HOST/PEER is a name or number from `nic peers`, or a
MAC. Not done: `tape run @HOST/NAME` (a startup-argument file on another machine; tapes read their own `@` files through
`fs_read`, which only knows local volumes). Test: `make test-netfs`.

## 9. Tape ABI v2

v1's tape ABI is a cdecl call with 43 appended arguments. v2 replaces it with
**one pointer to a versioned table**:

```rust
#[repr(C)] pub struct TapeApi {
    pub version: u32, pub size: u32,       // size lets old tapes ignore new fields
    pub putc: extern "C" fn(u32),
    pub should_abort: extern "C" fn() -> u32,
    pub alloc: extern "C" fn(usize) -> usize,     // one allocator, no pools
    pub free: extern "C" fn(usize, usize),
    pub args: *const u8, pub args_len: usize,
    /* ... keys, NICs, clocks, filesystem, audio ... */
}
extern "C" fn tape_main(api: &TapeApi);     // SysV64 ABI
```

- Tapes are ELF64 `ET_EXEC` images at a fixed load address inside the kernel's
  identity map (the same "fixed address, no PIC needed" idea as v1).
- Append-only still holds: fields are only ever added at the end and `size` tells
  a tape what exists. No more declaring 38 unused parameters to reach one.
- Large static buffers still go in `.bss`/`alloc`, never the stack.
- `memtest` loses its 64-bit excursion, its two pools, `alloc_frames` and
  `e820_entry` games: a 64-bit tape just allocates RAM and tests it, and the
  memory map is a field of the table.
- The shared crates (`tape-rt`, `tape-net`) are widened to `usize` and built for
  the 64-bit target. The protocol code in `tape-net` is already pure Rust and
  moves over almost unchanged.

### NIC interrupts (MSI)

NICs take **MSI** where the device has it, and fall back to polling where it does not, or
where the `no-msi` settings option or a failed self-test says so. `v2/kernel/src/msi.rs` hands
out vectors `0x50..0x5f`; `pci.rs` programs the capability (32- or 64-bit message address,
one vector) and turns the INTx pin off; the e1000 driver unmasks receive, ring-low, overrun
and link causes, sets an interrupt throttle (so a small-frame flood cannot mean a million
interrupts a second), and **proves delivery** by raising a link-status interrupt through `ICS`
and waiting for the message. The handler only reads `ICR` (which acknowledges) and counts;
frames are still drained by the foreground. When every NIC has an interrupt, `wait_for_event`
is a real `hlt` (an idle tape used 1% of a host core under QEMU, against 100% when spinning).
The `power [N]` shell command shows where the CPU's time went (`cpu::sleep` counts time in `hlt` and wake-ups; `cpu::spun`
counts the 100 us polling slices per reason: polled NIC, UART, serial output; the rest is "running"). It reports since boot and
since the previous `power`, so run it after a tape ends to see what the tape did; `power N` samples N seconds at the prompt.
It also lists the five busiest interrupt vectors (a storm shows here: the 3C905's floating GSI 11 pin fired 125000 times a
second on the Skylake board; INTx self-tests now count only interrupts the card claimed, and an unclaimed-interrupt guard masks
a pin that fires 200 times a tick), and on Intel CPUs the temperature, package watts (RAPL) and average speed (APERF/MPERF).
**Frequency scaling** is `power hwp [performance|balanced|power]` or `hwp balanced` in `settings`: it turns on Intel hardware
P-states, so the CPU ramps itself with load (a busy tape keeps full speed, no kernel policy). Off by default; it stays on until
reset and needs real hardware. laptop1: awake speed 31% of base at the idle prompt, 104% under a spinning tape (power
preference).
**Parking idle NICs** (`kernel/src/nicpark.rs`): `nic park N` / `nic unpark N` power a link-down interface's copper PHY down
(MII register 0, bit 11; e1000 classic and I350 through MDIC, RTL8139, RTL8168; not e1000e, which shares its PHY with the
management engine, and not the 3C905). A parked PHY cannot see a cable, so it wakes every 30 s for 8 s and stays up if the
link comes. `park-idle` in `settings` parks every NIC whose link has been down for a minute. Verified on skylake3's i350
(power-down bit read back, wake and unpark work); the watts saved are below what RAPL shows. The ptp tape sleeps between polls as a master by default and spins as a slave (`idle`/`spin` override).
Delivery is tried in order: **MSI-X** (only with the `prefer-msix` setting, and only on the
82574, where `IVAR` maps causes to a vector; a test hook, since MSI is enough for every part
here), **MSI**, then **legacy INTx through the I/O APIC**. INTx needs the interrupt's pin, which
ACPI's `_PRT` normally supplies and the kernel cannot read (no AML interpreter), so the driver
*probes*: it routes candidate pins (the firmware's interrupt-line hint, then GSI 16-23) to a
vector one at a time, makes the card raise an interrupt on purpose and keeps the pin where it
arrives. INTx handlers share a line (every handler on a vector runs, each clears its own
cause) and a per-tick storm guard masks a pin that floods. Whatever path is used must pass the
self-test, otherwise the NIC stays polled.

The e1000 driver covers three families by register table (`Regs`): the classic 82540EM,
the **e1000e** family (82574L, 82579V, I219-LM) with Linux's stricter reset sequence and the
TCTL values v1 found on a real I219-LM, and the **I350** (the Skylake's four-port card: moved
registers, explicit queue enables, the `SWSM`/`SW_FW_SYNC` PHY handshake, an MAC that is only
readable some ticks after reset). I350 and I219 are ports of v1 code that was verified on the
Skylake; QEMU has neither, so in v2 they are unrun until that machine is reachable.

## 10. What ports how

Rough size of the v1 kernel and how much changes (lines of kernel Rust):

| Area | Lines (v1) | v2 work |
| --- | --- | --- |
| shell, fosfs, console logic, term net, KUP, NETFS | ~12 K | mostly widen types; console gets a framebuffer backend |
| NIC drivers (e1000, e1000e, I350, RTL8139/8168, 3C905, NE2000) | ~6 K | widen DMA addresses; `allocate_below_4g` for 32-bit-DMA parts; MSI everywhere |
| USB (UHCI, EHCI, xHCI, usb_storage), AHCI | ~5 K | widen; boot-time bring-up |
| audio (HDA, ES1370, AC'97, USB audio) | ~4 K | widen; late, lower priority |
| pmem, paging, gdt, interrupts, lapic, arch, entry.S | ~3.5 K | **rewritten** for 64-bit; no real-mode trampoline |
| disk.rs, boot_info, debug, reboot | ~2.5 K | disk loses the BIOS backend; boot_info v4; reboot via firmware |

Things that **disappear**: the real-mode disk trampoline and its PIC/IVT/paging
dance, A20 handling, `stage1/stage2` assembly, the low-memory map, the 16/32-bit
GDT juggling, the 4-slot `map_mmio` pool, the `longmode` shim, and every "keep it
under `0x9f000`" rule.

Things that **stay**: FOSFS v3 on disk, the A/B slot format, KUP/NETFS/`term net`
wire formats, the shell command surface, the narrated-boot philosophy, the
debug/watchdog/crash-record machinery, `devloop`.

## 11. Milestones (each ends with something runnable and a test)

| # | Milestone | Done when |
| --- | --- | --- |
| M0 (done) | Loader skeleton | OVMF boots `BOOTX64.EFI`; it narrates to serial + firmware console, prints the memory map, GOP mode and RSDP. QEMU smoke test. |
| M1 (done) | Kernel hello | Loader loads an ELF64 kernel from a raw slot and exits boot services; kernel switches to its own GDT/page tables and prints through serial + framebuffer. |
| M2 (done) | Memory and interrupts | Physical allocator, identity map with NX/guard pages, IDT, LAPIC timer tick, faults reported legibly. |
| M3 (done) | Shell and network | PCI scan, one NIC (e1000 under QEMU), `term net` session working from boot. This is the first point `devloop` can drive v2. |
| M3b (done) | Local keyboard | PS/2 (8042 + I/O APIC IRQ 1) and USB HID boot keyboards on xHCI root ports and on EHCI (directly, or behind a hub through its transaction translator with split transactions) deliver keys to the shell; tested in QEMU (`make test-keyboard`: PS/2, `usb-kbd` on `qemu-xhci` and on `usb-ehci`, and a boot with no keyboard at all). **Not covered:** the split-transaction path (QEMU has no TT; needs the Sandy Bridge), keyboards behind hubs on xHCI, full/low-speed devices wired to an EHCI root port (companion UHCI/OHCI, no driver), hot-plug other than `usb rescan`. Verify on the Skylake and Sandy Bridge once reachable. |
| M4 (done) | Storage | AHCI, and USB Mass Storage (Bulk-Only) over xHCI and EHCI, behind one block layer; boot-disk discovery by the payload partition GUID the loader passes (BootInfo v5); FOSFS v3 read/write (byte-compatible with v1); `ls`/`cat`/`write`/`rm`/`disks`; `hostname`, `settings` (`deny-term-net`), `debug` (`reboot-on-crash`, `reboot-delay`, `crash-on-boot`) and `start` honoured. Tested in QEMU: boot from SATA (`make test`), from a USB stick on `qemu-xhci` and on `usb-ehci` (`make test-usb-boot`), configuration files and crash-reset (`make test-config`). **Added later:** NVMe (`nvme.rs`: polling, one I/O queue, 512-byte namespace 1, boot from it tested in QEMU) and hubs on xHCI (`usb_hub.rs`: route strings, hub slot contexts, transaction-translator fields; keyboards, sticks and nested hubs tested on `qemu-xhci`, `make test-hub`). **Confirmed on the Skylake (2026-10-03):** a 4-port USB 2 hub (1a40:0101) on a root port with a low-speed keyboard behind it, plugged in after boot and found by `usb rescan` (the machine would not POST with the hub attached, so it cannot be present at boot there). **Not yet:** USB sticks behind an EHCI hub's transaction translator beyond the Sandy Bridge case (the split path is hardware-only), NVMe on real hardware, NVMe interrupts and multiple queues.
| M5 (done) | Tapes | `TapeApi` v1 (one versioned table, `v2/abi/src/tape.rs`), tapes as ELF64 position-independent executables (relocated into a 4 MiB executable region, run on a stack of their own), `tape load NAME\|net` / `run` / `save` (the v1 `tape load net` protocol, so `send_tape.py` works), multi-NIC (up to 4 e1000, per-interface terminal session, promiscuous mode), and four tapes: `hello`, `calc`, `memtest` (memory + soak, no pools or long-mode excursion) and `nettest`. Tests: `make test-tapes`, `make test-nettest`. **Deviations from the sketch above:** tapes are PIE, not fixed-address (a fixed address may not be free RAM on a given firmware); `fs_remove`, `free_pages` and `nic_set_promiscuous` were appended to the table. **Not yet:** `tape-net` (needed by `router`/`bridge`/`ip-chat`, M8), audio (the network soak, `memtest soak net=A,B`, is back and tested over a QEMU hub cable), NIC interrupts (now done, see "NIC interrupts (MSI)" below).
| M6 (done) | Update and recovery | The loader runs the A/B slot state machine (one try for a pending kernel, automatic fallback) and tracks `running`/`crash_count` in the metadata sector; the kernel confirms a pending slot at its milestone; KUP over the network (v1 wire format, `tools/send_kernel_update.py` unchanged) writes the inactive slot and restarts; crash record in RAM (page `0x5000`), read back and printed by the next boot; reboot-on-crash through UEFI `ResetSystem(EfiResetWarm)`; crash-loop guard (five pre-milestone crashes in a row: the kernel stops resetting and halts); software watchdog (`watchdog [N]`, vector 240, longer limit while a tape runs, stays off after three unclean boots; a boot that runs five minutes clears the streak either way, so the watchdog returns on the next boot); `debug status\|slots\|crash\|hang`, `reboot`, `reset`. Test: `make test-debug-mode` (crash + record, bad and good KUP, fallback after a pushed kernel crashes, watchdog, loop guard, KUP disabled). **Not done:** the NVRAM copy of the crash record (UEFI `SetVariable` from a fault handler is risky; warm reset is the mitigation for RAM-wiping resets, unverified on real hardware), KUP from inside a running tape is accepted but untested, and nothing here has run on the Skylake.
| M7 (running) | Skylake | **Done on the Skylake and the Sandy Bridge (2026-10-03):** UEFI boot from a stick through xHCI (Skylake) and EHCI behind the rate-matching hub (Sandy Bridge); boot-disk discovery, FOSFS, KUP over the network with promotion, `reboot`; five NICs on the Skylake (I219-LM + 4x I350) all on MSI, the 82579V on MSI; tapes run; `memtest` over all 15.9 GiB (addr+moving+random) passes in 130 s; `nettest` through the cabled I350 ports forwards 71,851 of 71,862 generated 1500-byte frames at 900 Mbit/s with 0 lost (the rest were in flight at stop). Real-hardware bugs found and fixed: a fifth NIC overflowed the interrupt tables; the first control transfer after a port reset failed on Intel xHCI (retries, 8-byte default pipe for low/full speed); the PCH EHCI halted with a host system error until its DMA frame was in low memory; the UEFI `ResetSystem` call page-faulted on the Skylake (reset now uses the ACPI reset register / port 0xCF9 first); the UEFI Shutdown call faulted too (`shutdown` now enters ACPI S5 from the DSDT `_S5_` object first); a router started with a dead uplink refused to start because the e1000 refused frames with no link (now dropped), and its late DHCP lease was never promoted to the active uplink (fixed in both routers); `dmesg` took minutes because the framebuffer was scrolled by reading write-combining memory. Also confirmed on the Skylake: `shutdown`, the watchdog (`debug hang`), a USB keyboard, `router` with NAT and a DHCP server (internet through it, also after a late uplink lease). **Not yet tried on hardware:** the crash-loop guard, `bridge`, hubs, NVMe, audio, the I219 under sustained load.
| M8 | The rest | Remaining NIC drivers, audio, `router`/`bridge`, USB HID keyboard, polish. |
| M9 (done) | Recorder, FAT and the clock | `v2/tapes/recorder`: a field recorder for a USB audio interface -- meters from the moment the tape starts, a pre-roll buffer, takes by hand or by a sound/silence trigger, channel selection, WAV streamed to disk (RF64 past 4 GiB), a capture buffer of all spare RAM (several blocks, above 4 GiB) so a slow disk or a stall costs no audio, a disk speed test and slow-disk/backlog/audio-lost warnings. Disks: FOSFS, FAT32 and exFAT (`v2/fat`, root directory only, one contiguous run per file; exFAT can be formatted with `format diskN exfat`), chosen and formatted from the UI. Also: CMOS RTC clock (`date`, local time), `grow diskN` (FOSFS volume fills its disk), `ls`/`cat`/`write`/`rm`/`copy` on a named FAT disk, the shared libraries `tape-panel` and `audio-rec`. Tape ABI additions: `fs_stream_*`, `audio_select`, `disk_info`, `disk_format_exfat`, `audio_buffer`, `audio_set_capture_ring`. Full detail, limits and tests: `docs/recorder.md`. **Confirmed on the laptop (22-channel UFX to a USB stick and to exFAT):** pre-roll, trigger, channel picker, disk menu, format, grow. **Not yet:** exFAT mounted by a PC OS beyond what the user tried, a ring take that crosses a block boundary (hours at low channel counts; the logic has a boot self-test), non-blocking USB writes (a stalling stick freezes the recorder's UI, never the audio). |

Testing uses QEMU with OVMF (`OVMF_CODE_4M.fd` + a per-run copy of
`OVMF_VARS_4M.fd`, both present in this environment) and the serial console the
whole project already tests through. The v1 harness's habits carry over: bounded
waits, preserve serial output on failure, wait for the prompt before typing.

## 12. Compatibility and branching

- v1 (the current top-level tree) is maintained for the Pentium 4 and as the
  protected-mode teaching edition. Bug fixes land there; features may or may not.
- v2 develops under `v2/` on `main`. The two editions share the repository, the
  host tools and the docs; v1 remains the default build (`make` builds v1,
  `make ...` targets build v2) until v2 is good enough to promote.
- Wire and disk formats are shared, so one set of host tools drives both.
- The README and `AGENTS.md` grow a "which edition" paragraph; v2 gets its own
  `AGENTS.md` section kept small, with detail in `docs/`.

## 13. Risks and open questions

1. **UEFI quirks on real firmware.** OVMF is forgiving. The Skylake's firmware
   will have its own idea of the memory map, ExitBootServices retries, GOP
   formats and runtime-region handling. Mitigation: M7 early, narrated loader,
   a serial-less fallback of "print on the framebuffer".
2. **Local input.** No PS/2 on some boards means a USB keyboard stack, and a local
   keyboard is a requirement, so USB HID (interrupt-IN transfers on xHCI and EHCI,
   early enumeration) is on the critical path and scheduled as M3b.
3. **Boot-disk discovery.** Needs the native storage drivers working at boot, with
   the xHCI/EHCI handoff done correctly the first time. This is the part of v1's
   USB story (BIOS legacy emulation, takeover order, slow enumeration) that gets
   *more* important, not less.
4. **Framebuffer speed.** An uncached framebuffer makes scrolling crawl; needs PAT
   write-combining or a dirty-rectangle shadow buffer. Measure on the Skylake.
5. **IRQ routing without the PIC.** Legacy-INTx devices need the I/O APIC and MADT
   interrupt-source overrides. A fallback is to keep using the PIC where the
   firmware still provides it, but that is a hardware-by-hardware gamble; the NICs
   we care about already use MSI.
6. **ACPI scope creep.** Hold the line at "tables only, no AML". Anything that
   needs AML (some power/thermal/hotplug) is out of scope.
7. **Effort.** Roughly a third of the v1 kernel is rewritten and the rest is a
   mechanical-but-large widening. The milestones keep it shippable at every step;
   nothing needs the whole port finished before it is useful.
8. **Open decision**: do we want the loader to also provide a tiny UEFI shell-like
   fallback (boot the other slot by key press) for a bad kernel? Probably yes, but
   after M6.
9. ~~`v2/` directory or a sibling repository?~~ **Decided**: a `v2/` tree in this
   repository.

## 14. First steps (what to do next)

1. ~~Settle layout/branching~~ (decided: `v2/` in this repo).
2. Build M0 on this machine: a `x86_64-unknown-uefi` loader that prints the
   memory map under OVMF, plus the Python ESP/GPT image writer it needs, plus a
   `make test-loader` smoke test.
3. Boot that same image on the Skylake as an early reality check: if firmware
   quirks bite, we want to know before the kernel exists.


**Console UTF-8 (2026-10-09):** the framebuffer console decodes UTF-8 and draws code points through the font's ISO 8859-15 glyphs
  (the euro sign and Š š Ž ž Œ œ Ÿ included; typographic quotes and dashes become ASCII); in BIOS VGA text mode the same
  characters map to code page 437 where it has them. Anything else, and broken sequences, show as '?'. A tape's `putc` collects the
  bytes of one sequence before printing, so `telnet` to a UTF-8 server shows Swedish letters. Input stays ASCII. Tested by
  `make test-console-utf8` (glyph cells found in a screendump).

**Hostnames announced unprompted (2026-10-09):** every machine broadcasts a discovery-reply frame (its name) on each interface with a
  link, four times in the first seconds and then every 10 s, from the main loop and from a running tape's service loop. Receivers
  file the name in the peer table (`nic peers`) without having asked, and answer a machine they had not heard of with their own
  name, so a segment is mutually known within seconds of boot. Without a `hostname` file the name is `fos-` and the last three MAC
  bytes (it used to be `fos2` on every machine). The table is exposed to tapes by `peer_info` (ABI 5). `audio-net` takes the
  kernel's name as its default session name (`name=` still overrides), `audio-net list` prints the table, and on raw Ethernet / AVTP
  `to=NAME` resolves from it with no stream announcement; an IP `to=NAME` still waits for SAP, because the table holds MACs only.
  Tested by `make test-hostnames`. Peers are never aged out of the table.

**Config files on FAT/exFAT (2026-10-09):** `hostname`, `settings`, `debug` and `start` are looked up on the boot FOSFS volume, then the other
  FOSFS volumes, and only then in the root directory of the first FAT32/exFAT disk that has the file (`config::read_fat`), so a name or a
  start script can be edited on a PC. Files over 4096 bytes are ignored. Tested by `make test-config-fat`.

### audio-net priority, packet time and auto step-down

`priority=wifi|safe|balanced|low|live|auto` (panel row "Priority") picks the receive ring and the packet time together:
safe 12 ms / 1 ms, balanced 6 ms / 0.5 ms, low 3 ms / 0.25 ms, live 3 ms / 0.125 ms (wired, dedicated link), wifi 40 ms / 2 ms (worst case); `target=` and `ptime=` (125 to 4000 us, PCM)
override either. Measured on Skylake USB to laptop USB with the loopback probe (`send latency`): safe ~20 ms
and 0 underruns, balanced ~13.5 ms and ~7, low ~10 ms and ~74 per 28 s. Use low only on a quiet, wired
network. `auto` starts as safe, steps the receiver's ring down (12, 6, 3 ms) after 30 s without loss or
underrun, and on the first loss steps up one level and holds 60 s. The panel redraws less often at the
lower priorities (it stalls the stream loop), and the receive loop trims a ring that is far too full
(`target*2+96` frames). `make test-audio-net` covers it (`priority_test`). Only real hardware can check the
underrun counts and the loopback figures. With PTP (amd1 master, wired gigabit, skylake3 to laptop1, no probe) low
and live ran 115k and 229k packets with 0 lost and 0 underruns: capture to the receiver's ring ~3.5 ms (low) and
~3.2 ms (live) against ~7.2 ms at the default; `target=2` gave one underrun in 30 s.

### ptp (PTPv2 measurement, first step towards AES67 clocking)

`tapes/ptp` speaks PTPv2 (IEEE 1588-2008) over UDP/IPv4 on 224.0.1.129 (event 319, general 320): Announce, two-step
and one-step Sync, Delay_Req/Delay_Resp (end-to-end), correction fields, best-master comparison. `tape run ptp`
is a slave that only *measures* (the first sample sets the base offset; a real slave would step there): per
second it prints the mean offset, spread, one-way path delay and the clock-rate difference in ppm;
`tape run ptp master` is a small grandmaster (clock class 248, two-step Sync every `sync=` ms, 125 by default).
The protocol and arithmetic are in `tapes/ptp/src/lib.rs` (host tests); `main.rs` uses the shared `tape-ip` crate (`net`

`ptp l2` carries the same messages over Ethernet (Annex F: ethertype 0x88F7 to 01:1B:19:00:00:00, the message right after the Ethernet header, event messages told by type below 8), with no address, DHCP or IGMP, so it works on a bare switch. Framing helpers `build_l2_frame`, `l2_message`, `is_event` are host-tested; `make test-ptp-l2` checks the frames on the wire. `make test-ptp-e1000e` repeats the UDP and Ethernet runs on QEMU's e1000e (82574L, `FOS_NIC_MODEL`): the receive path and the software-stamp fallback; QEMU emulates no IEEE 1588 stamping, so hardware stamps stay a real-card check. Not yet done: audio-net following an L2 clock for raw/AVTP streams, and interop with `ptp4l -2` (gPTP/802.1AS, transportSpecific 1 with peer delay, is a separate profile).

**NTP into PTP into the RTC.** `crates/tape-net/src/ntp.rs` is the SNTP arithmetic (48-byte request with a cookie in the transmit field, reply checks, offset and delay from the four timestamps, Unix time to calendar, `tz=` parsing; host-tested) and `crates/tape-ip/src/ntp.rs` the exchange (four requests 0.1 s apart, the lowest-delay sample wins). DHCP option 42 is now requested and parsed (`DhcpLease::ntp_servers`, `net::dhcp_with_ntp`). `ptp master ntp=dhcp|A.B.C.D` asks before announcing and adds a fixed offset to its clock so PTP time is TAI (Unix + 37 s); the Announce then says timeSource NTP (0x50) and carries currentUtcOffset with the utcOffsetValid and ptpTimescale flags. The master never steps afterwards (it would unlock its slaves), so its oscillator drifts from NTP until it is restarted. `ptp settime tz=+2` on a slave waits for 8 samples, and only if the master's timeSource is a real reference (0x10-0x60) converts its time to UTC, then local time by `tz=`, and calls the new ABI 6 `wall_clock_set` (`kernel/src/clock.rs::set`, which also writes the RTC). One step, then the tape ends; never while recording. `ptp` takes the NIC of `nic use N` unless `nic=` says otherwise. There is no DNS client, so NTP servers are addresses. `make test-ptp-ntp` runs a fake NTP server on the QEMU segment (a day ahead of the host) and checks that the slave's `date` agrees and that a master without NTP leaves the clock alone.
and `igmp`, moved out of audio-net). Timestamps are software: the receive time is read when the polling loop gets the frame (the loop
spins), the transmit time just before the frame is handed to the NIC. A Sync read more than `late=` us after the
last empty poll waited in the ring and is skipped. The path delay is the median of the last nine exchanges, with
faster requests while it warms up.
Measured (skylake3 master, wired gigabit): amd1 path delay ~28 us, jitter ~3 us (mean of the per-second sd), rate
-5.5 ppm; laptop1 ~41 us, jitter ~20 us, rate -69 ppm. The QEMU guests do not share a time base, so `make test-ptp`
checks the protocol (lock, no lost Sync, wire format), not the offset. Only real hardware, and `ptp4l` as the
other end, can check interoperability; the I219 on the dev machine has hardware timestamping (not used).
**Hardware timestamps (I350, ABI 9).** `e1000.rs` arms the 82580-family IEEE 1588 unit (SYSTIM 1 ns/tick; RX filter
`EVENT_V2` plus ETQF/FTQF for L2 0x88F7 and UDP 319; TX stamps through an advanced descriptor) and `nic ts N [send|listen]`
shows it. The kernel attaches the RX stamp when it queues the frame; tapes read it with `nic_ts_rx` after `nic_receive`
and `nic_send_stamped`. `ptp` moves card stamps onto its own clock (`hw_to_local`) and prints which kind it uses;
`software` opts out. skylake1<->rack, I350 NIC 2 on a direct cable: path delay 1.9 us (min 0.0, max 2.5), sample-to-sample
noise about 1 us; the "jitter" line then mostly shows the -54 ppm clock-rate difference. Only real hardware checks it
(QEMU NICs lack the registers).
**Clock steering (ABI 10).** `nic_ts_adjust(nic, ppb)` writes TIMINCA (sign bit 31, size ppb*2^26/1953125, as Linux igb);
`nic_ts_step(nic, ns)` reads SYSTIM, adds and writes it back (exact to the ~5 us the register reads take).
`nic ts N adjust PPB` / `step NS` try them. skylake1 NIC 2 against the CPU timer over 4 s: +100000 ppb gave +99688,
-100000 gave -100152, +1000000 gave +1000332 (baseline +305600). `ptp` does not steer the card yet: it maps card stamps
onto its software clock, so a rate change does not show in its offsets. The pulse-per-second (SDP) output is not done:
the I350's SDP pins are not on the boards at hand to check it. Next: audio-net with `ts-refclk:ptp` audio-net with `ts-refclk:ptp`, RTP timestamps on the PTP timeline and a one-way delay readout.

**Multicast hash filter (ABI 12).** Tapes used to open all multicast (`RCTL.MPE`). `nic_multicast_join(nic, mac, on)`
sets one bit of the E1000 family's 4096-bit table (MTA, 0x5200): hash = `(mac[4] >> 4 | mac[5] << 4) & 0xfff`, register
`hash >> 5`, bit `hash & 31`, with a refcount per bit so two groups sharing a bit leave independently. Groups can share
a bit, so tapes still compare addresses. `ptp` (L2), audio-net's PTP listener and `tape-ip` IGMP join through
`tape_rt::multicast_group`, falling back to `nic_set_multicast` when it returns false (other NICs, old kernels). The
kernel clears all groups when a tape ends. `nic mcast N join|leave MAC | clear` does it by hand; `make test-nic` injects
frames to a joined and an unjoined group. The ICH/PCH parts (82579, I217-I219) have a 1024-bit table (32 registers) and
hash `(mac[4] >> 6 | mac[5] << 2) & 0x3ff`; the I350/82574 use the 4096-bit formula above. Checked on an 82579V: of 256
groups swept, only the joined one arrived. Only real hardware can confirm the tables behave like QEMU's. A switch with IGMP snooping on kept an 82579V from
getting UDP PTP (frames injected from a host arrived, the switch's group table never listed the card); with snooping off,
82579V and I350 work as slave and master over UDP.

**Audio-analysis crates (for the `ltas` tape and later ones).** All `no_std`, no allocation, host-tested (`make test`):
`crates/gfx` (a RAM `Canvas` of 0x00RRGGBB pixels: rectangles, lines, 8x16 text in the kernel's console font, and a PNG
writer: 8-bit RGB, filter "Up" plus fixed-Huffman deflate with distance-1 runs, so a flat chart is tens of KB, streamed through
a callback in 4 KiB IDAT chunks); `crates/dsp` (`math`: sin, cos, ln, log10, exp, sqrt without libm; `fft`: radix-2, split
re/im, per-stage twiddle tables, the same code built generic and with `avx2,fma`, chosen by the caller from `fp_level`;
`window`: Rect/Hann/Hamming/Blackman-Harris/Flat-top with coherent gain and power sum); `crates/ltas` (`Analyzer`: windowed FFT
every `hop` samples, one-sided mean square per bin, A/C/Z weighting per bin, 1/3, 1/6, 1/12-octave bands (ISO base-ten
centres, 20 Hz-20 kHz) by overlap-weighted bin summing or raw FFT bins, `Accumulator` with live, exponential, linear (the
LTAS) and peak views at once). Levels are dB re a full-scale sine plus a calibration offset; FFT-bin output is normalised for
noise, so a tone reads 2-3 dB low there.

**Shared pieces for audio-with-screen tapes.** What `ltas` needed that the next tape (a goniometer, level meters) needs too lives
in libraries, not in the tape: `crates/audio-screen` (`startup`: `device=`/`outdev=`, hard-float and screen checks; `Input`:
capture format, channel choice with `,` `.`, `read` of one channel or `read_pair` of two; `Playback`: pumps a generator to
the output mask; `Screen`: grid and frame buffers, the 20 fps rhythm, `present`; `poll_keys`), `gfx::plot` (palette, `Frame`
and dB `Scale`, the dB grid, staircase/polyline `curve`, Catmull-Rom `spline`), and `tape_rt::{text, save}` (`arg_value`,
`parse_f32`, `Line<N>`, `alloc_zeroed`, `alloc_pixels`; `save::Sink`, `find_disk`, `names(PREFIX, ..)`, `write_file`,
`with_buffers` for a CSV/PNG save to an exFAT or FAT32 stick). The Nordic PPM detector is `audio_wire::meter`. Behaviour of
`ltas` is unchanged (`make test-ltas`).

**The `gonio` tape** (`tapes/gonio`, `make test-gonio`) is a broadcast-style goniometer: a phosphor-persistence scope in the
middle (x = (R-L)/2, y = (L+R)/2, so mono is vertical, L only leans up-left, opposite polarity is horizontal), a Nordic PPM or
dBFS bar per channel on the right (`m` switches; peak hold and clip lamp), a correlation bar and a balance bar under the scope,
and (colours: `theme=dark|classic|light` or key `k`, from `gfx::theme`, which `ltas` shares: `classic` is black with yellow text like the older instruments, `light` is white with dark ink for bright rooms and printed screenshots, with an ink-style phosphor; every colour the charts, meters and scope draw with comes from the theme, so a new tape gets them by using `theme()`) and a loudness panel on the left, always shown: the scope leaves it at least 260 px, the numbers sit outermost, the M/S bars against the scope and the history along the bottom beside the correlation and balance bars; a narrower area drops the units, then the bars (`tapes/gonio/src/lufs.rs`, measurement in `dsp::loudness`: BS.1770 K-weighting, M 400 ms, S 3 s, gated integrated and LRA from 0.1 LU histograms; a bar each for M and S in LU against the target -23/-24/-14 on the EBU +9 or +18 scale, numbers, and a one-per-second history of S; keys `t` `e` `i`, args `target=` `ebu=`, saved by `w`; no true-peak yet). It takes the first two channels of the device (`chan=A,B`, `,` `.` step
the pair by two) and adapts its layout to the screen (`layout.rs`, designed for 16:9). QEMU has no audio input, so `src=test`
plays a built-in stereo tone through the same path (`phase=`, `right=`, `p` cycles the phase). The reusable parts:
`gfx::scope` (`Phosphor`, `draw_graticule`), `gfx::meter` (`Scale` DBFS/NORDIC, `draw_scale`, `draw_bar`, `draw_bipolar`),
`dsp::stereo` (`Correlator`: correlation and balance), `dsp::level` (`SamplePeak` with clip detection, `PeakHold`),
`audio_screen::meters` (one channel's PPM + sample peak + holds), `Input::set_pair`/`step_pair`, `noise::PairSine`. The scope has a linear and a log law (`gfx::scope::Law`, default log: distance is dB over `range=` 40 dB, so a -18 dBFS
signal reaches 55% of the radius; `l` toggles, `x` cycles 30-60 dB). The Nordic scale runs to +12 and is labelled on both sides
of the bars with a dimmer dBFS column outside. Capture only is started (an unfed output side silenced a USB device), and the
screen is redrawn every loop (`Screen::due` needs `mark_dirty`). The test
tone is not commensurate with the sample rate (997 Hz) so the dot scope does not draw 48 fixed points.

**Remembered options (`tape_rt::config`).** A tape keeps its settings in a FOSFS text file of the same `key=value` words the
command line takes: `Config::load(api, "gonio", args)` reads `gonio.cfg` (missing is fine), or, with `@NAME` on the command
line, that file instead (`diskN/NAME` and `@LABEL/NAME` work; a missing one is an error). Words on the command line come first
and so win; `#` comments and newlines are allowed in the file. `Settings` builds the body for `Config::save`, which rewrites
the file that was loaded. `gonio` saves with `w` (scale, style, law, range, gain, tau, rate; never `device=`, `chan=` or the
test tone, whose numbers are not stable). Tested in `crates/tape-rt` (host) and `make test-gonio`. Not yet used by `ltas`.

**The `ltas` tape.** `tapes/ltas` ties the crates together: it captures with `audio_read`, unpacks one channel
(`dsp::pcm`), feeds `ltas::Analyzer`, and draws into a RAM `gfx::Canvas` blitted at most 20 times a second. The built-in
generator (`crates/noise`: white, pink, sine, on/off bursts, a 10 ms fade, RMS level in dB re a full-scale sine) is written
to the output with `audio_write` in the same loop (full duplex through `audio_start(rate, capture, playback)`). The
analyser is about 1.2 MB, allocated with `api.alloc`. `make test-ltas` checks screen, loop, keys and exit against QEMU's HD
Audio codec; levels are checked by the crate tests and on real hardware. `device=N outdev=M` puts capture and playback on different devices (ABI 18, below). In a band mode the curves are a Catmull-Rom spline through the band levels (`style=bars` or `m` gives the bars/staircase).
The FFT has two code paths, baseline SSE2 and AVX2+FMA (`dsp::fft`); the tape uses AVX2 when `fp_level` says the CPU has it. The status line shows `avx2` or `sse2`, `simd=sse2` forces the baseline (`simd=avx2` is refused without AVX2) so the two can be compared, and on exit the tape prints the mean and worst microseconds per analysis frame (window, FFT and bands) for the FFT size in use.
`,` and `.` step through the device's input channels (the status line shows `ch N/M`; `chan=N` starts on N; the averages restart).
`t` cycles the exponential average's time constant: off, 0.125 ... 5 s (default 1 s); off (`tau=0`) makes each frame replace the average.
`s` saves `ltas-YYYYMMDD-HHMMSS.csv` (settings header, then centre/low/high Hz, resolved flag and the four views in
dB; `crates/ltas/src/csv.rs`) and `.png` (the screen without the message line) on the first exFAT disk, else FAT32, or `save=N`;
without a clock the name is `ltas-NNN`. Written through `fs_write` to `diskN/NAME`, so a PC reads them; `make test-ltas` runs
it on a `mkfs.exfat` image and `fsck.exfat` checks the volume.

**Separate audio input and output (ABI 18).** `audio_use_output(N)` / shell `audio output N|same` sends playback to a
different device than capture. `audio.rs` keeps `OUTPUT` beside `CHOSEN`: `audio_read`, the capture format and buffer follow the
input, `audio_write`, the playback format and space follow the output, `audio_start` starts each direction on its own device,
and `audio_info`/`audio_stats` combine the two (rates are the ones both offer). `usb_audio.rs` keeps `UNITS` (2) devices, each with its own streams and picked modes (`Backend::Usb(unit)`; `audio` lists each
with its port), so two USB interfaces (one in, one out) work; xHCI devices are independent, but a second device on the same EHCI
controller is refused (one iTD ring per direction per controller). A replugged xHCI device takes over its old unit (same port).
`make test-ltas` runs HD Audio in, USB out and checks the 1 kHz tone in QEMU's USB recording; `make test-usb-audio` plays on two
USB devices in turn and checks each recording. Two USB devices on real hardware untested.

**Graphics for tapes (ABI 17).** `gfx_info` (width<<32|height, 0 in BIOS VGA text mode), `gfx_open`, `gfx_close`, `gfx_fill` and
`gfx_blit` (a `u32` 0x00RRGGBB buffer with a stride in pixels, clipped at the right and bottom) let a tape draw on the
framebuffer. `gfx_open` clears the screen and stops the console painting it (the text grid keeps updating, so serial and
`term net` are unaffected); the kernel calls `gfx_close` when the tape ends and repaints the console. The kernel converts to
the framebuffer's pixel order (RGB swaps red and blue; anything else is BGR). Framebuffer reads are very slow (write-combining),
so a tape draws into a RAM back buffer and blits it. `tapes/gfxcheck`, `make test-gfx` (QEMU screendump).

**Hard-float tapes (ABI 16).** The kernel and ordinary tapes are soft-float (no SSE). A tape that does DSP is built for
`targets/x86_64-fos-tape-fp.json` (SSE2 baseline; the crate's `.cargo/config.toml` sets `build.target`, the output lands in
`build/cargo-target/x86_64-fos-tape-fp/`, and the Makefile's `FP_TAPES` lists them). `cpu::fp_enable` runs *before* the tape's entry,
not on request: a hard-float build may use an XMM register in its first instruction, so a call from `tape_main` would come too late.
It sets CR4.OSFXSR/OSXMMEXCPT, clears CR0.EM/TS, sets MP, and where CPUID has XSAVE, AVX, AVX2 and FMA sets CR4.OSXSAVE and XCR0 =
x87|SSE|AVX (read back with `xgetbv` before it claims AVX2); `fninit` and MXCSR 0x1f80 give a clean state. `fp_level()` tells the tape
`FP_SSE2` or `FP_AVX2` (`tape_rt::fp_level`); AVX2/FMA code lives in `#[target_feature(enable = "avx2,fma")]` functions chosen at run
time. `fp_disable` restores CR0, CR4 and XCR0 when the tape ends. No XMM/YMM state is saved anywhere: the kernel never uses it and runs one
tape at a time. A soft-float tape is unaffected. `tapes/fpcheck` and `make test-fp` (QEMU default CPU: level 1; `-cpu max`: level 2).
Verified on real hardware: skylake and rack (AVX2 + FMA, level 2).

**NIC counters in the nettest report (ABI 15).** `nic_counters(nic, out, count)` fills a u64 array: negotiated Mbit/s, duplex,
then running totals of missed packets, no-buffer events, CRC and symbol errors, PAUSE (XOFF) frames received and sent, and frames the
kernel's tape queue dropped (the statistics registers clear on read, so `e1000.rs` `error_counters` adds each read to a total). The
`nettest` master snapshots its test NIC when a test starts and the server does likewise; the T_REPORT frame appends the server's
changes (an older server sends none and its line is simply left out), and the report prints `link here:` and `link peer:` with
the speed, duplex and any counter that moved. Verified in QEMU (`make test-nettest-remote`) and on the I350 pair.

**Transmit checksum offload (ABI 14).** `nic_tx_offload(nic, on)` makes the I350's `send` hand IPv4 UDP frames whose checksum
field is 0 (and TCP frames) to the card: a context descriptor (MACLEN 14, IPLEN) plus an advanced data descriptor with
TXSM. The driver writes the pseudo-header sum into the field (not complemented) and the card sums the rest to the end of
the buffer (padding is zeros) and complements. A context takes a ring slot of its own that overwrites the slot's buffer
address, so `claim_tx_slot` treats the data descriptor after it as the proof the slot is free and puts the address back;
every send path goes through it. Frames with a checksum, fragments and non-UDP/TCP go the ordinary way. The nettest
generator turns it on around its own frames only (a relayed zero-checksum frame must stay as it is). The saving is
small at audio rates (a 1.5 KB sum is under a microsecond); it matters for nettest at line rate. QEMU has no I350, so
`tools/test_tx_offload.py` checks real hardware (`nic csum N` sends three frames, the host verifies the sums); checked
on skylake2's I350 (padded, even and odd lengths).

The RTL8168G/H (chip version 0x4c0 and up) has the same call: `send` writes the pseudo-header sum into the checksum field
and sets `opts2` bits 29 (IPv4), 30 (TCP) or 31 (UDP) plus the TCP/UDP header offset from bit 18 (Linux r8169's `TD1_*`; the
public datasheet lists none of them). Older RTL8168s use another layout and answer false. RX checksum is not used (no
consumer in the tape ABI). Checked on skylake4 and rack4 (chip 0x541) with `tools/test_tx_offload.py --nic 4`.

**VLAN filter (ABI 13).** `nic_vlan_filter(nic, vid, on)` sets one bit of the E1000 family's VFTA (0x5600, 4096 bits). The
filter (`RCTL.VFE`) is on only while at least one id is registered and the card is not promiscuous, so existing tapes
(bridge, nettest) still see every tag; untagged frames always pass. The kernel clears the table and the filter when a
tape ends. No tape uses it yet; `nic vlan N add|del VID | clear` drives it by hand and `make test-nic` checks registered,
other and untagged frames. Checked on real hardware: the I350 drops other tags (200 of 200 tagged 5 and untagged frames arrive, 6 of 200 tagged 6,
the rest background traffic); the 82579 and I219 (PCH) have no VLAN filter, VFE and the VFTA do not hold a write, so
the call returns 0 there and every tag arrives (`tools/test_nic_offload_hw.py vlan`).

**Per-packet receive stamps (ABI 11).** `nic_ts_all(nic, on)` makes the I350 stamp every received frame: the receive ring
then uses advanced descriptors (SRRCTL DESCTYPE 1, bit 30 asks for the stamp, TSYNCRXCTL type ALL), and a stamped frame
carries a 16-byte header (SYSTIML at byte 8, SYSTIMH at 12) that `receive()` strips and files as `nic_ts_rx`. The card
clock is tied to `micros()` every 100 ms by `tape_rt::rx_arrival_us(nic)`, which converts each stamp with wrap-safe 40-bit
arithmetic and falls back to `micros()` when the card cannot stamp; a tape that ends puts the card back to PTP-only
stamping. Changing SRRCTL by toggling RXDCTL.QUEUE_ENABLE kills receive: write it with RCTL.EN cleared. nettest uses it at
its three sink sites (`stamps=software` turns it off for comparison): on a direct 50 Mbit/s link the polling receiver's
jitter went from 177 us to 15 us. audio-net uses it for its PTP arrival statistics when PTP is on. Only an I350 has it;
QEMU cannot check it. Frames already in the ring when stamping is switched on carry no stamp (the card adds it on arrival;
the first few frames of `nic ts N all`, among them the 0x88b6 term-net frame that carried the command, show stamp 0). Every
frame received afterwards is stamped, whatever its ethertype or destination (checked on skylake2's I350: broadcast, unicast,
0x88b5, 0x88b6 and 0x8899, 39 of 42 stamped, the three missing ones the pre-existing frames).

**`ptp` card-clock steering (`steer`).** A slave with hardware stamps and clock steering (ABI 10) can discipline the NIC's own
clock instead of only measuring: the stamps then are the card's time, unwrapped past the I350's 40-bit count (`unwrap_card`),
and the offsets are true. The servo is linuxptp's PI servo for hardware stamps: the first two offsets give the rate error,
the clock is stepped once (`nic_ts_step`, only above 20 us), and then every sample sets ppb = kp*offset + drift with
drift += ki*offset (kp and ki from the sync interval: 1.30 and 0.13 at the default 125 ms); there is no later step. A first
design (step above 100 us, gains per second, no rate estimate) never converged: the steps repeated. Measured on the real
machines (laptop1 I219 master, clock1 82579V slave, over a switch, L2): offset +-0.3 us, rate +0.0 ppm (+64 ppm unsteered),
path delay 19.9 us. The same on two I350s (skylake2/rack2, NIC 2): offset +-0.3 us, delay 2.7 us, held for 1300 s, past the
40-bit count's wrap (about 18 min). A dip to -14 us every 8 Syncs came from the master asking for a hardware stamp on every
frame it sent (Announce, Follow_Up, Delay_Resp too) and waiting for it; `send` now stamps only Sync and Delay_Req, and the
offset sd fell from 2.2 to 0.5 us. `quiet` prints one line a minute instead of one a second: a console line costs about 10 ms. `Slave::clock_moved` shifts the times
the slave holds when the caller steps its clock. Only real hardware checks this; QEMU has no steerable NIC. A steered slave also prints the statistics after the first 60 s, when the servo has learned the rate (the whole-run
min/max of a 1300 s run, +-130 us, was the first seconds after the -17 ms step). `steer` on the *master* (L2 or UDP) makes
its Sync origin times the card's own stamps, the card first being set to the tape's clock: a plain master converts each
stamp to the polling loop's clock, and that conversion put a -6 us dip on about one Sync in 100 (a Sync sent a few ms off
its 125 ms slot). Verified on the I350 pair: settled offset sd 0.1 us, min -0.1, max +0.3 us (was sd 0.5, min -6.7).

**Timestamps on the e1000e family.** The 82579 (96 MHz clock) and the Skylake I219 (24 MHz) have the same TSYNC
registers as the I350 (Linux's e1000e `ptp.c`), with four differences handled in `e1000.rs`: SYSTIM counts fractions of
a nanosecond (`ns = SYSTIM >> shift`, 19 and 14); TIMINCA must be programmed (`pch_clock`) and is also how `ts_adjust`
steers (INCVALUE scaled by the ppb); SYSTIM cannot be written, so `ts_step` keeps an offset; and receive filtering is
RXMTRL (ethertype 0x88f7) plus RXUDP (319), with no per-frame stamping (`nic_ts_all` is I350 only). A transmit stamp is
asked for per frame through a data descriptor (DEXT, DTYP_D, EXTCMD_TSTAMP in the upper dword); the I219 also needs
FEXTNVM7 bit 0. The 82574 is not enabled (QEMU's e1000e would claim stamps it cannot give). Verified on a Skylake I219-LM
(laptop1), an 82579LM (piano1) and an 82579V (clock1): clocks run and steer (+100000 ppb measured +99.7 and +100.1 ppm), TX
and RX stamps arrive, and `ptp` laptop1 -> clock1 over a switch measures a path delay of 18 us against 51 us with
software stamps. audio-net behind a clock1 master (hardware stamps) shows capture-to-arrival of 76 us; behind piano1 (software) it was +-4 ms.

### audio-net on a PTP clock

`audio-net` follows a PTPv2 clock when one is found: `ptp=auto` (default), `ptp=l2` or `ptp=off`. `clock.rs`
holds one `ptp-tape` `Slave` plus a `Tracker` (local clock to master time, rate measured over 2 s windows,
trusts samples below its prediction more since software timestamps only err late) and joins 224.0.1.129 itself
(its own IGMP state, left on exit). A sender waits up to 3 s for an Announce and 13 s for a lock *before* it opens
capture (a long wait would overflow the capture ring), then stamps each packet with the PTP time of its first
sample, in sample periods modulo 2^32 (AES67), smoothed by a slow phase filter because the USB audio clock cannot
be steered; the SDP gets `a=ts-refclk:ptp=IEEE1588-2008:<grandmaster>:0`. It decides once: a lock mid-stream
would make the stamps jump. A receiver measures `PTP now - stamp` per packet (rejecting values outside -5..500 ms,
which is what an unstamped stream gives), and shows it in the summary and the panel as capture-to-arrival plus
the ring level. Raw and AVTP streams have no IP and listen for Ethernet PTP (0x88F7, the `ptp l2` master) instead; `ptp=l2` forces that for RTP. Raw stamps its header timestamp like RTP; AVTP sets a valid presentation time (capture + 2 ms, ns mod 2^32, `AVTP_OFFSET_NS`) on the PTP timeline, which is not a gPTP/802.1AS clock. Opus and `serve` are not wired. A raw or AVTP sender now also waits up to 3 s for a clock unless `ptp=off`. Measured on hardware (amd1 `ptp master l2`, skylake3 USB capture to laptop1): capture-to-arrival 0.26-0.40 ms at low/live, 2.7-2.8 ms to the ring, the same as RTP within noise (UDP: 0.28-0.43 ms, 3.0 ms). `make test-audio-ptp` (and `-raw`, `-avtp` over layer 2) runs a
master, a sender and a receiver in QEMU (lock, SDP, plausible delay, `ptp=off`); the guests share no time base, so
accuracy needs real hardware: compare the figure with the loopback `send latency`.

**Help screens in the tape file.** A tape's usage text lives in its own ELF: `tape_rt::tape_help!` puts it in a `.fos_help` section (kept by `tapes/tape.ld`), and `abi::elf::section` finds it through the section header table. `help NAME`, `NAME help` and `NAME --help` print it straight from the staged file, so the tape is neither loaded nor run and needs no argument handling of its own; a tape without the section runs as before (`ed help` still edits a file called help). `make test-scripts` checks all three spellings and the message for a tape with no screen.

**One master, both transports.** `ptp master both` (needs an IP, as UDP does) sends every Announce, Sync and Follow_Up as IPv4/UDP and as 0x88F7 frames, with one clock and shared sequence numbers; each transport's Follow_Up carries the time its own Sync left, so the second send does not skew it. A Delay_Req is answered over the transport it arrived by. Slaves still pick one transport at start (`both` is refused for a slave and with `l2`), so they never see two masters. `make test-ptp-both` runs a UDP and an Ethernet slave on one master in QEMU. On hardware, amd1 as master with laptop1 (UDP) and skylake3 (Ethernet, NIC 3) both locked at once: 0 Syncs lost, 18 Delay_Resp each.

**Serial output no longer stalls a running tape.** COM1 at 115200 baud takes ~87 us a byte and `serial::write_byte` waited for the UART, so an `audio-net` panel redrawing a few lines (a few hundred bytes with escape codes) held the receive loop up for 16-23 ms. With the 12 ms `safe` target the USB playback ring ran dry about 15 times a second (`Un` in the hundreds per 30 s, servo pinned at +2000 ppm); the command-line receiver, which prints almost nothing, was fine. While a tape runs (`term::set_tape_mode`) output now goes into a 128 KiB queue (`serial::defer`) that `serial::pump` feeds to the UART when it has room (from `tape_service` and the idle `wait_for_event`); tape end, panic, halt and reset call `serial::flush`. A full queue falls back to waiting. Measured on the Skylake: loop gap 16.9 ms -> 0.2 ms, `Un` 0 after lock, offset -150 ppm. The panel counters line now shows the ring range (`Ring 14/12 (2-17)ms`) and `Gap`, the longest receive-loop pass of the last second; `usb audio playback: ...` is printed when a USB stream stops.

**Servo anti-windup; steadier tone test.** `Servo::update` (`crates/audio-wire`) stops integrating while its output is at the +-2000 ppm limit and the error pushes further that way, so it comes back at once when the ring level turns. `tools/test_audio_net.py` measures a tone by the median distance between rising zero crossings instead of a crossing count: QEMU's emulated USB audio drops a few milliseconds now and then, which skewed the count (a wrong frequency in about half the runs). A receiver without `group=`, `source=` or PTP takes only unicast: a multicast sender needs `group=` or `source=auto` on the receiver (documented since the multicast work), so that case is by design.

### Shared ARP and DHCP frames

`crates/tape-net/src/frames.rs` builds ARP requests/replies and client DHCP frames and feeds received DHCP replies to a
`DhcpClient`. `ip-chat`, `telnet`, `router` and `tape-ip` (`audio-net`) all use it; each keeps only its own way of
sending a frame (callback, per-NIC send, tape API). Verified: host tests, `test-nettapes`, `test-telnet`,
`test-router`, `test-audio-net`.

### Parallel `make test-all`

`make test-all TEST_JOBS=N` runs `tools/run_tests.py`: N workers, each with its own `FOS_TEST_SLOT`; `tools/testslot.py`
`port()` shifts every fixed host port (QEMU multicast segments, host-forwarded and listening TCP ports) by 1000 per slot, so
tests do not share a segment. Logs go to `build/test-logs/`; the slowest tests of the last run start first; a failure is
re-run alone at the end (each worker also gets its own copy of the image, `FOSFOR_IMAGE`, because QEMU refuses two writers on one file; tests must honour that variable) and reported as "only when run alone" if it then passes. A new test with a fixed host port must
wrap it in `port()`. `fos.yaml` (the landsbro buildbot) uses `TEST_JOBS=8`. Without `TEST_JOBS` the run is serial as before.

### fterm and the serial tape calls

ABI 7 adds `serial_count/info/open/close/read/write/lines/set_lines/break`: the 16550 UARTs at COM2-COM4 (`kernel/src/uart.rs`; COM1 is the
console and is never offered). Polled, with a 4 KiB ring per port filled whenever a tape asks for input and while `wait_for_event` yields
(100 us) with a port open, since the FIFO holds only 16 bytes. The `fterm` tape is a serial terminal over them: keys out as typed (Ctrl-C and
Esc too), received bytes to the console, commands behind Ctrl-A plus a plain letter (no AltGr combination, for Nordic keyboards). `make
test-fterm` plays the device on QEMU's second UART over TCP.

The tape's transports are `Link`s (`tapes/fterm/src/link.rs`: `poll`, `read`, `write`, `status`, `close`; the loop never blocks): `serial.rs`,
`telnet.rs` (the telnet tape's session as a non-blocking state machine: DHCP or static, ARP, TCP, option negotiation, reusing `telnet_tape` and
`tape_net`; it does not answer cursor-position queries) and `termnet.rs` (HELLO/DATA/BYE to a MAC or an announced name from `peer_info`).
`run fterm telnet IP [PORT] [static LOCAL/PREFIX GATEWAY]` and `run fterm net NAME|MAC` follow `nic use`. `make test-fterm-net` runs both against a
host Telnet server and a host-side `term net` peer.

A bare `run fterm` (or `run fterm ui`) shows a settings panel like audio-net's and the recorder's (`tapes/fterm/src/ui.rs`; the rules, host-tested, are
`form.rs`): transport (serial, Telnet, `term net`), the rows that belong to it, `P` opens a list of the machines in the peer table (Up/Down, Enter chooses, Esc cancels), `C` connects, `W` saves
`fterm.cfg`, which is just the equivalent command line (`run fterm @fterm.cfg` repeats it; more words may follow the `@`). Keys are plain: Up/Down or
Tab select, Space/Backspace/Enter change, Left/Right too. A session returns to the panel. Over the console UART a raw ESC reads as abort, so arrow
keys only work from the keyboard and `term net`; that is why Space/Backspace exist. `make test-fterm-ui`.

**Keyboard layout.** The local keyboard (PS/2 and USB HID) decodes as US by default. `keymap sv` (also `se`, `fi`) in the shell, or a `keymap sv` line in
`settings`, selects Swedish/Finnish: å ä ö (and Å Ä Ö) on the keys right of P and L, the symbols moved, AltGr for `@ $ { [ ] } \ | ~` and the euro sign;
dead keys type their plain character. `abi/src/keymap.rs` holds the tables; å ä ö travel as private codes 0x90-0x96 and `tape_service` expands them to
UTF-8 for tapes (fterm sends them on to Telnet/serial). The shell and file names stay ASCII. `make test-keymap`.

**Serial shell.** `serial-shell [PORT] [BAUD]` in `settings` (default: the first listed port, 115200 8n1) puts this machine's shell on that port:
the kernel's output is mirrored to it (LF as CR LF) and its input is read like the keyboard's (`keyboard::pop` falls back to `uart::shell_read`).
It is opt-in, so "deny" means not having the line; there is no password, so it is a full shell for whoever holds the cable. A shell port is not
offered to tapes (`serial_open` gives `SERIAL_BUSY`, -3; `fterm` says so). The main loop polls the UART every 100 us instead of `hlt` while it is on,
so an idle machine burns CPU (the FIFO is 16 bytes, a pasted line would be lost otherwise); `test-idle-cpu` runs without it. `make test-serial-shell`
boots an image with the setting and plays the laptop on COM2. Planned: USB serial adapters as another port `kind` behind the same calls.

**COM1 receive interrupt** (`serial.rs`): the console's input used to be polled on the 10 ms idle tick, and the UART's 16-byte FIFO overran on a pasted
or scripted line (about 4 of 24 characters lost on rack1 at 115200). With an I/O APIC, IRQ 4 now goes to vector 0x42 (FIFO trigger 8, OUT2 set); the handler
only moves bytes into a 1 KiB ring (`RX_DROPPED` counts overflow) that `read_byte` empties. Without an I/O APIC or a UART it polls as before. Verified on
rack1 (a 127-character line sent in one write arrives whole; idle power unchanged) and by `test-serial-shell`, `test-idle-cpu`, `test-bios-boot`, `test`.

*Verified on real hardware (skylake, 2026-10-13).* A USB-serial adapter on the host (`/dev/ttyUSB3`, 115200 8n1, user in `dialout`) reached
the shell on skylake's serial-shell port; `help`, `uptime`, `debug status`, `usb`, `disks`, `nic` and `dmesg` all worked. `debug crash`
raises a #UD on purpose (it does not read the crash record): the exception dump went out over serial, the machine reset after the 3 s
`reboot-delay`, came back on the same slot and re-ran `start`. The boot log (`dmesg`) can be read over the cable even with every NIC down.
Use raw termios or `python3 -m serial.tools.miniterm /dev/ttyUSBN 115200`; send CR as Enter. Do not probe with `debug crash` or `debug hang`
unless a reset is wanted.

**USB serial adapters** (`kernel/src/usb_serial.rs` generic layer, `usb_serial_ti.rs` the first chip driver). A chip driver implements the
`Chip` trait (claims a vendor/product, picks the configuration, runs a one-time `attach` such as a firmware download, then `open`, `close`,
`set_lines`, `set_break`); the generic layer finds the bulk endpoints, owns the DMA bounce page, the receive ring and the statistics, and
works on **EHCI** (blocking bulk OUT; bulk IN polled with a 10 ms limit, a timeout meaning "no data") and **xHCI** (blocking bulk OUT; one
IN TRB kept queued, completions arriving in `background_event`, like `usb_midi`). `uart.rs` numbers the USB ports after the 16550s
(`SERIAL_KIND_USB`, name `USB1`...), so `serial_*`, `fterm list` and `fterm N 115200 8n1` work unchanged. No shell on a USB port yet
(`serial-shell` ignores them). Another simple chip (FTDI, CP210x, CH341, PL2303, CDC-ACM) is one more `impl Chip` and an entry in `CHIPS`.

The Moxa UPort 1110 (110a:1110, TI TUSB3410) has no firmware on board: copy Moxa's `moxa-1110.fw` (linux-firmware, LICENCE.moxa) to a FOSFS
volume (`foscopy.py --mac N moxa-1110.fw :disk1/moxa-1110.fw`). USB probing runs before the volumes are mounted at boot, so the first pass says
the file is missing; run `usb rescan` (or put it in `start`) and the driver downloads the firmware, resets the port and picks the adapter up as
`USB1` once it re-enumerates. A hub child is made to look new by `ehci::unclaim_hub_child`; `usb_hid::probe_all` runs one extra pass after a
download. The modem-status interrupt endpoint is not read: CTS/DSR/DCD/RI read as low, DTR/RTS as set. Verified on real hardware (clock1,
EHCI behind an Intel hub, to skylake's `serial-shell`): `run fterm 1 115200 8n1` showed skylake's `help`, `uptime`, `debug status` and `ls`.
Also verified on skylake's xHCI controller (full speed, firmware download, attach, bulk OUT, queued bulk IN; the adapter's d-sub wired to skylake's own COM1 gave a console feedback loop: 112904 bytes received, no errors). `usb` shows how many IN transfers completed. Not verified: unplug/replug, rates other than 115200, parity, the adapter against a second machine (a test against amd's COM1 got nothing; its port may be off). QEMU has no TUSB3410.

The Prolific PL2303 (067b:2303, `usb_serial_pl2303.rs`) needs no firmware: `attach` runs the vendor handshake (reads 0x8484/0x8383, writes 0x0404,
(0,1), (1,0), (2,0x44)), `open` sets the line coding (class request 0x20) and DTR/RTS (0x22), break is SEND_BREAK (0x23). The HX/TA init is assumed;
bcdDevice is not read, so the older PL2303 (non-HX) variants are untried. Verified on real hardware (rack's xHCI, full speed; the d-sub on skylake's COM1
console): `run fterm 1 115200` on rack showed skylake's whole boot log (5805 bytes), and typing `ver` from fterm reached skylake's shell, which answered.
Not verified: unplug/replug, other baud rates and formats, modem-status lines.

### USB-MIDI (kernel/src/usb_midi.rs)

Class-compliant MIDIStreaming interfaces (audio class 1, subclass 3) on xHCI, directly on a root port or behind a hub: `usb_hid::probe_device` finds one
after the keyboard and mass-storage checks (`find_endpoints`: bulk IN + bulk OUT). Messages travel as 4-byte USB-MIDI event packets (cable<<4 | CIN, three MIDI
bytes). The IN transfer is queued one max-packet at a time (a cheap full-speed cable has 8-byte packets), `background_event` splits packets into a
256-entry ring, `release` forgets a device with its slot. Shell: `midi` (list), `midi monitor` (print what arrives until Escape), `midi send [#N] HEX..`
(one message, no SysEx, cable 0). Verified on skylake3 and laptop1 with a 2381:0100 cable: enumeration, OUT transfers and, looped out to in on laptop1, IN events (note on/off, control change). A blocking monitor must call `drain_nics`, or a `term net` Escape never arrives. `make test-midi` covers only the no-device paths (QEMU has no USB-MIDI model). Not yet: SysEx
on OUT, several cables per device, a tape ABI (the X-Touch/Mackie work will add it).

### DNS resolution

A small A-record client: pure codec in `tape-net::dns` (host tests), blocking `tape_ip::dns::lookup`, and a
non-blocking `Phase::Dns` in the fterm Telnet link. `ptp ntp=NAME` uses it too. The server comes from `dns`, DHCP option 6,
or the gateway. Tested end to end by `make test-fterm-net` against a Python DNS responder reached as 10.0.2.2.

### tapesync (kernel/src/tapesync.rs, crates/tape-sync)

Explicit, pull-only, authenticated tape sync between machines: `tapesync [HOST]` lists what differs, `tapesync [HOST] apply` installs it. Without HOST every machine in the peer table is asked in turn (one that does not answer costs a 3 s timeout); the newest offer of each tape wins and its source is shown, and equal build times with other bytes are reported, not guessed.
Every machine that has a `synckey` file (one line of text, the same on the machines that trust each other; NETFS refuses to read
or write it) answers `TAG_SYNC_REQUEST 0x40` (a 16-byte nonce) with a manifest as a GET stream: per stamped tape on the boot
volume its name, size, build time, tape ABI and SHA-256, then HMAC-SHA-256 over `nonce || manifest`. The requester trusts only a
manifest that verifies for its own nonce (so no replay), fetches files with plain NETFS and installs one only if size, SHA-256
and stamp equal the signed entry. A *stamp* is 16 bytes in the ELF section `.fos_build`, added after linking by
`tools/stamp_tape.py` (it also records the CPU, x86-64 or aarch64, and `tapesync` skips tapes built for another one) (`make stamp-tapes`; the image and `build/fosfor/tapes/NAME` are the stamped copies, deploy those): the
commit time of HEAD for a clean tree (machines building one commit agree), else the build time, plus the ABI version from
`abi/src/tape.rs`. Rules (`tape_sync::decide`): missing here is new; a newer stamp replaces; the same bytes are left; our newer or
equal-time-different bytes are kept; a tape for a newer ABI than this kernel is skipped; a file here that is not a stamped tape
is never overwritten. The server answers one request per 2 s (hashing every tape is work) and not while its own client command
runs. New tapes need free FOSFS directory entries (64 per volume; DEFERRED.md plans a bigger directory), checked before anything
is fetched. FOSFS rewrites a file in place when it still fits, so a power cut mid-write can damage that one tape; run it again.
Not done: sync of non-tape files, the other disks, automatic or periodic sync. `make test-tapesync` (four machines: same key,
other key, no key) covers it; the crate's host tests cover SHA-256/HMAC vectors, the manifest and the rules.

### audio-net bench (tapes/audio-net/src/bench.rs, crates/audio-bench)

`audio-net benchserve` on one machine and `audio-net bench peer=NAME|MAC [proto=raw|avtp] [rate= bits= ch=] [tol=PERCENT]
[stepsecs=N]` on the other (the master, also the panel's T key) find the lowest `ptime=` and `target=` the link carries. The master
sweeps 125 us ... 2 ms packet times; each step streams numbered packets of the real frame size on an absolute schedule, first
master to server, then server to master (the larger need of the two counts). The receiver keeps one `i32` per packet (its arrival
relative to the ideal schedule) and judges them afterwards: the schedule is a line through the minima of half-second windows (absorbs
the clocks' ppm difference), a packet's lateness is its height above that line, and the smallest ring `target` that leaves at most
`tol=` (default 0.01 %, 100 ppm) of the packets late or lost is the step's *need*; the *clean* column is the target that leaves none.
A recommendation is need plus a quarter (at least 250 us), rounded up to whole ms (`target=` takes ms). Control frames are unicast
ethertype 0x88b5 (`ABN1`: HELLO/STEP/QUERY/REPORT), so no IP is needed; clocks are the PTP clock when both ends lock to the same
grandmaster, else a four-stamp offset (min round trip of up to 16). Nothing prints while a step runs. Network only: no audio device
(an end-to-end loopback and stress load are not done). 100 ppm is a glitch about every 10 s at 1 ms packets, so `tol=0` for a
margin. `make test-audio-bench` (QEMU, e1000: shows drops at 125/250 us, so only the 2 ms steps must be clean); only real hardware
tells what a given switch and NIC do. The pure part has host tests (`cargo test` in crates/audio-bench).

**The `clock` tape** (`tapes/clock`, `make test-clock`) is a Wharton-style studio clock: a ring of 60 red dots (a saturated red; the themes' alarm red looked pink on black) that light up one second at a time, the time as big yellow seven-segment digits (`gfx::dial`, 24 h by default) with the seconds below and the date; a pixel screen and a hard-float build like `gonio`. It keeps its own time: the model is `unix_ns = micros()*1000 + offset`, redrawn when the shown second changes. `ntp=` (default `dhcp`: option 42, else the gateway; an address or a name) asks at start and every `resync=` s (a non-blocking `tape_ip::ntp::Client`: the picture keeps running, the loop polls flat out for the 0.4 s a query takes); `source=ptp` instead listens as a PTP slave (`ptp_tape::Slave`, UDP group 224.0.1.129, or Ethernet 0x88F7 with `l2`, which needs no IP or DHCP; `domain=N`), needs a master whose Announce says its time is real, and follows every Sync, smoothing the offset by 1/8 and jumping when it moves over 50 ms. A PTP clock sleeps between polls (a Sync is timed up to 10 ms late, so the clock is about 0-5 ms off); `spin` polls flat out and discards late Syncs (sub-ms, CPU busy). The status line turns amber when nothing was heard for 10 s. Either way the machine's clock and RTC are set when they differ by 1 s. The zone is `tz=+H[:MM]`, else `ntp::guess_zone` from the machine's clock (which holds local time), else UTC. No hardware stamps for PTP yet; the accuracy is that of a software slave (hundreds of microseconds), plenty for a seconds dot. `lead=N` (0-3, key `l`, default off) is the old studio clocks' cue: the next 5-second mark is lit from the previous mark until N seconds later, dark for the rest and lit for good when the count reaches it (with the cue on, the 12 o'clock dot is dim at 55-59 like the rest, so the minute mark gets the cue too). The status and key lines are dim until a key lights them for 6 s (`bars=on` keeps them lit; a stale clock stays amber). The test runs a fake NTP server and a PTP master guest and decodes the screenshot.

### Services: quiet tapes in the background (kernel/src/tasks.rs)

`run NAME ARGS &N` starts a tape as a *service* that owns NIC N (1-based) and
runs next to the shell or a foreground tape. `ps` (or `tasks`) lists services, `kill N`, `kill NAME` (every service running that tape) or `kill all`
Scheduling is cooperative: a context switch (`fos_ctx_switch`, callee-
saved registers only) happens at tape-call boundaries (`should_abort`, `read_key`,
`micros`, `wait_for_event`, 200 us slices) and in the shell loop, never inside a
kernel routine, so no locks are needed. Each service has its own 4 MiB tape
region and 1 MiB stack; at most 2.

NIC ownership replaces frame filtering: frames of a service's NIC go only to the
service, and the foreground tape gets none from it. A second service on an owned
NIC is refused. Tape-end cleanup (multicast, VLAN, offload, timestamps) skips
service NICs; `nic_release` clears them when the service ends.

Limits: services must be quiet (console output is shared), must not be audio
tapes (audio, UART and gfx teardown are not owner-aware), and keys/Escape go to
the foreground only. KUP and NETFS work while a service runs. Test:
`make test-services` (ptp master service, slave locks on, foreground tapes,
`kill`). Only real hardware shows Sync jitter next to a busy foreground tape.

**Several screens** (Pi): splits are per physical screen; tape screen numbers 0 and 1 are whole screens, 2+2P and 3+2P the halves of split screen P. `run A | B + C | D` (up to four tapes, Tab moves the keys) and `run A | B screen=P &N` (background pair) use them; see `docs/aarch64-plan.md`.

**Two graphic tapes on one screen** (`run A ARGS | B ARGS [split=h|v]`, `make test-split`): the console is cut in two, side by side
(`split=h`, default) or stacked (`split=v`). Tape screens 0 and 1 become the halves (`framebuffer::split_set`; `gfx_info_on` reports a
half, fill and blit are clipped and offset into it), so a tape that lays itself out to its screen size needs no change. `tape-rt`
always uses the numbered gfx calls for that. A runs in front on screen 0 with the keys; B has screen 1. On the Pi both are tasks
(Tab moves the keys, as with `+`); on x86 B is a service without a NIC or keys and ends with A. `run A + B` (Pi) is the
two-display form: screen 0 and screen 1.
`rotate=r|l` (clockwise / anti-clockwise) turns the whole tape screen for a monitor mounted in portrait: tapes see an upright
screen of swapped width and height (`gfx_info`, fill and blit rotate onto the framebuffer, `framebuffer::Rotate`) and the split
is cut on that logical screen, so `rotate=r split=v` gives two squarish areas. The `rotate r|l|flip|off` setting makes it permanent (the Pi's second screen has `rotate1`, and `rotate all|0|1 MODE` picks the screens, and a `rotate=` on a `+`/`|` group turns the physical screen that group draws on);
the text console is not rotated. `make test-split` checks all four layouts.

### Native float audio (ABI 21, crates/audio-sample, crates/audio-wire)

One USB audio driver (`usb_audio.rs`) serves both kernels and now detects 32-bit IEEE float streams (alt settings with
the float format tag); `audio_info` reports one as slot 4, bits 32 and the new call `audio_float` says whether the
active stream is float (`AudioInfo` never gains fields). `audio-sample::Format` is the single place that knows a
slot's layout: float is full scale 1.0, integers use one scale of 2^(bits-1) with round-to-nearest, so integer
samples survive an f32 round trip exactly. Tapes carry f32 from the device to the DSP: `record`/`recorder` write
float WAV (format tag 3) for a float device, `play` reads it, `ltas`/`gonio` meter floats without conversion.

`audio-net` is a hard-float tape on libopus 1.6.1's float build (`FLOAT_APPROX`, SSE2, `make opus-float`):
`encode_float`/`decode_float`, so Opus never sees an integer. The PCM path stays bit-exact. Opus decoded to an
integer device is dithered (TPDF, `audio_wire::Dither`); a float device gets the samples as decoded. A private
wire mode `bits=32` (or `bits=f32`) sends big-endian IEEE singles: raw header bits 32, RTP payload `F32` in the
SDP (no RFC name exists), AVTP AAF format 0x01. L16/L24 remain the standards-based modes. `send latency` needs
16 or 24 bits. The panel's codec row has a "PCM 32-bit float" choice (saved as `bits 32`, last in the list so the Opus numbers stay put). Tests: `make test`
(`audio-sample`, `audio-wire` round trips) and `make test-audio-net` (run h: float RTP, F32 SDP, 350 Hz tone).
Only real hardware shows a float interface's behaviour and Opus's CPU cost on old machines.

### Drawbar organ and tape MIDI (ABI 25-26, crates/organ, crates/fx, crates/instrument-rt, tapes/organ)

ABI 25 gives tapes USB-MIDI: `midi_read` returns one received message (device, cable, length, up to 3 bytes) and
`midi_send` writes one to a device (`tape_rt::midi_event`, `midi_send`); ABI 26 `midi_devices` counts the connected ones
(`tape_rt::midi_devices`): the organ hides its on-screen keyboard while one is connected and gives the room to the panels. `crates/organ` is a `no_std` Hammond-style engine
with host tests: 91 tone wheels (one sine per semitone from C1), nine drawbars at 3 dB per step, 2nd/3rd-harmonic percussion,
key click, a scanner vibrato/chorus (modulated delay), a drive stage, a Leslie (800 Hz crossover, horn and drum with speed
inertia, Doppler delay) and a Freeverb-style stereo reverb after it (`reverb.rs`,
one level slider; lengths scale with the sample rate). The mod wheel (CC 1)
picks the Leslie in three zones (0-42 Stop, 43-84 Slow, 85-127 Fast). `tapes/organ` is the hard-float front end: touch panel and typed keys (a terminal sends no key
release, so typed notes ring 0.3 s), MIDI keyboard, 61 keys (MIDI 36-96). It keeps a short audio queue (`latency=`, default 40 ms)
and sends the picture in 32-row bands, topping the queue up between bands, because a full-screen framebuffer write takes longer
than the audio queue holds. `make test-organ` checks the picture and that a typed key sounds at the right pitch in a USB-audio
recording; MIDI input, touch mapping, underruns and the Leslie/loudness balance need real hardware.

Reusable parts: `crates/fx` holds the effects (`Rotary` (the Leslie), `Reverb`, `Ensemble` (a three-voice string-machine
chorus), `Phaser`, `overdrive`, `Delay`, `Sine`); `crates/organ` re-exports `Leslie` and `Reverb` from it. `crates/instrument-rt`
is the tape glue: `Output` (audio device, `rate=`/`latency=`/`out=`, queue pump, scope), `Touches` + `TouchTarget` (finger grabs),
`TypedKeys`, `poll_midi` and `blit_banded`. The next instrument (a divide-down string machine) is meant to need only its own
oscillators, UI and wiring.
