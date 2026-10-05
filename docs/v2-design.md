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
  directory. Test: `make test-v2-format`.
* **`ed`, `copy` and the diagnostic commands (ported from v1).** `ed` (`run ed NAME`, also
  `diskN/NAME` and `@LABEL/NAME`) is the v1 line editor; its cursor-key line editor needs the
  arrow/Home/End/Delete keys, which both keyboard decoders now produce as private codes
  (`keymap::KEY_*`) and a running tape receives as the ANSI sequences a terminal would send, and
  the framebuffer console now understands `ESC [ n C`, `ESC [ n D`, `ESC [ K` and `ESC [ 2 J`.
  The tape file callbacks (`fs_read`/`fs_write`/`fs_remove`) accept the same `diskN/NAME` and
  `@LABEL/NAME` references. `copy SRC NAME DST [NEWNAME]` moves a file between volumes through a
  page-allocator buffer (no 64 KiB limit). `cpuinfo`, `bootinfo`, `memmap`, `irqstat` (the I/O APIC
  pins), `pci read|write B:D.F`, `io r8|w8..` and `mmio r32|w32..` (mapped addresses only, never the
  kernel image) and `clear` report or poke the machine from the shell. Tests: `make test-v2-ed`,
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
  peaks), `audio tone [HZ] [SECONDS] [RATE]`. `make test-v2-usb-audio` plays a tone through QEMU's class 1.0
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
* **`audio-net` (network audio, stage A: raw Ethernet).** One tape, `audio-net send|recv`, with an `audio-wire`
  crate (host-tested) for the formats. Raw mode (ethertype 0x88ba): an 18-byte header (sequence, frame
  timestamp, stream id, channels, bit depth, frames, rate) and big-endian 16/24-bit interleaved samples,
  1 ms packets for a stereo pair and shorter for many channels so a packet fits one frame. `send` takes the
  capture side of the USB audio device (`ch=9-10,1` picks channels, `bits=`, `to=MAC`) or a test tone
  (`src=tone=440`, no audio hardware needed); `recv` locks onto the first sender, plays on the device
  (`ch=` places the stream's channels on output channels), bridges short sequence gaps with silence, resyncs
  after long ones, and keeps the playback ring near `target=` ms by dropping or repeating one frame at a
  time (a resampler replaces that in a later stage). Playback's transfer window is now ~6 ms (a packet plays a
  whole window after it is filled, so the window is the latency). `micros()` no longer goes backwards when
  the timer interrupt is late (a pending timer interrupt counts as an elapsed tick, and the value is clamped).
  Tested between two QEMU guests (`make test-v2-audio-net`: stereo 24-bit and mono 16-bit tones, 0 lost,
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
  Hardware (H6 on the Skylake to the UFX on the Sandy Bridge machine, raw Ethernet, 100 s, 100,045 packets): 0 lost,
  0 frames dropped or repeated, learned offset 8 ppm (so the two interfaces' clocks agree to ~10 ppm). With the
  sender deliberately skewed (`skew=` on a tone source) the learned offset moves by exactly the skew (+400 then
  -400 ppm gave +64 and -737 ppm, a difference of 801), again with nothing dropped. A caution: console output
  is slow (about 10 ms a line over serial), so any per-packet or per-loss message makes a receiver lose the next
  packets; `verbose` therefore prints only a few gap lines and one status line per 10 s.
  **Stage C, Opus:** `codec=opus` (with `proto=rtp`, 48 kHz) sends and receives Opus (RFC 7587, `opus/48000/2`) from the
  vendored fixed-point libopus (`build.rs` links `build/v2/opus-fixed/libopus_fixed.a`). One RTP stream per stereo
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
  that stalls while writing does not overrun the take at once. Tests: `make test-v2-record` parses the image on the
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
  address is this machine's). A receiver can say `source=NAME`: it waits (15 s) for the sender's announcement and takes
  codec, channel count, rate, port and multicast group from the SDP, so the cfg of a receiver can be just `mode recv`,
  `ip ...` and `source studio-a`; a sender can say `to=NAME` and sends to the listener's address and port with the
  codec it announced. Names are looked up again on every retry. The SDP parser (`audio_wire::sap`, host-tested) reads
  L16/L24 and Opus sessions, so announcements from other AES67 gear that use these are listed too (not tested: no
  such device here). Raw Ethernet and AVTP have no IP and are not announced. QEMU test: list, `source=`, `to=` and the
  withdrawal; hardware: the Skylake's 'h6' listed on the Sandy Bridge, which received it by name with no other
  parameters, and the Skylake's list showed the Sandy Bridge as a listener.
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
  serial line (`make test-v2-lineedit`). The history is not kept across reboots.
  **Scripts and bare command names:** a word that is not built in runs `NAME.sh` (or `NAME` as a tape): `NAME.sh` is a file
  of commands, `#` starts a comment, blank lines are skipped, `$1`..`$9` are the arguments and `$*` all of them, each
  command is echoed as `NAME.sh> command`, and scripts may call scripts four deep. Both are looked up on the boot volume,
  then on the other disks (a file with no dot in its name is taken for a tape; a built-in command of the same name always
  wins). The `start` file can use them. Scripts nest shell commands, which have large stack frames (the command function
  holds several 4 KiB buffers), so the kernel stack grew from 64 to 256 KiB; the three-deep recursion test had overflowed
  the old one with a double fault. The same overflow was already crashing `tapes/test_bridge_e2e.py` (`nic recv` inside a
  nested command) on older commits; with the bigger stack that test no longer crashes but still does not see the bridged frame
  (not investigated yet). Tested by `make test-v2-scripts`.
  **`nic` subcommands:** `nic sniff [N]` puts an interface in promiscuous mode and prints one line per frame (addresses,
  then ARP who-has/is-at, IPv4 with TCP/UDP ports and tags for DHCP, DNS, SAP, RTP, AVTP, audio-net, LLDP, fos frames) until
  Escape, at most 40 lines per pass so a busy network does not make the screen the bottleneck (it says if it skipped);
  the terminal's own frames are handled, not shown, and Escape also works from a `term net` client. `nic discover` probes
  every interface for three seconds and lists the fos machines that answer with their host names (`nic peers` shows the
  table again), `nic map` sends a probe from every interface with a link and shows which of this machine's other
  interfaces hear it (cabling; up to three rounds while links come up), `nic use N` picks the interface the others
  default to. `nic regs`, `nic send` and `nic recv` were already there. Tested over the serial line with injected
  frames, a second guest and a guest with two NICs on a QEMU hub (`make test-v2-nic`).
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
  `tape send`. Tested (`make test-v2-netfs`): foscopy both ways with a 37-chunk round trip compared byte for byte,
  refusals, and two guests copying to and from each other.
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
  keeps the shared GUIDs). Tests: `make test-v2-disks`, `make test-v2-stick-guids`.

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
| M3b (done) | Local keyboard | PS/2 (8042 + I/O APIC IRQ 1) and USB HID boot keyboards on xHCI root ports and on EHCI (directly, or behind a hub through its transaction translator with split transactions) deliver keys to the shell; tested in QEMU (`make test-v2-keyboard`: PS/2, `usb-kbd` on `qemu-xhci` and on `usb-ehci`, and a boot with no keyboard at all). **Not covered:** the split-transaction path (QEMU has no TT; needs the Sandy Bridge), keyboards behind hubs on xHCI, full/low-speed devices wired to an EHCI root port (companion UHCI/OHCI, no driver), hot-plug other than `usb rescan`. Verify on the Skylake and Sandy Bridge once reachable. |
| M4 (done) | Storage | AHCI, and USB Mass Storage (Bulk-Only) over xHCI and EHCI, behind one block layer; boot-disk discovery by the payload partition GUID the loader passes (BootInfo v5); FOSFS v3 read/write (byte-compatible with v1); `ls`/`cat`/`write`/`rm`/`disks`; `hostname`, `settings` (`deny-term-net`), `debug` (`reboot-on-crash`, `reboot-delay`, `crash-on-boot`) and `start` honoured. Tested in QEMU: boot from SATA (`make test-v2`), from a USB stick on `qemu-xhci` and on `usb-ehci` (`make test-v2-usb-boot`), configuration files and crash-reset (`make test-v2-config`). **Added later:** NVMe (`nvme.rs`: polling, one I/O queue, 512-byte namespace 1, boot from it tested in QEMU) and hubs on xHCI (`usb_hub.rs`: route strings, hub slot contexts, transaction-translator fields; keyboards, sticks and nested hubs tested on `qemu-xhci`, `make test-v2-hub`). **Confirmed on the Skylake (2026-10-03):** a 4-port USB 2 hub (1a40:0101) on a root port with a low-speed keyboard behind it, plugged in after boot and found by `usb rescan` (the machine would not POST with the hub attached, so it cannot be present at boot there). **Not yet:** USB sticks behind an EHCI hub's transaction translator beyond the Sandy Bridge case (the split path is hardware-only), NVMe on real hardware, NVMe interrupts and multiple queues.
| M5 (done) | Tapes | `TapeApi` v1 (one versioned table, `v2/abi/src/tape.rs`), tapes as ELF64 position-independent executables (relocated into a 4 MiB executable region, run on a stack of their own), `tape load NAME\|net` / `run` / `save` (the v1 `tape load net` protocol, so `send_tape.py` works), multi-NIC (up to 4 e1000, per-interface terminal session, promiscuous mode), and four tapes: `hello`, `calc`, `memtest` (memory + soak, no pools or long-mode excursion) and `nettest`. Tests: `make test-v2-tapes`, `make test-v2-nettest`. **Deviations from the sketch above:** tapes are PIE, not fixed-address (a fixed address may not be free RAM on a given firmware); `fs_remove`, `free_pages` and `nic_set_promiscuous` were appended to the table. **Not yet:** `tape-net` (needed by `router`/`bridge`/`ip-chat`, M8), audio (the network soak, `memtest soak net=A,B`, is back and tested over a QEMU hub cable), NIC interrupts (now done, see "NIC interrupts (MSI)" below).
| M6 (done) | Update and recovery | The loader runs the A/B slot state machine (one try for a pending kernel, automatic fallback) and tracks `running`/`crash_count` in the metadata sector; the kernel confirms a pending slot at its milestone; KUP over the network (v1 wire format, `tools/send_kernel_update.py` unchanged) writes the inactive slot and restarts; crash record in RAM (page `0x5000`), read back and printed by the next boot; reboot-on-crash through UEFI `ResetSystem(EfiResetWarm)`; crash-loop guard (five pre-milestone crashes in a row: the kernel stops resetting and halts); software watchdog (`watchdog [N]`, vector 240, longer limit while a tape runs, stays off after three unclean boots); `debug status\|slots\|crash\|hang`, `reboot`, `reset`. Test: `make test-v2-debug-mode` (crash + record, bad and good KUP, fallback after a pushed kernel crashes, watchdog, loop guard, KUP disabled). **Not done:** the NVRAM copy of the crash record (UEFI `SetVariable` from a fault handler is risky; warm reset is the mitigation for RAM-wiping resets, unverified on real hardware), KUP from inside a running tape is accepted but untested, and nothing here has run on the Skylake.
| M7 (running) | Skylake | **Done on the Skylake and the Sandy Bridge (2026-10-03):** UEFI boot from a stick through xHCI (Skylake) and EHCI behind the rate-matching hub (Sandy Bridge); boot-disk discovery, FOSFS, KUP over the network with promotion, `reboot`; five NICs on the Skylake (I219-LM + 4x I350) all on MSI, the 82579V on MSI; tapes run; `memtest` over all 15.9 GiB (addr+moving+random) passes in 130 s; `nettest` through the cabled I350 ports forwards 71,851 of 71,862 generated 1500-byte frames at 900 Mbit/s with 0 lost (the rest were in flight at stop). Real-hardware bugs found and fixed: a fifth NIC overflowed the interrupt tables; the first control transfer after a port reset failed on Intel xHCI (retries, 8-byte default pipe for low/full speed); the PCH EHCI halted with a host system error until its DMA frame was in low memory; the UEFI `ResetSystem` call page-faulted on the Skylake (reset now uses the ACPI reset register / port 0xCF9 first); the UEFI Shutdown call faulted too (`shutdown` now enters ACPI S5 from the DSDT `_S5_` object first); a router started with a dead uplink refused to start because the e1000 refused frames with no link (now dropped), and its late DHCP lease was never promoted to the active uplink (fixed in both routers); `dmesg` took minutes because the framebuffer was scrolled by reading write-combining memory. Also confirmed on the Skylake: `shutdown`, the watchdog (`debug hang`), a USB keyboard, `router` with NAT and a DHCP server (internet through it, also after a late uplink lease). **Not yet tried on hardware:** the crash-loop guard, `bridge`, hubs, NVMe, audio, the I219 under sustained load.
| M8 | The rest | Remaining NIC drivers, audio, `router`/`bridge`, USB HID keyboard, polish. |

Testing uses QEMU with OVMF (`OVMF_CODE_4M.fd` + a per-run copy of
`OVMF_VARS_4M.fd`, both present in this environment) and the serial console the
whole project already tests through. The v1 harness's habits carry over: bounded
waits, preserve serial output on failure, wait for the prompt before typing.

## 12. Compatibility and branching

- v1 (the current top-level tree) is maintained for the Pentium 4 and as the
  protected-mode teaching edition. Bug fixes land there; features may or may not.
- v2 develops under `v2/` on `main`. The two editions share the repository, the
  host tools and the docs; v1 remains the default build (`make` builds v1,
  `make v2-...` targets build v2) until v2 is good enough to promote.
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
   `make test-v2-loader` smoke test.
3. Boot that same image on the Skylake as an early reality check: if firmware
   quirks bite, we want to know before the kernel exists.

**Files on other disks.** Every command that takes a file accepts `NAME` (the boot volume), `diskN NAME` and `diskN/NAME`
(also `@LABEL NAME`, `@LABEL/NAME`), as v1 does: `cat disk2/audio.cfg`, `copy disk2/a.cfg disk1/b.cfg`,
`copy disk2/a.cfg disk1 b.cfg`, `write disk2/note hi`, `rm disk2/note`, `tape load disk2/audio-net`, `tape save disk2/x`,
`disk2/NAME args` (run a tape or script from that disk). The boot-time files (`start`, `hostname`, `settings`, `debug`)
and a bare name that a tape reads (`audio-net @audio.cfg`, `fs_read`) and a bare command word are searched on the boot
volume first, then on every other disk in order (`storage::volume_with_file`); writes with a bare name stay on the boot volume.
