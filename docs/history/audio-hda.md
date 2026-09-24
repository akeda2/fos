# History: ES1370 and Intel HDA audio drivers

This is the detailed, chronological development history of fos's audio
subsystem, moved out of `AGENTS.md` to keep that file within its size
budget. It covers the ES1370 driver from its first test-tone-only DAC1
increment through ADC/capture, multi-instance support, and the network-fed
`audio-net` tape, plus the Intel HDA driver from discover-only through
CORB/RIRB transport, multi-controller support, stream-DMA playback,
capture, and per-codec multichannel negotiation. See `AGENTS.md`'s own
"Real-time audio streaming" section for the current, condensed summary.

## Real-time audio streaming: the ES1370 driver

Raised by a real request: could fos support realtime audio streaming
between two dedicated machines, each with its own audio interface card,
talking over plain layer-2 Ethernet? The answer settled on after
discussion is "yes, as a genuine teaching-grade real-time exercise,"
provided two project-shaping decisions are kept: **audio and `router`
never run on the same machine** (this project's "do one thing at a time
and do it well" philosophy, applied literally -- each dedicated machine
gets its own single-purpose image), and this kernel's single-threaded,
polling-only architecture is *not* replaced with a general preemptive
scheduler just for this. A real scheduler was investigated and rejected
for exactly the reasons "Cooperative/preemptive scheduling" above already
gives: the single most disruptive cost (auditing/rewriting every place
that assumes single-threaded execution, especially `disk.rs`'s BIOS
real-mode trampoline) buys nothing once audio and router are guaranteed
never to compete for one CPU. What audio *does* need, and does not yet
have anywhere else in this kernel, is a real ISR that does non-trivial,
time-critical work instead of this project's usual "ack, count, flag,
defer to a foreground poll" convention -- see `kernel/src/es1370.rs`'s own
module doc comment for why that departure is deliberate and scoped
narrowly to this one driver.

**ES1370 was chosen as the first target the same way RTL8139 was chosen
as the first NIC:** it is QEMU's own emulated sound device
(`-device ES1370,audiodev=snd0`), so this driver is testable in CI right
now, without needing physical hardware for every iteration. It is also
directly relevant to real hardware this project expects to run on:
Creative's early "Sound Blaster PCI" cards (SB PCI64/128) are commonly a
rebranded Ensoniq ES1370 or its near-identical successor ES1371 -- a
Sound Blaster Live!/Audigy card is a *different*, unrelated chip
(Creative's own EMU10K1/EMU10K2), not part of this family, so a physical
card's exact identity should always be confirmed with `pci` first, the
same caution this project already applies to every other NIC/serial/
printer card.

Every register offset, bit, and DMA semantic in `kernel/src/es1370.rs`
was cross-checked directly against QEMU's own `hw/audio/es1370.c`
emulation (itself transcribed, with attribution, from Linux's historic
`drivers/sound/es1370.c`), the same rigor this project applies to every
other register-level driver after the RTL8139 lessons above. DAC1 (one
stereo 16-bit playback channel) and ADC (one stereo 16-bit recording
channel) are both implemented -- see "ES1370 ADC/capture support" below
for the ADC half; DAC2, the MIDI UART, and the joystick port are all
left untouched, and this driver does not program the external
AK4531-style mixer chip real hardware also carries (reachable through
the indirect `CODEC` register) -- QEMU's emulated backend has no such
gating, so this gap does not block testing here, but a physical card
may need its master/DAC (and, once a real microphone is attached, its
mic/ADC) volume unmuted through a separate path before audio is actually
audible or captured. All of this is, as of this writing, **unverified
against real hardware** -- expect real-hardware fixups the same way every
other PCI driver here needed them (see "Real hardware notes" above).

The driver programs a small double-buffered DMA ring (two periods of
1024 stereo 16-bit frames each, roughly 23 ms/period at 44.1 kHz) and
lets the hardware loop through both halves continuously once started;
`Es1370::handle_interrupt` -- installed as this device's legacy PCI IRQ
handler via the existing `interrupts::register_pci_irq` facade every
other PCI backend already uses -- refills whichever half just finished
playing each time `STATUS`'s `STAT_DAC1` bit is raised. A bare
`Cell<u32>`/`AtomicU32` pair (`tone_phase`, `interrupt_count`/
`periods_serviced`) is all the state this needs, precisely because the
work done per interrupt is small and bounded by construction (exactly
`PERIOD_FRAMES` samples, never a variable amount), unlike NIC frame
parsing.

### A genuine interrupt storm, not a missing interrupt -- confirmed via QEMU's own `-d int` trace

The first working version of `handle_interrupt` served the very first
period correctly (proven by a `-audiodev wav` capture containing real,
continuous, correctly-shaped test-tone audio) but reported **zero**
interrupts ever having fired, on every subsequent check, via a raw
counter (`interrupts::pci_irq_count`) incremented unconditionally in
this kernel's shared PCI-IRQ dispatch path before any device-specific
handler runs at all. Those two observations flatly contradict each
other under the assumption "no interrupt fired" -- and turned out to
both be true simultaneously, for a much stranger reason.

`STAT_DAC1` is a **level**, not an edge, and -- unlike RTL8139's `ISR`,
which a driver acknowledges by writing back the status bits it just
handled -- `STATUS` on this chip is entirely read-only. QEMU's own
`es1370_write` has no case for it at all. The *only* documented way to
lower an already-set `STAT_DAC1` is the specific transition ES1370's
datasheet (and QEMU's `es1370_maybe_lower_irq`) requires:
`SERIAL_CONTROL`'s `P1INTEN` bit going from 1 to 0. The original driver
never did this, so the first period's genuine, correct interrupt raised
the level and then left it stuck asserted forever. The instant that
handler's `iret` restored `EFLAGS.IF`, the still-asserted legacy PCI
`INTx#` line refired *immediately* -- before any other instruction,
including one that could print a counter or read a keystroke, ever ran
again. This was confirmed directly, not inferred: `qemu-system-i386 -d
int -D logfile` showed the identical faulting `EIP` re-entered over
188,000 times within a couple of seconds -- a genuine interrupt storm
that consumed effectively all CPU time from the moment DAC1 first
activated, starving the very shell command that would have reported a
nonzero count. QEMU's own `-trace enable=es1370_*` output corroborated
this from the device side: a real `irq=1` transfer trace event on
(roughly) every period, immediately followed by a repeating
`es1370_lost_interrupt` trace event -- QEMU's own internal detection
that the device wanted to re-assert an already-asserted level.

The fix, `Es1370::acknowledge_dac1`, toggles `P1INTEN` off then
immediately back on (rather than leaving it permanently disabled) right
after each period's refill -- clearing the level for that period while
keeping the next period's interrupt enabled, exactly mirroring the real
driver convention this chip's own datasheet documents. Confirmed fixed
three ways: the `-d int` storm disappeared entirely (the shell/keyboard/
timer stayed fully responsive throughout a multi-second test), the raw
and per-device interrupt counters agreed and incremented steadily (one
period roughly every 23 ms, matching `PERIOD_FRAMES`/44.1 kHz), and a
fresh WAV capture came back 100% non-zero with no discontinuities,
confirming glitch-free, continuously-refilled playback end to end.

This is a genuinely new failure class for this project's driver work,
worth naming plainly for future debugging: **a level-triggered interrupt
source that the driver never acknowledges does not look like "no
interrupt" from a symptom-only view -- it can look exactly like one**,
because the resulting storm can starve the very code path that would
otherwise reveal it. Every other PCI backend here (`rtl8139.rs`,
`threecom.rs`, `e1000.rs`) already acknowledges its own status register
on every interrupt as a matter of course, so this exact trap could not
recur there; ES1370 is simply the first device in this kernel whose
"how do I clear this" is a register *transition* instead of a
write-back, and that shape is worth checking explicitly for the next
driver that has anything resembling a persistent, level-style status
bit.

### Auditing every `cli`-disabled section in the kernel for audio-safe duration

Once ISR-driven refill was confirmed correct, the next real risk to
glitch-free playback is anything else in this kernel that disables
interrupts (`cli`) for long enough to make the CPU miss a whole DAC1
period (~23 ms at the current `PERIOD_FRAMES`) -- if that happens, the
hardware loops back and replays a stale half of the ring rather than
waiting, audibly. Every runtime `cli` site in `kernel/src/*.rs` (boot-time
`cli`s in `boot/*.S` and `kernel/src/entry.S` all execute before
interrupts are ever enabled, so they cannot affect a running audio
stream and are out of scope here) was reviewed for its worst-case
duration:

- **`disk::call_trampoline` (`kernel/src/disk.rs`) is the one real risk,
  by a wide margin.** Its `cli`-disabled span covers the *entire* BIOS
  `INT 13h` round trip through `boot/stage2.S`'s real-mode trampoline --
  fundamentally unbounded from this kernel's point of view, since it is
  however long real BIOS/firmware takes to complete one genuine disk
  transfer (easily tens of milliseconds on real spinning or USB media,
  dwarfing one 23 ms audio period many times over). Worse, this window's
  IDT is temporarily the *real-mode* IVT with the PIC remapped to
  BIOS-conventional vectors (see `call_trampoline`'s own doc comment) --
  even if `EFLAGS.IF` is set for part of this window, this kernel's own
  ES1370 interrupt gate is not reachable at all while it lasts, so any
  DAC1 interrupt that fires during a BIOS disk call is simply not
  serviced by this driver until the call returns, no matter how the real
  BIOS's own default interrupt stub handles it. A native ATA/AHCI driver
  would remove this entirely but is explicitly deferred elsewhere in this
  document, for unrelated reasons of its own scope. **The actionable
  mitigation is architectural, not a code change**: the dedicated audio
  machine must never perform disk I/O while a stream is active. This is
  exactly what "do one thing at a time and do it well" already implies in
  practice -- the eventual audio-streaming tape (see the project's own
  todo list) must load any configuration it needs *before* entering its
  streaming loop, and never touch FOSFS/disk again once streaming starts.
- **`interrupts::register_pci_irq`'s own `cli`/`sti` pair, `scancode_queue_stats`/`pop_scancode`'s
  (`interrupts.rs`), and `read_u64_atomically`'s** are all a handful of
  register/array reads or one PIC port write apiece -- sub-microsecond,
  negligible next to a 23 ms budget regardless of how many times any of
  them run.
- **`keyboard::set_caps_lock_led` (`kernel/src/keyboard.rs`) is the one
  other real, if much smaller, finding.** It holds `cli` across up to two
  bounded 8042-controller polling loops (`wait_for_input_empty`/
  `wait_for_output`, `100_000` iterations each, the same
  bounded-poll-then-give-up shape `parallel.rs`'s `BUSY_POLL_ATTEMPTS`
  already uses elsewhere) -- normally a handful of microseconds in the
  overwhelmingly common case where the 8042 responds immediately, but
  theoretically up to on the order of 100 ms in the pathological case
  where the controller never responds at all. This is deliberately left
  as a documented, accepted, rare edge case rather than a code change:
  it only triggers on an actual Caps Lock keypress, and this project's
  own single-purpose-machine philosophy already implies a dedicated
  audio-streaming machine is not expected to have an operator typing on
  it interactively while a stream is live.

No code changes were needed for the second and third bullets; the first
is recorded here as a hard design constraint on the eventual
audio-streaming tape rather than a kernel change, consistent with this
project's preference for a small, explained trade-off over speculative
complexity elsewhere in this document (e.g. the floppy `A`/`B` naming
collision, the `DISK2`-named-file collision).

### The `audio` shell command

`audio` (bare, no arguments, mirroring `pci`'s own shape) reports
whether an ES1370 (or compatible) controller was found and its I/O base,
lazily performing PCI discovery/DMA-buffer allocation/IRQ registration
and starting DAC1 playback (currently a fixed ~441 Hz test tone -- see
`Es1370::fill_period`'s own doc comment -- standing in for a real network
audio source until the streaming transport below is wired in) on its
first call. It also prints the raw and DAC1-recognized interrupt counts
(`interrupts::pci_irq_count`/`Es1370::interrupt_stats`), the count of
periods refilled so far, and the raw `STATUS` register value -- the same
diagnostic depth `pci`/`irqstat` already provide, and exactly what made
the interrupt-storm bug above possible to root-cause from a serial-only
test harness with no audio hardware of its own to listen to.

### `audio.rs`: a format-agnostic facade, and network-fed playback

`kernel/src/audio.rs` mirrors `nic.rs`'s own shape deliberately: a small
enum (`Audio`) wrapping every supported backend (currently one,
`Audio::Es1370`), a lazy `initialize()`/`get()` pair, and a stable,
format-agnostic contract both the tape ABI and the `audio` shell command
build on -- so a second codec driver (an AC97 or HD-Audio card, neither
implemented yet) can slot in the same way a second NIC backend already
does, without either caller changing. This split exists specifically
because ES1370 is a genuine *silicon* ceiling, not a driver choice: its
DAC1 rate generator only offers four hardwired rates (5512/11025/22050/
44100 Hz) and its sample-format field only encodes 8- or 16-bit PCM (see
`es1370.rs`'s own register doc comments) -- this project's real Sound
Blaster PCI hardware target shares that ceiling. A future 24-bit/48 kHz
card should not need `audio.rs`, the ring buffer, or the tape ABI's
`audio_submit` callback to change at all; a tape must always ask the
active device its real format (`AudioFormat`, via the ABI's
`audio_sample_rate`/`audio_channels_and_bits` callbacks) and produce
exactly that format -- this kernel does no sample-rate conversion or
bit-depth requantization of its own, the same "small kernel API,
protocol/format logic stays in tape code" split this project already
applies to NIC frame parsing.

The tape ABI grew three more trailing callbacks (append-only, cdecl,
after the existing `nic_*`/`ticks` set): `audio_sample_rate() -> u32` (0
if no device), `audio_channels_and_bits() -> u32` (channels in the low
byte, bits-per-sample in the next byte, 0 if no device -- packed into one
u32 deliberately, to avoid a fourth trailing parameter for what is really
one small format fact), and `audio_submit(data: u32, length: u32) -> u32`
(pushes raw PCM bytes into the network-fed ring, returning how many bytes
were actually accepted -- a short count means "ring full, retry the
remainder shortly," mirroring `nic_send`'s own short-write convention).
Calling `audio_submit` at all permanently switches the card from its
default built-in test tone into network-fed mode (`Es1370`'s
`network_mode` flag) -- there is no way back to the test tone without a
reboot, the same one-way switch `tape_load_net`'s "first sender locks the
attempt" convention uses elsewhere in this project.

**The network-fed ring buffer, and a real sizing pitfall worth
remembering.** `PcmRing` (`audio.rs`) is a fixed-capacity, lock-free,
single-producer/single-consumer byte ring -- exactly the same shape and
safety argument as `console`'s own `RELAY_QUEUE`/`interrupts`'s scancode
queue: monotonically increasing `write_pos`/`read_pos` counters, a
`Release` store of `write_pos` only after every byte is written, and an
`Acquire` load of it before the consumer (the DAC1 ISR's own period
refill, `Es1370::fill_period`) touches any byte it implies is valid.

A first attempt sized `RING_CAPACITY_BYTES` generously against the
*worst* format this project expects to ever support (24-bit stereo at
48 kHz, 384,000 bytes/sec) at 64 KiB -- and that measurably does not fit.
Investigating why turned up two separate, easy-to-conflate effects,
worth spelling out precisely since a future contributor sizing another
large `.bss` array here could otherwise draw the wrong lesson from either
one alone:

- **`objcopy -O binary` does not bake `.bss` into `kernel.bin` at all,
  contrary to an earlier (now corrected) claim in this document's own
  size-budget history above.** `.bss` is `linker.ld`'s last section, with
  nothing loadable after it; `objcopy`'s raw-binary output simply
  truncates right after `.data` and never extends out to `.bss`'s full
  virtual size, regardless of how large any `.bss` static declares itself
  to be. This is *why* `kernel.bin`'s own file size stayed byte-identical
  across many different `RING_CAPACITY_BYTES` values during bisection --
  a red herring that has nothing to do with the array being "dead" or
  "live." `kernel/src/entry.S` is what actually makes a large `.bss`
  array safe at runtime: it zero-fills the whole `__bss_start`..
  `__bss_end` range with an explicit loop before any Rust code runs, the
  same way any freestanding kernel's startup stub must, precisely because
  the loaded image itself never contains those bytes.
- **The real, enforced budget is the link-time `ASSERT(__kernel_end <
  0x60000, ...)` in `linker.ld`**, which *does* depend on `.bss`'s true
  linked size (the address arithmetic that computes `__kernel_end`, not
  the raw file's byte count) and *does* depend on whether `--gc-sections`
  actually keeps a large unreferenced static's section at all. Bisecting
  through the real `make kernel` recipe (not a bare `cargo build
  --release`, which uses different `-Z build-std` flags and gives
  misleading numbers here) found a genuine, reproducible LLVM/linker
  cliff for a `[0u8; N]` initializer this large: `N = 32768` links,
  `N = 40960` does not -- confirmed identical whether `submit`/
  `audio_submit`/`audio_status` are genuinely reachable, live code paths
  or not, so (unlike a first, since-retracted theory) this is not
  actually a dead-code-elimination artifact at all; it is a real codegen
  threshold for how large a zero-initializer LLVM will still fold away
  cheaply before it has to be emitted as a real, full-sized `.bss`
  region. `RING_CAPACITY_BYTES` was set to 16 KiB -- comfortably inside
  that threshold, covering ~93 ms of jitter tolerance at ES1370's current
  44.1 kHz/16-bit/stereo format (176,400 bytes/sec), and ~43 ms at the
  hypothetical 24-bit/48 kHz future ceiling. A future codec driver
  needing more headroom at that higher rate should re-measure this same
  real link budget rather than assuming today's 32 KiB/40 KiB cliff still
  holds once other kernel code has grown or shrunk around it.

`audio_status()` (the `audio` shell command's handler) was rewritten to
go through this facade (`audio::get()`) instead of calling `es1370::get()`
directly, so it keeps working unchanged once a second backend exists, and
now also reports the network-fed ring's current backlog
(`Audio::ring_backlog()`/`Es1370::ring_backlog()`, in bytes out of
`audio::RING_CAPACITY_BYTES`) and underrun count
(`Audio::underrun_count()`) -- useful for telling a healthy, steadily-fed
stream apart from one about to glitch, without waiting for an actual
underrun to happen first.

No dedicated audio-streaming tape exists yet to actually drive
`audio_submit` end to end over the network (see "Deliberately deferred
work" -- this is the next planned increment); this section documents the
kernel-side facade, ABI, and sizing investigation only.

### `tapes/audio-net/`: network-fed playback, and a file-replay stand-in for live capture

`tapes/audio-net/` is the dedicated tape driving `audio_submit` end to
end over the network -- the increment the previous section's closing
note flagged as not yet built. It shares `CHAT_ETHERTYPE` (`0x88b6`) with
`chat`/`nic discover`/`tape`/`term net`/NETFS/KUP, adding three new tags
continuing that numbering: `AUDIO_TAG_START` (`0x40`, declares the
stream's sample rate/channel count/bits-per-sample so a format mismatch
against the local device can be rejected up front rather than played as
noise), `AUDIO_TAG_DATA` (`0x41`, one PCM chunk), and `AUDIO_TAG_STOP`
(`0x42`, ends the stream cleanly). Unlike `CHAT_TAG_TEXT`/`TERM_TAG_DATA`,
which trim trailing zero padding on receipt, `AUDIO_TAG_DATA` carries an
explicit 16-bit length prefix instead: legitimate 16-bit signed PCM
silence is genuinely all-zero bytes, and must never be confused with
Ethernet's own 60-byte minimum-frame padding the way printable text safely
can be. Like `TERM_TAG_DATA` (and unlike `TAPE_TAG_DATA`'s strict,
abort-on-gap sequence numbering), there is deliberately no sequence number
at all: this is a live, ongoing best-effort stream where an occasional
dropped frame should read as a brief audio glitch, not a fatal transfer
error worth aborting over.

The tape has two modes. `tape run` (no arguments) is **listen mode**: it
queries the local device's actual format via `audio_sample_rate()`/
`audio_channels_and_bits()`, prints it,
then waits for an `AUDIO_TAG_START` from any peer -- locking onto the
first sender whose declared format matches (a mismatched stream is
rejected with a clear message, but the tape *keeps waiting* rather than
exiting, so a misconfigured first sender doesn't force a reboot/rerun
just to try a corrected one) -- and forwards every `AUDIO_TAG_DATA`
payload into `audio_submit`. Because `audio_submit` has short-write
semantics (mirroring `tape_nic_send`'s own convention: it returns bytes
actually accepted, which may be less than requested if the ring is
momentarily full), `listen_mode` retries the remainder in a bounded
64-spin loop and only counts genuinely undelivered bytes as dropped once
that bound is exceeded, rather than blocking indefinitely. `tape run send
NAME [PEER_MAC|PEER_NUMBER]` remains a stand-in for live capture: even
though `kernel/src/es1370.rs` now implements real ADC/recording support
(see "ES1370 ADC/capture support" below) and exposes it through the tape
ABI's `tape_audio_capture` callback, this tape has not yet been given a
mode that reads it -- `send` still replays a
pre-recorded, headerless raw PCM file already on FOSFS (read whole via
`fs_file_length`/`fs_read` into an `alloc()`-backed buffer, capped at 16
MiB, since the FOSFS tape callback ABI has no partial/offset read) at a
fixed 44100 Hz/2ch/16-bit format -- this project's real ES1370 ceiling,
not an arbitrary choice -- pacing `AUDIO_TAG_DATA` chunks (1024 bytes
each) against `ticks()`'s 100 Hz PIT count using a computed absolute
target-tick schedule (`start_ticks + offset * PIT_HZ / bytes_per_sec`)
rather than a fixed per-chunk sleep, to avoid cumulative drift over a
long transfer. A genuine live-capture send mode (reading
`tape_audio_capture` in a loop instead of a file) is the natural next
step toward the user's "ADC to DAC, and eventually full-duplex" goal,
but remains unbuilt as of this writing.

Verified end to end with `tapes/test_audio_net_e2e.py` (not part of
`make test`, mirroring every other tape's own `test_*_e2e.py` harness):
a single QEMU guest carries both an RTL8139 (the ordinary multicast
Ethernet segment) and a QEMU-emulated ES1370 captured to a host-side WAV
file via `-audiodev wav,id=snd0,path=...`. The test script itself acts as
the sending peer -- constructing and sending the exact
`AUDIO_TAG_START`/`DATA`/`STOP` wire frames directly, the same protocol a
real `tools/send_audio.py` or the tape's own `send` mode would use --
streams a short synthetic 440 Hz tone into the tape's listen mode, and
confirms both the tape's own byte-count summary (received == sent,
dropped == 0) and, the stronger proof, that the captured WAV file
actually contains real, non-silent audio matching the sent tone (a peak
amplitude comfortably above a silence-noise floor), not just that the
tape's own counters looked right.

### A real bug this test found: shared legacy PCI IRQ lines silently broke a second device's `init()`

Building the test above surfaced a genuine kernel bug, not a test
artifact: the ES1370 device consistently failed to initialize
(`audio_status()` reporting "An audio controller was found but could not
be initialized") the moment an RTL8139 NIC was also attached to the same
QEMU guest. `info pci` at the QEMU monitor showed why: QEMU's i440fx/
PIIX3 IRQ router assigned *both* devices to the same legacy line, IRQ 11
-- normal, expected PCI behavior (a real motherboard's fixed slot wiring
can do the same), not an emulator quirk. `interrupts::register_pci_irq`
(`kernel/src/interrupts.rs`) only ever supported one handler per IRQ
line: a second, different device registering on an already-claimed line
was rejected outright, `es1370::initialize()` treated that rejection as
a fatal `Status::InitFailed`, and the whole device came up unusable --
silently, with nothing about the failure pointing at the real cause.

This was a real, previously-undiscovered gap, not specific to ES1370:
*any* two PCI backends this project has (two NICs, a NIC plus ES1370, a
future I350 alongside anything else) could hit the identical failure the
moment QEMU's (or a real motherboard's) IRQ router happened to assign
them the same line -- which audio-plus-networking, the two device
classes this project's whole current priority is built around, makes a
routine combination to hit, not a rare edge case.

The fix: `PCI_IRQ_HANDLERS` grew from one `Option<IrqHandler>` slot per
line to a small fixed-size array (`MAX_SHARED_HANDLERS` = 4 -- the same
bounded-fixed-array convention this kernel already uses everywhere else,
comfortably covering any realistic sharing scenario this project's small
number of PCI backends can produce). `register_pci_irq` now searches for
an existing matching handler (idempotent re-registration, unchanged),
then an empty slot, and only fails if the line's handler capacity is
genuinely exhausted; `dispatch_pci_irq` now calls every handler
registered on that line instead of at most one. This is safe by
construction, not just by observation: every backend's own
`handle_interrupt()` already checks its own device's status register
before doing anything (`nic.rs`'s documented "ack/count/flag, nothing
else" design, and ES1370's own `handle_interrupt` checks `STAT_DAC1`
the same way), so a handler that isn't the actual interrupt source
simply finds its own status bit clear and no-ops -- calling every
handler on a shared line costs nothing beyond that one harmless check.
`pci_irq_registered` (used by `nic`/`audio`'s own diagnostics to decide
whether to report raw IRQ fire counts) was updated to mean "at least one
handler is registered on this line," preserving its existing meaning for
every caller. Confirmed fixed by rerunning the same reproduction (`info
pci` showing IRQ 11 shared between the RTL8139 and ES1370, `audio`
reporting successful discovery afterward) and by rerunning `make test`,
`make test-nic-watchdog`, `make test-stack-guard`, and
`make test-page-protection` -- all pass unmodified, confirming this is a
targeted fix with no regression to the existing single-handler-per-line
cases those tests already exercise.

### ES1370 ADC/capture support: implemented

The user's stated project goal, once `tapes/audio-net/` above proved the
network-fed playback path works end to end: real microphone input (ADC
-> network -> speaker output, DAC), and eventually full-duplex
(simultaneous capture and playback) as a stretch goal. `kernel/src/
es1370.rs` now implements DAC1 (playback) and ADC (recording) as two
independent, simultaneously running DMA channels sharing one chip; DAC2,
the MIDI UART, and the joystick port remain untouched. Every ADC register
offset, bit, and DMA/interrupt semantic was re-derived from QEMU's own
`hw/audio/es1370.c`, the same source and rigor used for DAC1: ADC's
`FRAMEADR`/`FRAMECNT` registers alias DAC1's own `0x30`/`0x34` offsets,
distinguished only by which page (`PAGE_ADC` = 0x0d vs. `PAGE_DAC` =
0x0c) is selected in `MEM_PAGE` before writing them; `CTRL_ADC_EN =
1<<4`; `CTRL_PCLKDIV` (bits 28:16) is a clock divider shared between ADC
and DAC2, precomputed here as a fixed `CTRL_PCLKDIV_44100` constant for
the driver's one supported rate (`divider = ((1411200 + rate/2)/rate) -
2`); `SCTRL_R1INTEN = 1<<10`/`SCTRL_R1FMT_STEREO16 = 0b11<<4`; and
`STAT_ADC = 1<<0` is an independent, level-triggered status bit distinct
from `STAT_DAC1 = 1<<2` -- both can be set simultaneously on one
interrupt, so `handle_interrupt` checks each unconditionally rather than
treating them as an if/else.

**The one real bug this surfaced, caught before it ever reached real
hardware:** `CONTROL` and `SERIAL_CONTROL` are genuinely *shared*
registers -- `CONTROL`'s `CTRL_PCLKDIV` field and DAC-vs-ADC enable bits,
and `SERIAL_CONTROL`'s per-channel format/interrupt-enable bits, all live
in the same 32-bit register regardless of which channel a given bit
belongs to. The DAC1-only version of `start_dac1`/`acknowledge_dac1`
wrote a fixed, hardcoded bit pattern to each register -- harmless with
only one channel ever touching them, but it would have silently
clobbered whichever channel's bits it didn't intend to set the moment a
second channel existed. Every one of these functions
(`start_dac1`/`start_adc`/`acknowledge_dac1`/`acknowledge_adc`) was
rewritten to read-modify-write instead: read the current value, flip
only the bits that function owns, write it back. Confirmed correct by
booting with both channels enabled and observing DAC1's own period count
keep advancing at its expected rate the entire time ADC is also running
(and vice versa) -- if either write path had clobbered the other
channel's enable/format bits, one channel would have silently stalled.

`audio.rs`'s facade gained a read-direction counterpart to
`audio_submit`: `Audio::capture`/`capture_overrun_count`/
`capture_backlog`/`capture_periods`, backed by a second, independent
`PcmRing` (`CAPTURE_RING`) and a new `PcmRing::pop_upto` (a variable-
length "however much is ready" pop, unlike `pop_into`'s all-or-nothing
shape, mirroring `tape_nic_receive`'s own convention). The tape ABI grew
a matching `tape_audio_capture(out, capacity) -> u32` callback (the
final, append-only `TapeEntry` cdecl argument), and `audio_status()` now
reports the ADC's own period-captured/overrun counts and capture-ring
backlog alongside DAC1's existing diagnostics.

**Design choice: ADC runs unconditionally, exactly like DAC1.** `Es1370::
initialize()` calls `start_adc()` right after `start_dac1()`,
unconditionally -- there is no "only start recording once a tape asks
for it" gate. This mirrors DAC1's own always-on design (silence plays
continuously even with nothing feeding `audio_submit`) for the same
reason: a second, always-running DMA channel costs nothing observable
when unused, and it means `audio_status()`'s ADC diagnostics are always
meaningful immediately after boot, not only after some tape has opted
in. Captured bytes nobody ever reads via `tape_audio_capture` simply
overflow `CAPTURE_RING` and increment its overrun counter once full --
the same harmless "recording into a room nobody is listening to"
behavior a real always-on ADC would have, not a fault condition.

**Reserved low-memory region grew again, to `0x70000`.** ADC's second
DMA capture buffer and its associated code pushed the kernel past the
previous `0x60000` ceiling (see "Low-memory map" for why all four
constants -- `kernel/linker.ld`, `kernel/src/pmem.rs`'s
`RESERVED_BELOW`, `tools/write_layout.py`'s `KERNEL_RESERVED_END`, and
`tools/write_netrom_layout.py`'s `MAX_KERNEL_BYTES` -- must always move
together). One non-obvious wrinkle surfaced while sizing this: a naive
`size`-command `text+data+bss` sum undercounts the real footprint,
because `linker.ld` 4096-byte-aligns the `.text`/`.rodata`/`.data`
section boundaries, adding padding that sum doesn't reflect -- `readelf
-S`'s real final `.bss` end address is the number that actually matters
against the `ASSERT` ceiling. `tools/write_layout.py`'s `SLOT_SECTORS`
(and therefore `tools/build_image.py`'s A/B kernel slot sizing) is
derived automatically from `KERNEL_RESERVED_END`, so growing that one
constant alone was sufficient -- no separate disk-layout constant needed
updating.

**Verified with `tapes/test_audio_capture_e2e.py`** (not part of `make
test`, mirroring every other tape's own harness): using QEMU's `none`
audiodev backend (bidirectional -- it opens both an out and an in voice
-- but silence-only), the test confirms DAC1 and ADC periods both
advance simultaneously and independently (proving the shared-register
read-modify-write fix above actually works, not just compiles), and that
`CAPTURE_RING` -- deliberately never drained in this test, since no tape
calls `tape_audio_capture` -- correctly fills to capacity and then
reports a steadily incrementing overrun count rather than wedging or
corrupting state. **This cannot verify real, non-silent captured audio
content**, unlike the DAC1/`tapes/test_audio_net_e2e.py` test, which can:
QEMU's own `audio/wavaudio.c` (confirmed by reading its source directly)
implements only `HWVoiceOut` in this project's installed QEMU version
(10.2.1) -- there is no `in.path`/file-sourced fake-microphone input
option at all, despite that combination appearing in some online
examples written against other QEMU versions or forks. Verifying actual
captured sample values therefore needs either a host environment with a
real, usable audio input backend (ALSA/PulseAudio/PipeWire, generally
unavailable in a sandboxed dev/CI container) or real hardware, the same
honestly-documented gap this project already accepts for other
QEMU-model-limited features (e.g. the 3C509/I350 real-hardware-only
testing cost documented above).

Full-duplex (a tape both feeding `audio_submit` and reading
`tape_audio_capture` in the same loop) is now mechanically unblocked --
both channels run independently and simultaneously. `tapes/audio-net/`
now has three modes dispatched from its startup argument: bare
`tape run` (listen mode, unchanged), `tape run send NAME [MAC]` (the
pre-recorded-file replay stand-in described above), and
`tape run capture [MAC]` -- the genuine live-capture send mode this
section's closing note originally called for, described next.

### `tape run capture`: genuine live-microphone streaming

`capture_mode` (`tapes/audio-net/src/main.rs`) is the ADC counterpart to
`listen_mode`: it reads the local device's real format via the same
`audio_sample_rate`/`audio_channels_and_bits` callbacks `listen_mode`
already used, sends one `AUDIO_TAG_START` declaring it, then loops
pulling bytes out of `tape_audio_capture` (non-blocking, "however much
is ready" semantics -- see "ES1370 ADC/capture support" above) and
forwarding them as `AUDIO_TAG_DATA` frames via `nic_send`, exactly
mirroring `send_mode`'s own pacing/chunking but sourced from the live
capture ring instead of a FOSFS file. `should_abort()` is checked
unconditionally at the top of every loop iteration, before the
non-blocking capture call, so a remote or local abort is expected to be
noticed within one iteration regardless of whether any audio happens to
be flowing that moment.

Verified correct end to end using QEMU's `none` audiodev backend (no
real audio device at all -- `audio_capture` simply returns 0 bytes most
iterations): streaming starts, `nic_send`s real (silent) PCM frames at
the expected 44.1 kHz/stereo rate, and a remote abort byte sent partway
through is detected and handled in the same call, printing
`"audio-net: capture stopped locally after N byte(s) sent."` and
returning cleanly to the shell prompt -- proving the ABI wiring,
argument parsing, and abort-polling logic here are all correct. See
the next section for what happened -- and did not happen -- when this
same mode was pointed at a genuine, physical microphone on a real host
machine, and why that gap was not chased further.

### Real-microphone testing on a dev machine: QEMU audiodev backend pitfalls (host-environment, not a fos bug)

Answering "can a real microphone on this dev machine feed `tape run
capture`?" surfaced two host-side QEMU audiodev quirks, neither of which
is a fos kernel or tape bug -- both are recorded here since they cost
real time to isolate and would otherwise be rediscovered the hard way by
the next person trying the same thing on a similarly PipeWire-based
Linux dev machine.

**`-audiodev alsa,...` deadlocks completely against a PipeWire-provided
ALSA compatibility layer.** On this machine (PipeWire providing the
system's ALSA plugin, as is now the Linux desktop default), pointing
QEMU's `alsa` audiodev backend at the "default" device -- for either
direction (`in.dev`/`out.dev`) independently -- hung the entire guest
indefinitely: not just the audio subsystem, the whole VM stopped
responding to anything, including plain keypresses, for 120+ seconds
with zero progress. Plain host-side `aplay`/`arecord` against that same
"default" device in the same shell session completed instantly,
confirming the underlying audio pipeline was healthy and the hang was
specific to QEMU's own `alsa` backend implementation, not this machine's
audio configuration. This is a known category of bug: QEMU's `alsa`
backend's own polling/threading assumptions don't fully match the
semantics PipeWire's ALSA-compatibility shim presents, unlike a
genuine, unmediated ALSA driver talking directly to hardware. **Fix**:
use QEMU's native `-audiodev pipewire,id=snd0` backend instead --
property names differ (`in|out.name=<sink/source>`, not `in.dev`/
`out.dev`, which are `alsa`-backend-specific); omitting them entirely
uses PipeWire's own default routing. With `pipewire`, the `audio` shell
command returned instantly and reported correct ES1370 register state.

**The originally reported "`tape run capture` hangs on real audio +
remote abort" was not a real bug at all -- it was two bugs in the ad hoc
test harness's own output-parsing logic.** A first pass genuinely
reproduced what looked like a 40+ second hang and, lacking `strace`/`gdb`
access (see below for why that belief was itself wrong), that was
tentatively attributed to a QEMU/PipeWire lock-ordering deadlock. Once
`strace`/`gdb` were confirmed usable after all (see below), re-running
the exact same scenario -- both a single guest launched directly under
`strace -f`, and the full two-guest demo, several times each -- never
reproduced any hang at all: every remote-abort round trip completed in
under a second. Instead, the demo script itself had two bugs, both
instances of a general class worth watching for in any `term_net`
byte-at-a-time-relayed test harness:
1. A checkpoint-ordering race: one guest's output-buffer length was
   captured as a search starting point *after* a slow round trip on the
   *other* guest, whose output kept streaming into that same buffer the
   whole time in the background -- so the checkpoint ended up placed
   *after* the very text it was meant to search for, and the subsequent
   bounded `read_until` call simply timed out looking in the wrong
   place.
2. A marker-prefix race: a `read_until` marker
   (`b"capture stopped locally"`) that is itself a strict prefix of the
   full line (`"capture stopped locally after N byte(s) sent.\n"`,
   assembled from three separate `print_bytes`/`print_u32`/`print_bytes`
   calls, each relayed one byte at a time over `term_net`) can match
   before the rest of the line has actually arrived, so a later regex
   against the still-partial buffer fails. This is the exact same bug
   class already documented above for the `"audio"` command's own marker
   (fixed there by using `b"Raw STATUS register"` instead of `b"ADC:"`).
   **The general lesson: a `read_until` marker must always be the true
   tail of the line a subsequent regex will parse, never merely an
   early-appearing prefix or substring of it**, or the exact same class
   of intermittent, timing-dependent failure can recur in any future
   `term_net`-driven test.

Both bugs are now fixed in
`tapes/test_audio_net_real_mic_e2e.py`, committed but deliberately kept
outside `make test` -- a manual, environment-dependent demo, since its
"real microphone" half only works with a physical audio input device
actually present and reachable on the host; see below.

**A second, more interesting false positive surfaced once the harness
bugs above were fixed: the demo could report "PASS: real, non-silent
audio flowed" even when the ADC channel had captured zero real bytes.**
`kernel/src/audio.rs`'s DAC1 channel runs its own default idle test tone
continuously and unconditionally regardless of network activity (see
"The `audio` shell command" above) -- so a captured WAV file being
non-silent proves nothing about whether real network-relayed audio ever
reached it; the demo's own before/after `audio` status check (`ADC: 0
periods captured; overruns (ring full): 0` before *and* after an 8-second
capture attempt, while `DAC1` periods kept advancing) revealed that on
this specific sandboxed dev machine, the ADC channel was capturing
*literally nothing at all*, not even digital silence (silence would
still count as nonzero captured bytes, since the ADC produces a steady
sample stream regardless of amplitude). `wpctl status` confirmed the
root cause directly: PipeWire on this host has **no audio Source node
registered at all** -- only a dummy/virtual sink for output -- because
the system is configured to expect a specific USB audio CODEC input
device that is not actually connected in this sandboxed environment.
QEMU's `pipewire` audiodev backend degrades silently in this situation
(no error, just an ADC channel that never produces a single sample),
which is exactly what made the false positive possible. `tapes/test_audio_net_real_mic_e2e.py`
was fixed to assert `bytes_sent > 0` and `bytes_received > 0` from the
tapes' own reported network byte counts before ever trusting the WAV
file's amplitude -- a captured WAV's non-silence is only meaningful
*after* confirming real bytes actually flowed over the network, not on
its own.

**Root cause of the missing PipeWire Source, precisely identified: this
sandbox's SSH session is not the seat0 session.** This dev machine is a
physical laptop, normally accessed by SSH rather than its own screen/
keyboard; after a host power outage, its physical desktop session had
not been logged back into again. `loginctl list-sessions`/
`seat-status seat0` showed `seat0`'s only active session was still
`gdm-greeter` (the login screen itself, class `greeter`, on `tty1`) --
every `akeda` session (including this one) showed `SEAT -`, i.e. not
seat-assigned at all, because an SSH login is never attached to a
physical seat the way a graphical desktop login is. This matters
because the laptop's real built-in microphone device node
(`/dev/snd/pcmC0D0c`, "HDA Intel PCH Headphone Mic") is udev-tagged
`uaccess` -- systemd-logind grants read/write ACL on `uaccess`-tagged
device nodes only to whichever session is the *active* one on a given
seat, confirmed directly: `getfacl /dev/snd/pcmC0D0c` showed an ACL
entry for `gdm-greeter` and none at all for `akeda`. So this SSH
session's own PipeWire user instance (`akeda`'s systemd `--user`
manager) never receives access to the real hardware capture device at
all, regardless of how QEMU's `pipewire` audiodev backend is
configured -- it correctly shows only a dummy/virtual sink and no
Sources, because from logind's point of view `akeda` genuinely has no
seat-granted device access right now. `wpctl status`'s "Default
Configured Devices" entry naming a USB audio CODEC input is just
WirePlumber's cached last-known configuration from whenever someone
last logged into the desktop with that device attached -- it does not
mean the device is currently reachable, and is not itself proof of a
missing microphone (check the live `Audio -> Sources` list instead, as
this section's earlier `wpctl status` output demonstrates).

**Fix**: once someone physically logs into (or is auto-logged into) the
laptop's own desktop session -- making an `akeda` session the *active*
one on `seat0` instead of `gdm-greeter` -- logind will grant that
session's PipeWire instance the same `uaccess` ACL on the real hardware,
and the whole `tapes/test_audio_net_real_mic_e2e.py` pipeline should be re-run at that
point to get the final, genuine non-silent-signal confirmation this
section has been building toward. This is a same-machine, purely local
prerequisite (nothing to do with SSH access, network config, or fos
itself) -- re-run `wpctl status` after logging in and confirm a real
device (not just the dummy sink) appears under `Audio -> Sources`
before re-attempting the demo.

**Practical takeaway**: the tape/kernel-side capture ABI and logic
(`capture_mode`'s `should_abort`/`audio_capture`/`nic_send` loop,
`Es1370`'s ADC DMA channel, remote-abort handling) are all proven
correct -- confirmed with QEMU's `none` backend (silent digital data,
correct byte rate, instant remote-abort response) and now also with
`pipewire` (correct instant remote-abort response; zero hang ever
reproduced across many runs). What remains unverified is a **genuine
non-silent captured signal**, blocked purely by the seat0/logind
device-ACL issue just diagnosed above, not by any fos or QEMU bug --
tracked as the `audio-real-mic-verify` follow-up until someone logs
into the laptop's desktop session in person.

**Corrected: `strace`/`ptrace` access in this sandbox.** An earlier
version of this section claimed this sandbox lacks `CAP_SYS_PTRACE`
entirely and so cannot use `strace`/`gdb` at all -- that claim was
tested directly and found wrong. The real, verified picture: `strace`
uses the `ptrace` system call to intercept and log every syscall a
process makes (e.g. showing a `futex(...)` call it's blocked in, or a
`poll`/`read` waiting on a file descriptor); `gdb` also uses `ptrace`,
to pause a process and inspect its actual call stack/thread state.
**Launching a target process directly under either tool
(`strace -f -o log <command>`, `gdb --args <command>`) works today with
zero privilege changes** -- this is ordinary parent-traces-child
`ptrace`, which Linux's Yama LSM always permits regardless of scope
setting, confirmed by directly running `strace -f -o log
qemu-system-i386 ...` in this environment with no error. Only
*attaching* to an already-running, unrelated process
(`strace -p <pid>` / `gdb -p <pid>`) is blocked here, confirmed via a
live test (`ptrace(PTRACE_SEIZE, ...): Operation not permitted`),
because `/proc/sys/kernel/yama/ptrace_scope` is `1` ("restricted":
ptrace only between a direct parent/child, or if the target opts in via
`PR_SET_PTRACER`) -- a normal, unprivileged-process posture
(`CapEff` empty, `CapBnd` nearly full), not a container-specific
restriction. To also enable attach-mode tracing, in descending order of
convenience: prefer direct-launch tracing when the target process can be
started that way at all (covers the overwhelming majority of cases, as
it did here); `sudo sh -c 'echo 0 >
/proc/sys/kernel/yama/ptrace_scope'` to relax to classic same-uid
ptrace rules (needs interactive `sudo`, resets on reboot); or
`sudo setcap cap_sys_ptrace+eip <binary>` to grant the capability to one
specific tool binary, mirroring this project's existing
`python3-netraw` convention (see "Talking to a real fos machine from a
sandboxed dev environment" above).

### The `audio-real-mic-verify` follow-up, resolved: a genuine non-silent capture, and a QEMU TCG/BQL timing artifact found along the way

The prior section's blocker was purely a seat0/logind device-ACL issue on
one specific laptop, tracked as `audio-real-mic-verify` until someone
could test from a machine with a real, reachable microphone. That
happened on a different physical dev machine ("bengt"): a genuine active
seat0 session (not just SSH) and a real PipeWire `Audio -> Sources` entry
were confirmed present via `wpctl status`, closing out that
prerequisite.

**A real, unrelated bug in the test script itself surfaced first.**
`tapes/test_audio_net_real_mic_e2e.py` resolved `sys.path`/`ROOT`/`IMAGE`
using `Path(__file__).resolve().parent` (pointing at `tapes/`) instead of
`.parent.parent` (the repo root) -- every sibling `test_*_e2e.py` script
uses `.parent.parent` for this exact reason, since the script itself
lives one level below the repo root. This immediately raised
`ModuleNotFoundError: No module named 'send_tape'` before the test could
even start a guest. Fixed by correcting the tools-path insert and
`ROOT`/`IMAGE` construction to use `.parent.parent`, keeping a bare
`.parent` only for importing the sibling `test_primes_e2e` module (which
does live alongside this script in `tapes/`).

**With that fixed, the test ran but reported the mic guest sending 0
captured bytes -- even though the microphone itself was confirmed
genuinely working** (`arecord` captured real, non-silent audio directly
on the host; a standalone diagnostic running the plain `audio` shell
command showed the ES1370 ADC ring filling and overflowing normally while
the guest sat idle, proving the pipewire-backed device model itself
receives real mic input correctly). The bug only appeared once the
`audio-net` tape's actual `capture` mode ran its polling loop -- a code
path `tapes/test_audio_capture_e2e.py` had never exercised, since that
existing regression test only checks the `audio` command's own counters
and never drains the ring via the tape ABI's `tape_audio_capture`
callback.

Isolating it: a diagnostic running `tape run capture` for a fixed 4
seconds showed only 4 raw PCI IRQ fires / 0 recognized ADC periods for
the entire window, versus roughly 275 raw fires / 169 ADC periods over
the same 4 seconds with no tape running at all (an idle shell loop) --
the correct rate for 1024-frame periods at 44.1 kHz (~23 ms/period, ~43/s).
This was exactly, deterministically reproducible across repeated runs,
which argued against ordinary host scheduling noise. A second diagnostic
running `tapes/bigsieve` instead (a tape with a pure CPU-bound loop and
*no* I/O port polling of any kind) reproduced the identical starvation
pattern -- proving this is not specific to `audio-net`'s own
`should_abort`/`nic_send` polling, but a general property of *any* tight,
uninterrupted guest busy loop under QEMU's software (TCG) execution
mode: the continuously-running vCPU thread holds the Big QEMU Lock
almost the entire time, starving the separate host-thread-driven audio
timer callback that the `pipewire` audiodev backend needs serviced on
real wall-clock schedule to pull genuine samples through. **This is a
QEMU/TCG emulation-fidelity limitation of this specific test
methodology, not a fos kernel or tape bug** -- real hardware's audio
interrupt is asserted by silicon regardless of what the CPU happens to
be executing at that instant, so no equivalent starvation is possible on
a real machine.

**Fix confirmed**: adding `-accel kvm -cpu host` (hardware-virtualized
execution instead of interpreted) to the exact same reproduction fully
resolved it with no other change -- 24 raw fires / 15 DAC1 periods / 13
ADC periods captured / 9 overruns over the same 4-second window, and the
tape correctly forwarded all bytes it captured. `tapes/
test_audio_net_real_mic_e2e.py` now auto-detects `/dev/kvm` accessibility
(`kvm_available()`, a plain `os.open`/`os.close` probe) and adds
`-accel kvm -cpu host` to both guests' QEMU invocations when available,
printing a clear warning and falling back to plain TCG (with capture
byte counts expected to read 0, for the reason above, rather than a real
failure) when it is not -- this is the first script in this repository to
request KVM acceleration at all; every other `test_*_e2e.py`/
`qemu_smoke.py` script uses default TCG, since exact real-time device
timing has never mattered for any of those tests before ES1370.

**Final result, `audio-real-mic-verify` now genuinely closed**: with both
fixes applied, the full test passes end to end -- mic -> ES1370 ADC ->
network -> `audio-net` capture/listen tapes -> ES1370 DAC1 -> a captured
WAV file containing real audio with peak amplitude 8000 (of 32767 max)
and RMS 5449.1, clearly a real signal well above a silence noise floor,
not the false-positive "PASS" the earlier section's `bytes_sent == 0`
guard was added specifically to prevent.

### Multi-instance ES1370: more than 2 channels via several physical cards

Raised directly by the user's stated goal: could fos support more than
ES1370's hard 2-channel silicon ceiling (see "Real-time audio streaming:
the ES1370 driver" above) -- 4 or 8 channels, say -- if bandwidth allows?
ES1370's `SERIAL_CONTROL` format field only ever encodes mono/stereo
8-/16-bit PCM (re-confirmed against QEMU's own `hw/audio/es1370.c`), so
more channels from *one* card is not on the table; two options were
weighed instead: combine several independent physical ES1370-family
cards (the user's own box of assorted Sound Blaster PCI cards, each an
independent stereo pair), or build a driver for a genuinely
multi-channel codec family (Intel HDA). The user chose to build the
multi-card path first, deferring HDA to a later session.

**The driver/facade conversion mirrors `nic.rs`/`rtl8139.rs`'s existing
multi-instance pattern exactly, not a new design.** `kernel/src/
es1370.rs`'s singleton `static mut CARD: Option<Es1370>` became
`CARDS: [Option<Es1370>; MAX_INSTANCES]` (`MAX_INSTANCES = 4`, chosen the
same way `rtl8139.rs`'s/`nic.rs`'s own small fixed caps are -- comfortably
above what any one real machine is likely to carry, without an unbounded
or dynamically-sized array this kernel's no-heap design can't support
anyway) plus a `CARD_COUNT`; `initialize()` became `initialize_all() ->
Result<usize, Status>`, walking every PCI match `pci::find_devices`
returns (up to `MAX_INSTANCES`) and calling a new `init_card(address)`
helper (the same per-card bring-up `initialize()` used to do once,
unchanged in substance) for each; `get()` became `get(index: usize) ->
Option<&'static Es1370>`; and `handle_pci_irq` now iterates every card
instead of checking one, exactly mirroring `nic.rs`'s own IRQ dispatch --
safe for the identical reason the ES1370+RTL8139 shared-IRQ-line fix
already established (every backend's own `handle_interrupt()` checks its
own status register first and no-ops harmlessly if it isn't the actual
source, so calling every card's handler on a shared line costs nothing
beyond that one check). Each card's own DAC1/ADC ring buffers
(`RINGS`/`CAPTURE_RINGS`) became `[audio::PcmRing; MAX_INSTANCES]`
arrays too, indexed by a new `instance: usize` field stored on each
`Es1370` -- so every ring-touching method (`submit`, `capture`,
`ring_backlog`, `capture_backlog`, `fill_period`,
`drain_capture_period`) now reaches its own card's own ring, never
another card's.

`kernel/src/audio.rs`'s facade mirrors this identically:
`DEVICES: [Option<Audio>; MAX_DEVICES]`/`DEVICE_COUNT` replace the former
`ACTIVE`, `initialize()` now calls `es1370::initialize_all()` and
populates every discovered slot, and new `get_at(index)`/`count()`
functions join the existing `get()` (kept meaning "device 0", purely for
backward compatibility with the non-indexed tape ABI callbacks that
predate this feature). **Deliberately, and unlike `nic.rs`, there is no
"active device" selection concept at all** (no `set_active`/
`ACTIVE_INDEX`) -- the entire point of driving several cards is to use
all of them *simultaneously* as independent stereo pairs for one
combined multichannel stream, not to pick one card at a time the way
`nic use N` picks one NIC. A tape wanting genuine multichannel output is
expected to address every device by index, not switch between them.

**The tape ABI grew five more trailing callbacks**, following the exact
same append-only-cdecl convention as every prior ABI growth in this
document: `tape_audio_count() -> u32` (how many devices were found) and
four index-based counterparts to the existing non-indexed audio
callbacks -- `tape_audio_sample_rate_at`,
`tape_audio_channels_and_bits_at`, `tape_audio_submit_at`, and
`tape_audio_capture_at` (all taking a 0-based device index as their first
argument, matching `nic_mac_at`'s own indexing convention). Every
existing tape ignores all nine audio callbacks (five new, four
pre-existing) exactly like any other trailing cdecl argument; the
existing `tape_audio_sample_rate`/`tape_audio_channels_and_bits`/
`tape_audio_submit`/`tape_audio_capture` callbacks keep meaning "device
0" unchanged.

**`audio` (the shell command) was rewritten to report every discovered
device, not the single one it used to assume.** It now prints an overall
`N audio device(s) found:` line, then one indexed block per device
(`[0] ES1370 at I/O base 0x...`, followed by that device's own
IRQ/DAC1/ADC/backlog/underrun/overrun diagnostics, each indented two
spaces) -- deliberately allowed to scroll past one screen once several
real cards are attached, the same `pci` precedent already established
for an inherently unbounded hardware listing. This changed the exact
text `audio` prints (the former single-device `"ES1370 found: I/O base
0x...` line is gone), which broke two pre-existing tests' own marker
strings (`test_audio_capture_e2e.py`'s `b"ES1370 found"` check and
`test_audio_net_e2e.py`'s identical one) purely because they searched for
that literal, now-removed substring -- both were updated to look for
`b"[0] ES1370"` instead, confirmed passing again afterward. No other
test needed a change; `test_audio_net_real_mic_e2e.py`'s own marker
(`b"Raw STATUS register"`, chosen specifically to be a true line-tail
rather than a prefix -- see that section's own note on this exact class
of bug) still appears verbatim, just now indented two spaces under its
device's own block.

**Reserved low-memory region grew again, from `0x60000` to `0x80000`.**
Growing `MAX_INSTANCES`-many copies of every per-card static (DMA
buffers, ring arrays, `CARDS`) pushed `__kernel_end` well past the
previous ceiling. Measuring the real overflow needed the same technique
this document's own audio-ring-sizing investigation above used:
temporarily raise the linker `ASSERT` to something absurd (`0x200000`),
build, then read `.bss`'s real start/size back out of `readelf -S`
directly, since the `ASSERT` failure itself reports no numeric detail --
this measured `.bss` starting at `0x37000`, sized `0x41fb4`, giving
`__kernel_end = 0x78fb4`. `0x80000` was chosen as the new ceiling
(roughly 28.75 KB of headroom above that measurement) and, as this
project's "Low-memory map" section requires, all four constants that
must always move together were updated in the same change:
`kernel/linker.ld`'s `ASSERT`, `kernel/src/pmem.rs`'s `RESERVED_BELOW`,
`tools/write_layout.py`'s `KERNEL_RESERVED_END`, and `tools/
write_netrom_layout.py`'s `MAX_KERNEL_BYTES` -- plus a sixth,
undocumented-until-now spot this same grep sweep turned up:
`boot/stage2.S`'s soft-reboot page-touch/resync loop had its own
hardcoded `0x70000` bound that also needed updating to `0x80000`, since
it is not derived from any of the four "official" constants at all.

**A fifth dependency this project's own soft-reboot documentation
already warned about, checked and confirmed necessary again**:
`kernel/src/debug.rs`'s `SLOT1_LBA`/`SLOT_SECTORS` constants (used by the
A/B kernel-slot machinery) are not automatically derived from
`KERNEL_RESERVED_END` and had to be manually recomputed (`python3
tools/write_layout.py --kernel build/kernel.bin --output /dev/stdout`)
and copied in by hand -- `SLOT1_LBA` moved `2849 -> 2977`, `SLOT_SECTORS`
moved `768 -> 896`. `debug.rs`'s own doc comment (which already
documents an earlier real crash caused by exactly this drift) was
extended with this session's growth as a second confirming data point.
Re-running `tools/write_layout.py` after every later shell-only ABI
change (which does not change kernel size at the sector-rounding
granularity) confirmed these two values did not need to move again for
those later, smaller edits -- only the ring-array-driven growth above
actually crossed a sector boundary.

**Verified with `tapes/test_audio_multi_e2e.py`** (not part of `make
test`; run via the new `make test-audio-multi` target): two independent
QEMU-emulated ES1370 devices, each with its own `none` audiodev backend
(chosen for the same environment-portability reason
`test_audio_capture_e2e.py` already gives), are confirmed discovered at
two distinct I/O bases (not the same device reported twice), both
report the expected 44100 Hz/2ch/16-bit format independently, and both
devices' DAC1 period counts keep advancing independently and
simultaneously over a 1.5-second window -- the concrete proof that the
array-of-instances refactor genuinely drives two separate cards rather
than aliasing one card's state onto the other. This test deliberately
scopes itself to the driver/facade/shell-command plumbing only, not the
new tape-ABI callbacks themselves (`tape_audio_count`/`..._at`); those
remain to be exercised by a future update to `tapes/audio-net` (the
actual multichannel streaming payoff), not yet built as of this writing.
`make test` (the full existing regression suite) was re-run after this
whole refactor and passed unmodified, confirming no regression to any
existing NIC/audio/disk/boot behavior from either the driver conversion
or the reserved-region growth.

`tapes/audio-net`'s own move to genuine multichannel streaming --
designing a wire format that declares a device/channel count and
interleaves PCM across several independent stereo devices, and extending
`AUDIO_TAG_START`/`AUDIO_TAG_DATA` to carry it -- is the next planned
increment, not yet started as of this writing.

### `tapes/audio-net`'s wire protocol grows a per-device tag, instead of interleaving

The multichannel-streaming increment flagged above as "not yet started"
raised a real design fork: should one combined multi-card stream be a
single frame format interleaving every device's samples together (e.g.
`[dev0-L, dev0-R, dev1-L, dev1-R, ...]` per frame), or should each
device keep its own independently tagged sub-stream sharing one session?

**Tagged per-device sub-streams were chosen, not interleaving**, for the
same reason this project's other wire protocols already tolerate
occasional loss as "a glitch, not an abort" (see `AUDIO_TAG_DATA`'s own
original doc comment, and `TERM_TAG_DATA`'s identical reasoning above):
an interleaved frame's channels are only meaningful relative to each
other, so one dropped frame -- routine on this project's best-effort,
no-retransmission protocol family -- would desynchronize *every*
channel's byte alignment permanently, with no way to recover it later.
A dropped frame in a tagged sub-stream, by contrast, only glitches that
one device; every other device's own sub-stream is completely
unaffected. Tagging is also the more natural fit for genuinely
independent multi-card hardware (as opposed to one hypothetical
multichannel codec): each physical Sound Blaster card's sub-stream can
be sent, dropped, or resent independently, arrive over a different NIC,
or have its sender join/leave the session at a different time --
`listen_mode`'s "first `AUDIO_TAG_START` from any device index locks the
peer" design (below) depends directly on this independence.

**Wire format, extended, not replaced:**

| Tag | Payload (was) | Payload (now) |
| --- | --- | --- |
| `AUDIO_TAG_START` | `sample_rate:u32 LE, channels:u8, bits:u8` (6 bytes) | adds a trailing `device_index:u8` (7 bytes) |
| `AUDIO_TAG_DATA` | `length:u16 LE` + PCM | adds a leading `device_index:u8` before the existing `length:u16 LE` + PCM (`DATA_HEADER_LEN` grew 2 -> 3 bytes) |
| `AUDIO_TAG_STOP` | no payload | unchanged -- still ends the *entire* session, not one device's sub-stream |

This is a breaking wire-protocol change, not an additive one -- accepted
deliberately, since (unlike, say, NETFS/KUP) nothing outside this
project's own paired sender/receiver ever speaks this exact protocol, so
there is no interoperability cost to changing the payload layout
outright rather than growing it compatibly. `MAX_DEVICES = 4` is a new
tape-side constant deliberately mirroring `kernel/src/es1370.rs`'s
`MAX_INSTANCES`, duplicated rather than shared (no header mechanism
crosses the kernel/tape boundary, the same situation every other
tape-side mirrored constant in this project is already in).

**`listen_mode` was rewritten for per-device demultiplexing.** It now
queries `audio_count()` (clamped to `MAX_DEVICES`) and prints every
locally attached device's own format up front, then tracks
`local_formats`/`device_active` arrays across the session. The *first*
`AUDIO_TAG_START` received from any peer, for any device index, locks
that peer's MAC as the session's `source` (mirroring `tape_load_net`'s
existing "first sender wins" convention) and resets every device's
active/byte-counter state for the new session; any *later* `AUDIO_TAG_
START` from that same locked peer, naming a different device index, is
then accepted as an additional sub-stream within the same session
(printed as `"device N sub-stream active"`) rather than being rejected
or re-locking the source. Each sub-stream's declared format is validated
against that specific local device's own real format independently --
one device's stream can be rejected as a mismatch while another
device's stream is accepted in the same session, since resampling is
still out of scope exactly as it always was. `AUDIO_TAG_DATA` frames are
demultiplexed by their leading `device_index` byte to the matching
local device's `audio_submit_at(index, ...)`; an out-of-range or not-yet-
activated device index is silently dropped, mirroring the existing
"drop what can't be understood" tolerance this protocol family already
has elsewhere. Byte counters (`bytes_received`/`bytes_played`/`bytes_
dropped`) remain one running session-wide total across every device's
sub-stream combined, not broken out per device -- this matches the
existing single-device summary's own granularity and was not considered
important enough to change.

**`capture_mode` was rewritten to round-robin across local devices.**
It queries `audio_count()`, prints every device's format, sends one
tagged `AUDIO_TAG_START` per local device up front (each carrying that
device's own real format and index), and its main loop now polls
`audio_capture_at(index, ...)` for every attached device once per pass,
sending a tagged `AUDIO_TAG_DATA` frame for whichever device(s)
produced a non-empty result that pass -- so one slow or silent device's
capture never blocks another device's sub-stream from being forwarded
promptly. `send_mode` (the pre-recorded-file replay stand-in for live
capture) is unaffected in spirit -- it still replays exactly one file at
the fixed 44100 Hz/2ch/16-bit format -- but now tags every frame it
sends with a new `SEND_DEVICE_INDEX = 0` constant, since the wire format
requires a device index on every frame regardless of how many devices
are actually involved.

**Verified with a new, dedicated e2e test, `tapes/test_audio_net_multi_
e2e.py`** (not part of `make test`, mirroring every other tape e2e
harness): two QEMU-emulated ES1370 cards, each with its own `wav`
audiodev backend, are driven through one `audio-net` listen-mode
session carrying two independently tagged sub-streams at once -- device
0 gets a real, non-silent 440 Hz tone, device 1 gets genuine PCM
silence (all-zero samples), sent interleaved frame-by-frame over the
same session. The test deliberately does not just check "some audio
played somewhere": it asserts device 0's captured WAV contains the real
tone (peak amplitude comfortably above a silence floor) *and* device 1's
captured WAV settles to near-zero amplitude once its own sub-stream
starts submitting silence -- if `device_index` demuxing were broken
(e.g. both sub-streams aliased onto the same local device, or one
silently swapped for the other), this is exactly the check that would
catch it. One real measurement subtlety surfaced while writing this
test and is worth remembering for any future audio-capture-verifying
test: a device's own captured WAV file spans its *entire* guest
lifetime, including the boot-time narration before any tape ever ran --
during which every device still plays its default ~441 Hz idle test
tone (see `network_mode`'s own doc comment in `kernel/src/es1370.rs`:
"there is no way back to the test tone without a reboot" once a
device's first `audio_submit`/`audio_submit_at` call flips it into
network-fed mode) -- so a naive whole-file peak check on the
"expected silent" device would spuriously fail on that earlier idle
tone. The fix was to check only the *tail* of that device's capture
(after a deliberate `time.sleep(1.0)` once the streaming session's byte
counts are confirmed, giving the device time to settle into pure
post-submission silence) rather than the whole file -- the device-with-
a-real-tone's check does not need this same care, since its own
sub-stream's tone dominates the peak regardless of when in the capture
window it is measured.

The pre-existing single-device test, `tapes/test_audio_net_e2e.py`, was
updated (not replaced) to speak the new 7-byte `AUDIO_TAG_START`/tagged
`AUDIO_TAG_DATA` payloads with a fixed `DEVICE_INDEX = 0`, and its
`"Local audio device: ..."` marker was updated to match `listen_mode`'s
new per-device-list printed format (`"Local audio device(s): N.\n  [0]
..."`). `tapes/test_audio_net_real_mic_e2e.py` needed no changes at all,
since it drives both peers through the real compiled tape end to end
(never constructing raw wire frames itself) and its own markers
(`"Streaming live ADC input"`, `"capture stopped locally"`) are
unaffected text that only moved position within `capture_mode`'s
rewritten body. `make test` (the full existing regression suite),
`tapes/test_audio_capture_e2e.py`, and `tapes/test_audio_multi_e2e.py`
were all re-run after this change and passed unmodified, confirming no
regression to the underlying driver/facade work from the prior session.

### Intel HD Audio ("HDA"): a discover-only first increment

Genuine Intel HDA support -- the second hardware family the user's own
box of sound cards includes, considered as a follow-up once ES1370
multi-device streaming was working -- now has a first, deliberately
narrow increment: `kernel/src/hda.rs` and the new `hda` shell command
reset an Intel HDA controller and enumerate whatever codec(s) respond,
reading back each one's root-node Vendor/Device ID. **There is no
playback or capture yet** -- this step proves controller discovery and
codec enumeration work, nothing more, the same incremental approach
`i350.rs`'s empty `CANDIDATE_DEVICE_IDS` and the very first ES1370
increment (test-tone-only DAC1, before ADC/network-fed streaming
existed) both already established for this project.

**HDA's controller/codec split is architecturally different from every
prior audio/NIC backend here.** An "HDA controller" (the PCI device this
driver discovers) is a generic DMA/verb-transport engine, not itself an
audio device -- the actual codec(s) (DAC/ADC/mixer widgets) sit behind it
on a private, controller-owned "CODEC" bus addressed by a small integer
("codec address", `cad`, 0-14) and are discovered, not assumed, by
reading a bitmask register after reset. This is a real architectural
split ES1370 (one PCI function *is* the whole audio device) has no
equivalent of.

**The Immediate Command interface was used instead of the full CORB/RIRB
DMA ring buffers**, specifically because it needs zero guest-memory setup
at all -- the spec's mandatory command/response path otherwise requires
allocating and programming two ring buffers (Command Output Ring Buffer,
Response Input Ring Buffer) before a single verb can be sent. Confirmed
this synchronous path is real, not a QEMU stub, by reading QEMU's own
`intel_hda_set_ics`/`intel_hda_corb_run`/`intel_hda_response`
(`hw/audio/intel-hda.c`, tag v10.2.1, this project's installed QEMU
version): `IC` (offset `0x60`) holds the 32-bit verb to send; writing
`IRS_BUSY` (bit 0 of `IRS`, offset `0x68`) submits it; polling `IRS_BUSY`
back to clear and `IRS_VALID` (bit 1) set indicates a response is ready
in `IR` (offset `0x64`); writing 1 to `IRS_VALID` clears it for the next
command, per spec. Every register offset/bit here was cross-checked
directly against QEMU's own `intel-hda-defs.h`, the same rigor this
project applies to every other register-level driver after the RTL8139
lessons (see "Real hardware notes" above) -- **all of it remains
unverified against real hardware**, exactly like every other freshly
scaffolded driver here before its first physical test.

**The 32-bit verb encoding** (validated against QEMU's own
`hda_audio_command` decode logic in `hw/audio/hda-codec.c`): a command
word is `(cad << 28) | (nid << 20) | (verb << 8) | payload`, where `nid`
is the target widget/node (`AC_NODE_ROOT = 0x00` for this step, since it
only reads the root node's own Vendor/Device ID parameter) and `verb`
here is `AC_VERB_PARAMETERS = 0xf00` (get-parameter) with payload
`AC_PAR_VENDOR_ID = 0x00`. This single formula covers both the spec's
12-bit "short" verbs and 4-bit "long" verbs uniformly, since their bit
ranges never overlap -- no special-casing needed, confirmed against
several different verb constants in QEMU's decode path.

**`GCTL_CRST`'s reset polarity is the opposite of an intuitive "write 1
to reset" convention** -- bit 0 of `GCTL` (offset `0x08`) reads 0 while
the controller is held in hardware reset and reads/must be written 1 for
normal operation; it is not a self-clearing pulse. `reset_controller`
clears it, polls until it reads back 0, sets it again, and polls until
it reads back 1 -- both transitions bounded (`IMMEDIATE_COMMAND_POLL_
ATTEMPTS`-style spin ceilings, this project's usual "poll then give up
cleanly" convention, not a real timeout value from the spec) -- then
waits a short, fixed number of PIT ticks (`CODEC_ENUMERATION_WAIT_
TICKS`) before reading `STATESTS`, since the spec allows codecs a brief
settle period after reset before they assert their own presence bit.

**Only QEMU-confirmed PCI IDs are registered for now** -- Intel
`0x8086:0x2668` (`intel-hda`, ich6) and `0x8086:0x293e`
(`ich9-intel-hda`) -- following the same "don't guess real-hardware IDs,
confirm via `pci` first" convention `i350.rs`'s empty candidate list
already established; the user's actual Sound Blaster PCI cards will need
their real vendor/device IDs read via the existing `pci` command before
being added here. **Single-instance for this step** (only the first
matching controller found is driven), a deliberate scope-narrowing
documented in `hda.rs`'s own module doc comment, not a hard ceiling --
mirroring how `es1370.rs` itself started single-instance before growing
`MAX_INSTANCES` once multi-card streaming became a real, motivated need.

**QEMU's synthetic codec identity, confirmed empirically, not just from
reading source**: `hda-output`/`hda-duplex`/`hda-micro` all report
Vendor ID `0x1af4` (QEMU's own convention for synthetic devices, shared
with virtio) -- a real `-device intel-hda -device hda-duplex,audiodev=...`
QEMU boot reported Device ID `0x0022` for `hda-duplex` specifically,
confirming the `0x1af4:0x00xx` family predicted from reading
`hw/audio/hda-codec-common.h` alone.

Verified end to end with `tapes/test_hda_e2e.py` (not part of `make
test`, mirroring every other tape/driver e2e harness's `term_net`-driven
structure): boots a guest with `-device intel-hda -device
hda-duplex,audiodev=none`, drives the `hda` shell command over a live
`term net` session, and asserts the reported vendor ID matches `0x1af4`
and that a second, immediately following `hda` call reports the exact
same codec without re-probing (the command is idempotent once
`initialize()` has already succeeded, matching every other lazy-init
backend's `Option<Backend>`-cached convention here).

CORB/RIRB DMA-ring support, real node-graph widget enumeration (walking
each codec's Audio Function Group and its DAC/ADC/pin-complex widgets),
stream-descriptor/BDL-based playback and capture, and format negotiation
are all deliberately deferred to a later phase -- this increment proves
only that the controller and codec(s) can be found and talked to at all.

### Intel HDA, step two: CORB/RIRB command transport and real node-graph enumeration

The natural next increment after discover-only: `kernel/src/hda.rs`
switched entirely from the optional Immediate Command interface to
CORB/RIRB (Command Output Ring Buffer / Response Input Ring Buffer), the
HDA spec's mandatory command transport, and added a real node-graph
walk -- root node -> Audio Function Group -> that AFG's own child
widgets (DAC/ADC/pin complexes) -- replacing the prior step's "only ever
read the root node's Vendor/Device ID" scope. Still no playback or
capture; this is a diagnostic/plumbing increment, mirroring how ES1370
grew in stages (test tone, then ADC, then network-fed streaming) rather
than attempting stream DMA in one step.

**Why CORB/RIRB despite Immediate Command already working.** Immediate
Command is genuinely optional per spec -- real hardware is not required
to implement it -- while CORB/RIRB is the transport every real
controller must support, and it is also the same DMA-ring shape (a
host-memory ring, a hardware-maintained write pointer, a
software-maintained read pointer) actual stream playback's Buffer
Descriptor Lists will need later, so building it now doubles as
practice for that shape rather than being throwaway plumbing.

**Every offset, bit, and DMA read/write-pointer semantic here was
cross-checked directly against QEMU's own `hw/audio/intel-hda.c` (tag
v10.2.1) source, not just header constants** -- specifically
`intel_hda_corb_run`/`intel_hda_response`/`intel_hda_set_corb_wp`/
`intel_hda_set_rirb_wp`, and the `regtab[]` register-definition table's
`reset`/`wmask` fields for `CORBSIZE`/`RIRBSIZE`/`CORBRP`/`RIRBWP`. One
practical lesson from this cross-referencing worth recording for future
large-source-file investigations: `web_fetch` silently truncated at
least one of these files with no truncation notice, giving an
incomplete (and briefly misleading) picture; downloading the full file
with `curl` and then `grep`/`sed`-ing it directly is the reliable way to
get complete, verifiable reference source.

**The CORB/RIRB read/write-pointer convention is a real gotcha, not
obvious from the register names alone.** Both `CORBWP` and `RIRBWP` hold
the index of the *last* written entry, not "the next free slot" --
confirmed directly from `intel_hda_corb_run`'s `d->corb_rp = (d->corb_rp
+ 1) & 0xff` (reading the verb at that *pre-incremented* index) and
`intel_hda_response`'s identical pre-increment before writing a
response. So posting a new verb means: compute `(current_CORBWP + 1) &
0xff`, write the verb into *that* ring slot, then update `CORBWP` to
match -- writing the verb at the *current* `CORBWP` index (the
intuitive-but-wrong reading) would silently clobber whatever slot the
hardware last consumed, or race a still-in-flight command. `RIRBWP`
mirrors this exactly on the read side, with one further wrinkle:
**software has no RIRB read-pointer register at all** -- there is no
`RIRBRP`, only `RIRBWP` (hardware's own last-written index) -- so this
driver tracks its own "last consumed" index in a `Cell<u16>` on `Hda`
(interior mutability, since every `send_corb_command` call takes only
`&Hda`) and polls `RIRBWP` for it to advance to `(tracked + 1) & 0xff`
before reading that slot.

**`CORBSIZE`/`RIRBSIZE` are read-only in practice, not negotiated.**
QEMU's own `regtab[]` gives both registers no `wmask` at all, and both
reset to `0x42` (bit 6 set, "256-entry size supported"; bits[1:0] = `10`,
"256 already selected"). The HDA spec does allow smaller ring sizes (2
or 16 entries) on some real controllers, selected by writing bits[1:0],
but since 256 is both the maximum and QEMU's only supported size,
`setup_corb_rirb` simply reads each register back after programming the
base-address registers and treats anything other than the 256-entry
selection as unsupported, rather than attempting to write a particular
size -- the safer assumption for real hardware that might likewise
silently ignore an unsupported write.

**`CORBRP`'s reset is a two-phase handshake; `RIRBWP`'s is a
self-clearing pulse -- these are genuinely different, not two
descriptions of the same thing.** `CORBRP_RST` (bit 15) must be set,
polled until the readback confirms it, cleared, and polled again until
the readback confirms *that* -- mirroring Linux's own `azx_reset`.
`RIRBWP_RST`, by contrast, self-clears instantly per
`intel_hda_set_rirb_wp`'s handler (`if (d->rirb_wp & ICH6_RIRBWP_RST)
d->rirb_wp = 0;`), needing no poll at all. Getting these confused (e.g.
polling `RIRBWP` for a bit that already cleared itself) would either
hang or silently skip the intended reset.

**`RINTCNT` must be set to its maximum (`0xff`) at setup, not left at
its reset default**, because `intel_hda_corb_run` stalls verb processing
once `rirb_count == RINTCNT` until software clears `RIRBSTS` -- a
signal this driver, which only ever polls `RIRBWP` and never touches
`RIRBSTS` at all, would never satisfy. Missing this would manifest as
"the first handful of verbs work, then the controller silently stops
answering" -- a real, easy-to-hit trap for a polling-only driver like
this one.

**Node-graph walk.** `find_audio_function_group` reads the root node's
own `NODE_COUNT` parameter (start nid + count, packed as bits[23:16]/
bits[7:0]) and checks each subordinate node's `FUNCTION_TYPE` for
`AC_GRP_AUDIO_FUNCTION`, rather than assuming the first subordinate node
is always the AFG -- a real codec could expose more than one function
group (e.g. audio plus modem). `enumerate_afg_widgets` then reads the
AFG's own `NODE_COUNT` for its child-widget range and, per widget, reads
`AUDIO_WIDGET_CAP` (widget type via bits[23:20], stereo via bit 0) plus,
conditionally, `PIN_CAP` (for a `Pin` widget, giving output/input
capability) and `CONNLIST_LEN`/`GET_CONNECT_LIST` (for any widget
reporting `AC_WCAP_CONN_LIST`, giving its first upstream connection).
Only the short-form (one byte per entry, up to four entries) connection
encoding is read -- sufficient for QEMU's synthetic codecs, which only
ever expose one connection per widget in short form; a real codec's
long-form (16-bit) entries or more than four connections would need a
fuller parser, out of scope for this step.

**Confirmed against QEMU's actual `hda-duplex` layout, not just
predicted from source**: root (nid 0) -> AFG (nid 1) -> DAC (nid 2,
`audio output`, stereo) -> line-out pin (nid 3, `pin complex`,
output-capable, connected to nid 2) -> ADC (nid 4, `audio input`,
stereo, connected to nid 5) -> line-in pin (nid 5, `pin complex`,
input-capable). Note the ADC's connection points *at* its pin (nid 4 ->
nid 5), the reverse direction of the DAC/output-pin pair (nid 3 -> nid
2) -- an input path's converter reads *from* its pin, while an output
path's pin reads *from* its converter, exactly the direction each
signal actually flows; `tapes/test_hda_e2e.py`'s own regex assertions
initially got this backwards and had to be corrected once the real
output made the actual topology obvious.

Verified end to end with the extended `tapes/test_hda_e2e.py`: beyond
the prior step's vendor/device-ID and idempotency checks, it now also
asserts the AFG nid, the DAC/line-out-pin pair (nid 2 -> nid 3
connection), and -- when the guest is configured with `hda-duplex`
(as this test's fixture is) -- the ADC/line-in-pin pair (nid 4 -> nid 5
connection) all appear correctly in the `hda` command's output. `make
test` (unrelated to HDA) was re-run unmodified and continues to pass,
confirming no regression from this rewrite.

Stream-descriptor/BDL-based playback and capture (`SD_FORMAT`, a Buffer
Descriptor List, `SET_STREAM_FORMAT`/`SET_CHANNEL_STREAMID` verbs,
`AC_PINCTL_OUT_EN` to actually enable a pin's output, and starting DMA
via `SD_CTL`) remain the next, larger phase -- not yet scoped in detail.

### Intel HDA, step three: more than one controller

`kernel/src/hda.rs`'s own doc comment on the (then-)single-instance
`MAX_CODECS` constant had already flagged this exact limitation as
deliberate-but-revisitable, explicitly citing `es1370.rs`'s own
instance-count growth (see "Multi-instance ES1370 audio driver" below)
as the precedent to follow "once a real need for it appears" -- it
appeared directly, from the user owning a box of several different
physical sound cards and asking whether more than one HDA controller
could be handled at once (e.g. one onboard controller plus a discrete
add-in card).

The conversion mirrors `es1370.rs`'s established shape exactly:
`initialize()` became `initialize_all() -> Result<usize, Status>`
(idempotent, returns the discovered count on success/repeat calls,
`Status::NotPresent`/`InitFailed` unchanged otherwise), the single
`static mut CONTROLLER: Option<Hda>` became `static mut CONTROLLERS:
[Option<Hda>; MAX_INSTANCES]` plus a `CONTROLLER_COUNT` counter, and the
former index-less `get()` became `get(index: usize)` (0-based, matching
`es1370::get`'s own convention, not the shell's 1-based `nic use N`). A
new `count()` accessor lets a caller loop `0..hda::count()` without
needing to know `MAX_INSTANCES` itself. The per-address bring-up logic
(PCI enable, MMIO BAR map, controller reset, CORB/RIRB ring setup, codec
discovery) was extracted into a new `init_controller(address, instance)`
helper -- mirroring `es1370::init_card`'s own split -- so one
controller's bring-up failure does not prevent an already-discovered
earlier one from being usable; `initialize_all` simply skips a failed
address and continues. `Hda` gained one new field, `instance: usize`
(mirroring `Es1370`'s own field of the same name and purpose), purely so
`hda_status`'s per-controller output can print "Controller N: ..." from
the struct itself rather than needing to be threaded through separately.

**No structural change was needed to `Hda` itself, `CodecInfo`,
`WidgetInfo`, `discover_codecs`, `find_audio_function_group`,
`enumerate_afg_widgets`, `reset_controller`, or any register/verb
constant** -- every one of those already operated on one controller's
own `mmio_base`/`corb_base`/`rirb_base`, so they needed nothing beyond
being called once per discovered address instead of once total. This is
exactly why the conversion was safe and small: `paging::map_mmio` is
already keyed off each controller's own distinct PCI BAR, so two
controllers' MMIO windows, CORB/RIRB rings, and register state can never
collide with one another simply by construction, with no extra
bookkeeping required to keep them apart.

**`MAX_INSTANCES` was set to `2`, not `es1370.rs`'s `4`.** The two
drivers' growth motivations are genuinely different: ES1370's `4` was
chosen to reach a target *channel count* (each ES1370 card is a fixed
one-DAC-per-card stereo pair, so more instances is the only way to add
more channels once playback exists). HDA has no playback implemented
yet, and when it does, a single HDA codec can already expose several
independent DAC/pin pairs on its own node graph -- so this ceiling only
needs to cover a plausible number of physically installed
*controllers*, not multiply out to a channel-count target. `2` also
already matched this driver's pre-existing `find_devices` match-array
size (sized for "at most a couple of these on one machine" even before
this change), so no companion constant needed to grow alongside it.

`kernel/src/shell.rs`'s `hda_status()` was split into an outer loop
(calling `initialize_all()`, printing a `"N Intel HD Audio
controller(s) discovered:"` summary line, then iterating
`0..hda::count()`) and a new `hda_status_one(&Hda)` printing one
controller's own MMIO base/IRQ/stream-count/codec-node-graph block,
prefixed `"Controller N: ..."` -- reusing exactly the same per-codec/
per-widget printing logic the prior single-controller version already
had, unchanged. This follows this driver's existing "no arguments
needed, just show me everything" diagnostic style (matching `pci`'s own
shape) rather than adding `nic use N`-style index-based selection
syntax, since HDA has no persistent "active controller" concept to
select yet (no playback/capture exists to direct at one card over
another).

Verified end to end with a new, dedicated `tapes/test_hda_multi_e2e.py`
(not part of `make test`, mirroring `tapes/test_hda_e2e.py`'s own
structure): a guest configured with *two* independent `intel-hda`
controllers (`hda0` carrying an output-only `hda-output` codec, `hda1`
carrying a full-duplex `hda-duplex` codec, each on its own `-audiodev
none` backend) confirms both are discovered (`count == 2`), reset
independently with genuinely distinct MMIO bases, and each one's own
codec is enumerated correctly and independently -- controller 0's block
has no ADC/line-in widgets at all (proving `hda-output`'s narrower
topology wasn't accidentally inherited from the other controller),
while controller 1's block has both a DAC/line-out and an ADC/line-in
pair, exactly like the existing single-controller test's `hda-duplex`
fixture -- and that a second `hda` call remains idempotent, still
reporting both controllers with no re-probe. `tapes/test_hda_e2e.py`
(the original single-controller test) was re-run unmodified and
continues to pass unchanged, confirming this conversion is backward-
compatible for the single-controller case. `make test` (unrelated to
HDA) was also re-run and continues to pass.

### Intel HDA, step four: real stream-DMA playback

The prior two increments got as far as codec/node-graph *enumeration*;
this one makes an Intel HDA controller actually produce sound, joining
`audio.rs`'s facade (and, through it, `audio-net`) alongside ES1370 with
no tape-side changes at all -- `tapes/audio-net/src/main.rs` already
iterated every device generically via `audio_count()`/`audio_sample_rate
_at()`/`audio_submit_at()`, so an `Audio::Hda` variant slots in exactly
the same way a fifth NIC backend joins `nic::initialize`'s own loop.

**Register/verb semantics were re-derived from QEMU's own emulation
source** (`hw/audio/intel-hda.c`/`intel-hda-defs.h`/`hda-codec.c` at the
pinned `v10.2.1` tag, fetched via `raw.githubusercontent.com` since this
sandbox has no local QEMU source tree and `apt-get source` has no
`deb-src` configured) -- the same rigor and reasoning this project
already applied to ES1370's own register work. Two points were load-
bearing:
- **The verb-encoding formula has two shapes**, matched by QEMU's own
  `hda_audio_command`: a 12-bit-verb/8-bit-payload form when
  `(data & 0x70000) == 0x70000` (used by the pre-existing discovery
  code's `verb()` helper), and a 4-bit-verb/16-bit-payload form
  otherwise (a new `verb16()` helper), needed for
  `SET_STREAM_FORMAT`/`SET_AMP_GAIN_MUTE`, whose payloads don't fit in
  8 bits.
- **`SD_CTL` (per-stream control) and `SD_STS` (per-stream status)
  alias the same 32-bit register**: the low 24 bits are real control
  bits, but the top byte *is* `SD_STS` -- so every `SD_CTL` write must
  mask `& 0x00ff_ffff` or it silently acknowledges a completion status
  the driver never meant to touch. `write_sd_ctl`/`read_sd_ctl` wrap
  this masking once so every call site can't get it wrong.

**DMA is codec/controller-pulled, not driver-pushed**, the same shape
ES1370's own DAC1 ring already uses: `setup_playback` allocates a
physically contiguous 2-period DMA buffer (`pmem::allocate_contiguous`)
plus a one-page Buffer Descriptor List (`pmem::allocate`), writes two
16-byte BDL entries (`addr:u64`/`len:u32`/`flags:u32`, only the
interrupt-on-completion flag used), performs the documented
stream-reset handshake, programs `SD_CBL`/`SD_LVI`/`SD_FORMAT`/
`SD_BDLPL`/`SD_BDLPU`, sends the codec verbs that route a fixed stream
tag to the discovered DAC/pin pair and unmute it
(`SET_STREAM_FORMAT`/`SET_CHANNEL_STREAMID`/`SET_AMP_GAIN_MUTE`/
`SET_PIN_WIDGET_CONTROL` -- sent defensively, non-fatal if a codec
doesn't answer, matching the existing discovery step's tolerance),
pre-fills both periods with silence, registers a legacy PCI IRQ handler
(`handle_pci_irq`, mirroring ES1370's own shared-line-safe pattern:
check this device's own `SD_STS_BCIS` bit before doing anything, so
sharing an INTx line with an unrelated device costs nothing beyond one
harmless register read), and finally sets `SD_CTL`'s stream tag/IOCE/RUN
bits and unmasks the controller's global/controller/per-stream
interrupt-enable bits in `INTCTL`. `find_playback_target` walks the
already-enumerated node graph for a DAC (`AudioOutput` widget) and an
output-capable `Pin`, preferring a pair the graph already reports as
connected and falling back to the first plausible pair otherwise --
exactly the same "prefer confirmed, fall back to first-plausible"
tolerance this driver's discovery step already uses elsewhere. A
controller whose graph has no usable DAC/pin pair, or no free output
stream, is left with `has_playback() == false` and is not added to
`audio.rs`'s device list at all -- `hda status` remains a separate,
always-available way to inspect such a controller's node graph
regardless, unchanged from the previous two increments.

**Format is a fixed 44100 Hz / 2ch / 16-bit ceiling** (`PLAYBACK_FORMAT
= 0x4011`), the same ceiling QEMU's synthetic codec's converter widgets
appear to support and, not coincidentally, identical to ES1370's own
hardwired ceiling -- true per-codec-negotiated multichannel/higher-bit-
depth support is deferred, undocumented and untested against this
project's installed QEMU version, the same "flag it as speculative
until real hardware or a richer emulation proves otherwise" convention
already applied to I350's empty `CANDIDATE_DEVICE_IDS`.

**`audio.rs`'s facade grew an `Audio::Hda(&'static hda::Hda)` variant**
alongside the existing `Audio::Es1370` one: every method gained a
matching arm, `MAX_DEVICES` became `es1370::MAX_INSTANCES +
hda::MAX_INSTANCES` (both backends share one flat `DEVICES` array,
mirroring `nic.rs`'s own multi-backend facade), and `io_base()`'s return
type widened from `u16` to `u32` since HDA's MMIO physical base doesn't
fit in 16 bits (ES1370's own arm now just casts its I/O port number
`as u32`) -- `shell.rs`'s `audio_status()` print format string was
updated from `0x{:04x}` to `0x{:08x}` to match, and its header line's
wording changed from "I/O base" to the backend-neutral "base" for the
same reason. `initialize()` no longer early-returns the moment ES1370
reports `NotPresent`/`InitFailed`; it now falls through with `found = 0`
so HDA discovery still runs on a machine with no ES1370 at all, then
adds each HDA controller that has `has_playback() == true` after the
ES1370 loop. Capture-side methods (`capture`/`capture_overrun_count`/
`capture_backlog`/`capture_periods`) all return `0`/no-op for
`Audio::Hda`, since this driver's initial scope is playback-only --
mirroring ES1370's own DAC1-before-ADC history, and a candidate for a
future increment the same way ES1370's ADC was.

`kernel/src/shell.rs`'s `hda_status_one` was updated to match: the
stale "node graph, no playback yet" wording is gone, and a controller
with `has_playback() == true` now prints its active format, IRQ/period/
interrupt counts, underrun count, and network-fed ring backlog --
exactly the same diagnostic depth `audio_status()` already provides for
ES1370, just placed under the existing `hda`/`hda status` per-controller
block instead of `audio`'s own per-device one (a controller can be
inspected either way: `audio` for the format-agnostic facade view every
backend shares, `hda` for the full node-graph-plus-playback view unique
to this driver).

**The reserved low-memory region grew again, from `0x80000` to
`0x90000`**, across all four required constants (`kernel/linker.ld`'s
`ASSERT`, `kernel/src/pmem.rs`'s `RESERVED_BELOW`, `tools/
write_layout.py`'s `KERNEL_RESERVED_END`, `tools/write_netrom_layout.py`'s
`MAX_KERNEL_BYTES`) -- diagnosed the same way this region's every prior
growth was: temporarily raising the linker `ASSERT` far past any
plausible real ceiling to get a linkable binary, then reading `.bss`'s
real end address via `readelf -S -W` (`0x83fb4`, about 16 KiB over the
prior `0x80000` ceiling) rather than trusting a naive `size`-command
`text+data+bss` sum, which undercounts `linker.ld`'s own 4096-byte
section-alignment padding.

Verified end to end with a new `tapes/test_hda_playback_e2e.py` (not
part of `make test`, mirroring `tapes/test_audio_net_e2e.py`'s own
structure almost exactly, just with `-device intel-hda,msi=off -device
hda-output,audiodev=snd0` in place of `-device ES1370,audiodev=snd0` --
`msi=off` is required since `kernel/src/hda.rs` only ever registers a
legacy PCI IRQ handler, and QEMU defaults a new-enough HDA controller to
MSI unless told otherwise): `audio` reports the discovered controller
with the expected fixed format, `hda` reports active playback with a
matching format line, `audio-net`'s listen mode accepts a locally
generated 440 Hz tone streamed over the wire exactly like the ES1370
test does, and the captured WAV file contains real, non-silent audio
(peak amplitude comfortably above a silence floor) -- proving the whole
chain (codec/DAC/pin discovery, BDL programming, the stream-reset
handshake, and IRQ-driven period refill) actually produces sound, not
just that the tape's own byte counters look right. `tapes/
test_hda_multi_e2e.py`, `tapes/test_audio_multi_e2e.py` (whose own
regex needed the same "I/O base" -> "base" wording update above),
`tapes/test_audio_net_e2e.py`, and `tapes/test_audio_capture_e2e.py`
were all re-run afterward and continue to pass unmodified (beyond that
one regex fix), confirming this is additive and does not regress
ES1370's own playback/capture paths. `make test`,
`make test-nic-watchdog`, `make test-stack-guard`, and
`make test-page-protection` were also re-run and continue to pass,
confirming the reserved-region growth and the shared-PCI-IRQ-line/
paging code this driver touches remain correct.

Real, per-codec multichannel negotiation (beyond this fixed 2-channel
ceiling) and validation against a real physical HDA-compatible sound
card both remain open, deferred follow-ups -- consistent with this
project's existing "confirmed in QEMU, flagged as unverified against
real hardware" convention for every other register-level driver here.
HDA capture (ADC) support is implemented -- see the next section.

### Intel HDA, step five: capture (ADC) support

The one HDA/audio gap this document's own "Anything left on the todo
list?" summary called out as still open -- HDA was playback-only, unlike
ES1370's existing dual DAC1/ADC support -- is now closed.
`kernel/src/hda.rs` gained a genuine second, independent capture stream
alongside its existing playback stream, mirroring `es1370.rs`'s own
DAC1/ADC pattern as closely as the two chips' register models allow:

- **`find_capture_target`** is the input-path mirror of
  `find_playback_target`: per this module's own doc comment on QEMU's
  synthetic `hda-duplex` codec layout, a pin's `GET_CONNECT_LIST` entry
  names its *source* converter regardless of direction -- an output
  pin's connection names its DAC, an input pin's connection names its
  ADC -- so this reuses the identical two-pass strategy (prefer a
  confirmed-connected input-capable pin/`AudioInput` pair, falling back
  to simply pairing the first `AudioInput` widget with the first
  input-capable pin on the same codec).
- **`setup_capture`** mirrors `setup_playback`, with three real
  differences: it uses stream index `0` directly rather than
  `input_stream_count`'s offset (per `STREAM_BASE`'s own doc comment,
  input streams occupy the *low* indices, unlike output streams which
  start immediately after them), it allocates its own separate DMA
  buffer (`capture_buffer_addr`, distinct from `buffer_addr`) rather
  than sharing playback's, and it programs the codec-side amp/pin verbs
  with `AC_AMP_SET_INPUT`/`AC_PINCTL_IN_EN` instead of their output-path
  counterparts (`AC_AMP_SET_OUTPUT`/`AC_PINCTL_OUT_EN`). A new
  `CAPTURE_STREAM_TAG` deliberately reuses the same numeric value as
  `PLAYBACK_STREAM_TAG` (stream tags are only ever matched against one
  direction's own `SD_CTL` register per the HDA spec, so there is no
  collision). Bails out cleanly (leaving `has_capture()` reporting
  `false`) on `input_stream_count == 0`, no confirmed ADC/pin pair, or a
  DMA allocation failure -- exactly as generously as `setup_playback`
  already treats its own failure paths, and for the same reason: a
  controller missing one direction is still perfectly usable for the
  other.
- **`write_sd_ctl`/`read_sd_ctl`** were generalized to take an explicit
  stream-index parameter (previously hardcoded to `self.stream_index`),
  so both directions' bring-up code can share these two helpers instead
  of duplicating the write-1-to-clear-safe register access pattern.
- **`handle_interrupt`** now checks `self.stream_index` and
  `self.capture_stream_index`'s own `SD_STS` bits unconditionally, not
  as an if/else -- mirroring `Es1370::handle_interrupt`'s own independent
  `STAT_DAC1`/`STAT_ADC` checks -- since both streams can legitimately
  complete a period on the same interrupt, and this controller's legacy
  PCI IRQ line may already be shared with another device entirely (see
  "A real bug this test found: shared legacy PCI IRQ lines..." above).
- **`drain_capture_period`** is the capture-path counterpart to
  `fill_period`: copies one just-completed period out of the ADC's own
  DMA ring into a new `CAPTURE_RINGS` static (mirroring `es1370.rs`'s own
  array of the same name exactly, including living as a standalone
  top-level array indexed by controller instance), incrementing
  `capture_overrun_count` on any bytes that don't fit -- there is no
  fallback content to generate on this side, unlike `fill_period`'s
  silence fallback, since captured samples either exist or they don't.
- **`audio.rs`'s facade** (`capture`/`capture_overrun_count`/
  `capture_backlog`/`capture_periods`) had its `Audio::Hda(_) => 0` stub
  arms replaced with real calls into the methods above, so any tape
  using the ABI's `audio_capture`/`audio_capture_at` callbacks now gets
  real HDA-sourced audio automatically, with no tape-side change needed
  -- the same reason `tapes/audio-net`'s existing `capture` mode (see
  "`tape run capture`: genuine live-microphone streaming" above) works
  against an HDA card without modification.
- **The dedicated `hda` shell command's per-controller status
  (`hda_status_one`)** gained a capture block mirroring its existing
  playback block (active format, periods captured, overrun count,
  capture ring backlog), reporting "No usable capture stream ..." for a
  controller with none, exactly like its existing playback fallback
  message.

Verified with a new `tapes/test_hda_capture_e2e.py`, mirroring
`tapes/test_audio_capture_e2e.py`'s structure closely (same `intel-hda`
+ `hda-duplex` codec combination `tapes/test_hda_e2e.py` already uses to
prove ADC/line-in discovery, `-audiodev none` since QEMU's packaged
audiodev backends have no genuine file-sourced fake-microphone input --
see that ES1370 test's own doc comment for why): playback and capture
periods both advance independently and simultaneously (proving
`handle_interrupt`'s unconditional dual check actually works, not just
compiles), and an undrained `CAPTURE_RINGS` slot correctly fills to
capacity and reports a steadily incrementing overrun count instead of
wedging. Every existing HDA/audio e2e test (`tapes/test_hda_e2e.py`,
`tapes/test_hda_multi_e2e.py`, `tapes/test_hda_playback_e2e.py`,
`tapes/test_audio_capture_e2e.py`) was re-run afterward and continues to
pass unmodified, confirming this is additive. `make test` was also
re-run and continues to pass; this segment also fixed an unrelated,
previously-stale `boot/stage2.S` soft-reboot resync loop bound
(`0x80000` -> `0x90000`, matching the reserved-region ceiling everywhere
else already tracks it -- see "Low-memory map" above).

Like `Es1370`'s own ADC support, this remains **unverified against real
hardware** -- every register offset/bit/DMA semantic was cross-checked
against QEMU's own HDA emulation, the same rigor and the same open
real-hardware caveat every other driver in this project carries until a
physical card actually confirms it.

### Intel HDA, step six: real per-codec multichannel negotiation

Every playback/capture stream up to this point was fixed at 2 channels
(stereo), the same ceiling ES1370 has for a genuine silicon reason (its
`SERIAL_CONTROL` format field only ever encodes mono or stereo). HDA has
no such ceiling in its own spec -- a converter widget's
`AUDIO_WIDGET_CAP` parameter carries a real
`AC_WCAP_CHAN_CNT_EXT` field (bits 13:15) reporting how many channels
beyond the base stereo pair it actually supports, and the stream-format
encoding both the controller's own `SD_FORMAT` register and
`AC_VERB_SET_STREAM_FORMAT` share already has room for up to 16 channels
in its low nibble -- so this was a real, unnecessary gap between what
HDA-based real hardware (a 4/6/8-channel-capable Sound Blaster PCI or
motherboard codec, exactly the "box of different sound cards" this
multichannel effort was originally scoped around) can do and what this
driver ever asked for.

**The authoritative channel-count formula**, confirmed directly from
Linux's own HDA driver source (`include/sound/hdaudio.h`'s
`snd_hdac_get_wcaps_channels`, and `sound/hda/common/hda_local.h`'s
identical `get_wcaps_channels`):

```
ext = (wcaps & AC_WCAP_CHAN_CNT_EXT) >> 13;
channels = ((ext << 1) | 1) + 1;
```

This formula is only meaningful once `AC_WCAP_STEREO` (a separate bit,
already read by this driver's existing `widget.stereo` field) is
confirmed set -- Linux's own caller (`sound/hda/common/proc.c`) always
gates on that bit before ever computing this formula, and this driver's
new `widget_max_channels(cap: u32) -> u8` helper mirrors that exactly: a
mono-only widget (stereo bit clear) reports `1` regardless of whatever
its `chan_cnt_ext` field happens to contain, since that field's meaning
is only defined relative to an existing stereo pair.

**A second, genuinely separate verb is required to *activate* more than
2 channels**, not just to encode the count in the stream format:
`AC_VERB_SET_CVT_CHAN_COUNT` (`0x72d`, `verb()` category, confirmed
against Linux's `include/sound/hda_verbs.h`), payload `channels - 1`.
This is distinct from both `AC_VERB_SET_CHANNEL_STREAMID` (`0x706`,
assigns a stream tag and *starting* channel only, already sent
unconditionally by `setup_playback`/`setup_capture`) and
`AC_VERB_SET_STREAM_FORMAT` (which encodes the channel count in the
format word, but per spec does not by itself widen a converter beyond
its default stereo activation). This driver sends
`SET_CVT_CHAN_COUNT` only when negotiation settles on more than 2
channels -- a widget reporting `chan_cnt_ext = 0` implies `channels <=
2`, where the verb is both unnecessary and not guaranteed to be
understood, consistent with this driver's existing "an unanswered verb
is not fatal" convention for every other optional codec-side step.

**Widget discovery** (`enumerate_afg_widgets`) now computes and stores
`max_channels: u8` on every `WidgetInfo` via `widget_max_channels`, right
alongside the existing `stereo` field it depends on. **Target
resolution** (`find_playback_target`/`find_capture_target`) now returns
a 4th tuple element, `min(converter.max_channels, pin.max_channels)` --
the pin itself can independently cap the real channel count on a codec
whose physical jack wiring supports fewer channels than its converter
electronics could otherwise drive, so the negotiated ceiling must
account for both widgets, not just the DAC/ADC. **`setup_playback`/
`setup_capture`** each clamp that negotiated value to `1..=
MAX_NEGOTIATED_CHANNELS` (a new driver-side ceiling, `8`, chosen as a
practical stopping point comfortably covering 7.1 surround without
unbounded growth) and compute their own local `format`/`bytes_per_frame`/
`period_bytes`/`buffer_bytes` from it via the new
`format_for_channels(channels) -> u32` helper (generalizing the old
fixed `PLAYBACK_FORMAT` constant), rather than assuming a single fixed
2-channel size everywhere. Playback and capture negotiate and store
their own channel counts independently (`Hda::playback_channels`/
`Hda::capture_channels`, both defaulting to `2` before either stream is
brought up) precisely because a codec's ADC and DAC converters need not
report the same `AC_WCAP_CHAN_CNT_EXT` capability -- `fill_period`/
`drain_capture_period` each read their own direction's negotiated
per-frame byte size via new `playback_bytes_per_frame`/
`playback_period_bytes`/`capture_bytes_per_frame`/`capture_period_bytes`
methods instead of a single shared constant.

**One pre-existing, accepted ABI limitation this task does not
resolve**: `audio.rs`'s tape-facing `Audio::format()` is one shared
method per device, used by the tape ABI's `audio_channels_and_bits`/
`_at` callbacks for *both* `audio_submit` (playback) and
`audio_capture` (capture) directions. `Hda::format()` therefore reports
`playback_channels` specifically (surround/speaker output being the
primary motivation for this work), not a per-direction pair -- on a
real codec whose ADC and DAC negotiate genuinely different channel
counts, a tape reading captured bytes via `audio_capture`/`_at` should
not assume they match `format()`'s reported channel count. This mirrors
an identical, already-accepted asymmetry in `Es1370`'s own single shared
`format()` method, and in practice is a non-issue against every existing
QEMU test today, since QEMU's codecs negotiate both directions down to 2
either way.

**QEMU is a hard verification ceiling for this specific feature, not
just the usual "not yet tested against real hardware" caveat.** Every
one of QEMU's synthetic HDA codec models (`hda-output`/`hda-duplex`/
`hda-micro`, confirmed directly by reading `hw/audio/hda-codec.c`)
reports `chan_cnt_ext = 0` on every widget it exposes, and QEMU's own
`hda_codec_parse_fmt()` performs no capability check against a
converter's real channel count at all -- meaning every existing and
future automated QEMU-based test can only ever exercise the "negotiate
down to the base stereo pair" path through this new logic, never the
>2-channel path itself, `SET_CVT_CHAN_COUNT` being sent, or
`format_for_channels` producing anything but its 2-channel value. This
is the same honestly-documented class of gap this project already
accepts for I350 (no QEMU model exists at all) and real-microphone ADC
content (QEMU's audiodev backends have no fake-microphone input) --
here, a QEMU model exists and is exercised, but its codec's own reported
capabilities cannot exercise this feature's actual new code path.
Confirming genuine >2-channel negotiation, the `SET_CVT_CHAN_COUNT`
verb's real effect, and audible surround output all remain open,
physical-hardware-only verification steps against the user's own
multichannel-capable Sound Blaster PCI cards.

Verified with the full existing HDA/audio test suite
(`tapes/test_hda_e2e.py`, `test_hda_playback_e2e.py`,
`test_hda_multi_e2e.py`, `test_hda_capture_e2e.py`,
`test_audio_capture_e2e.py`, `test_audio_multi_e2e.py`,
`test_audio_net_e2e.py`, `test_audio_net_multi_e2e.py`, and `make
test`), all passing unmodified -- expected, since every QEMU codec this
project tests against negotiates to exactly 2 channels either way, so
this change is provably additive and non-regressing for every currently
automatable scenario.

