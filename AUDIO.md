## Real-time audio streaming: the ES1370 driver, and Intel HD Audio

`kernel/src/es1370.rs` and `kernel/src/hda.rs` are two independent PCI
sound-card drivers, exposed to tapes through a shared, format-agnostic
facade (`kernel/src/audio.rs`, mirroring `nic.rs`'s own multi-backend
shape) and the `audio`/`hda` shell commands. ES1370 (Ensoniq/Creative
"Sound Blaster PCI", QEMU's `-device ES1370`) implements DAC1 (playback,
one always-on stereo 16-bit channel with a default test tone) and ADC
(recording) as two independent, simultaneously running DMA channels, with
up to `MAX_INSTANCES` (4) cards supported at once. Intel HDA ("HD Audio",
QEMU's `-device intel-hda` + a codec such as `hda-duplex`) is a genuinely
different architecture -- a generic CORB/RIRB DMA command-transport engine
talking to one or more codecs on a private bus, each exposing a node graph
(root -> Audio Function Group -> DAC/ADC/pin-complex widgets) -- with
stream-descriptor/BDL-based playback and capture, real per-codec channel-
count negotiation (`AC_WCAP_CHAN_CNT_EXT`, up to `MAX_NEGOTIATED_CHANNELS` =
8), and up to `MAX_INSTANCES` (2) controllers supported at once.

The tape ABI's audio callbacks (trailing, append-only cdecl, after the
`nic_*`/`ticks` set): `audio_sample_rate`/`audio_channels_and_bits`/
`audio_submit`/`audio_capture` (device 0 only, kept for backward
compatibility) and their indexed counterparts `audio_count`/
`audio_sample_rate_at`/`audio_channels_and_bits_at`/`audio_submit_at`/
`audio_capture_at`. `audio_submit`/`_at` push raw PCM into a lock-free,
network-fed ring (`PcmRing`, 16 KiB per direction per device -- a real,
measured link-time codegen ceiling, not an arbitrary choice) that the
DAC's own ISR drains each period; calling it at all permanently switches
that device from its default test tone into network-fed mode.
`tapes/audio-net/` is the reference tape: `tape run` (listen mode, receiving
one or more per-device-tagged network sub-streams and forwarding them to
local playback), `tape run send NAME [MAC]` (replays a pre-recorded FOSFS
file), and `tape run capture [MAC]` (genuine live-microphone streaming,
round-robining every local device via `audio_capture_at`).

HDA's own sample rate and bit depth are negotiated, not fixed: `hda.rs`
queries a discovered DAC widget's `AC_PAR_PCM` capability and prefers
48 kHz over 44.1 kHz and 24-bit over 16-bit whenever the widget actually
advertises support, falling back to the historical 44.1 kHz/16-bit
baseline otherwise (`negotiate_format`) -- unlike channel count, which
was already negotiated this way via `AC_WCAP_CHAN_CNT_EXT`, this was
previously just a hardcoded `0x4010` format value, chosen for parity with
ES1370, not a hardware limit HDA silicon actually has. ES1370 itself is
unaffected: it is genuinely silicon-limited to 44100 Hz/16-bit (only 4
hardwired rates, format field only encodes 8/16-bit PCM). **24-bit
specifically cannot be exercised under QEMU at all**: its synthetic HDA
codec only ever implements 16-bit PCM (`QEMU_HDA_PCM_FORMATS` in
`hda-codec-common.h`), so that path is real-hardware-only, exactly like
this section's own already-documented "unverified against real
hardware" status below -- and the 24-bit sample container size (a 4-byte
container per sample, matching Linux ALSA's `S24_LE` convention, chosen
over RFC 3190's tightly-packed 3-byte wire convention) is a documented
assumption pending that same real-hardware confirmation, not a verified
fact.

Both drivers are, as of this writing, unverified against real hardware
(every register offset/bit/DMA semantic was cross-checked directly against
QEMU's own emulation source, the same rigor applied to every other driver
here after the RTL8139 lessons in "Real hardware notes" above) -- the
user's box of physical Sound Blaster PCI/HDA cards is the intended
validation target. A real, previously-undiscovered kernel bug (two PCI
devices sharing one legacy IRQ line silently broke whichever device's
`init()` ran second) was found and fixed while testing this feature; see
"shared PCI IRQ lines" above. Real-microphone testing on a dev machine
surfaced two host-side QEMU/PipeWire pitfalls and one seat0/logind
device-ACL gotcha, unrelated to fos itself.

The full chronological development history -- the ES1370 interrupt-storm
bug (a level-triggered status bit the driver never acknowledged, causing an
interrupt storm that looked exactly like "no interrupt fired"), the
`.bss`/link-budget investigation behind `PcmRing`'s sizing, multi-instance
ES1370, the audio-net wire-protocol's per-device tagging design, and every
HDA increment (discover-only, CORB/RIRB + node-graph enumeration,
multi-controller, stream-DMA playback, capture, and multichannel
negotiation) -- is in `docs/history/audio-hda.md`.

## AC'97: a third audio backend, and reduced ES1370 headroom to make room for it

`kernel/src/ac97.rs` is a third PCI sound driver, alongside ES1370 and HDA,
for Intel's ICH-family AC'97 ("Native Audio Mixer" / "Native Audio Bus
Master") controller -- motivated by a real, physically owned machine (the
Pentium 4/ICH4 test box used throughout this file's real-hardware
sections) whose onboard audio is otherwise permanently inaccessible: fos
has no driver for any other codec family that chipset exposes. It is
QEMU's own emulated sound device too (`-device AC97`, PCI ID `8086:2415`,
Intel's original `82801AA`), so, unlike the still-fully-real-hardware-only
I350/3C509 drivers, this one gets real `make test`-style QEMU coverage.
`ac97::CANDIDATE`-equivalent discovery matches exactly two IDs: `0x2415`
(QEMU's own model) and `0x24d5` (the confirmed real ICH4 device) -- a
deliberately narrow pair, not a speculative broadening across every
ICH-generation ID, even though Intel's own AC'97 register layout is
documented backward-compatible across generations.

**Scope: playback-only, no network-fed streaming yet.** Only the PCM-out
bus-master channel is programmed (no PCM-in/mic-in), always playing a
built-in test tone; `ac97::MAX_INSTANCES` is 1 (a motherboard
south-bridge function, never more than one per real machine).

Fitting this driver in required correcting a real measurement bug in
this project's own "check `size build/kernel.elf`" convention (summing
its text/data/bss columns ignores `linker.ld`'s `ALIGN(4096)` padding
gaps -- see "Low-memory map" above for the corrected `__kernel_end`-based
guidance) and reclaiming `.bss` headroom by dropping
`es1370::MAX_INSTANCES` from 4 to 2. A real interrupt-storm bug (an
unacknowledged `SR_FIFOE` status bit re-firing the shared PCI IRQ line)
was found and fixed while bringing this driver up under QEMU -- the same
class of bug `es1370.rs`'s own `acknowledge_dac1` doc comment already
describes, now hit a second time in a second driver.

**Confirmed on the real P4/ICH4 machine this driver targets -- but that
machine's onboard AC'97 has no physical codec behind it.** Both BAR0 and
BAR1 read back as a hard `0` even after `PciAddress::assign_io_bar`'s
fallback-assignment attempt, in full D0 power -- per the PCI spec, this
means no register storage exists behind either BAR in silicon at all,
not a firmware oversight: this specific board's ICH4 south bridge
exposes the AC'97 function in PCI config space without an actual codec
chip ever populated (audio was a discrete add-in-card option on boards
of this era). The driver itself is otherwise confirmed correct end to
end under QEMU.

Full detail (the exact register-level layout cross-checked against
QEMU's `hw/audio/ac97.c`/`ac97.h` source, the sliding-`LVI` ring
technique's own derivation, the alignment-padding investigation
narrative, and the full real-hardware BAR-assignment investigation) is in
`docs/history/audio-hda.md`.

## Real-time audio streaming over RTP/UDP: the `audio-rtp` tape

`tapes/audio-rtp/` streams the same raw PCM audio `audio-net` does, but
over genuine RTP (RFC 3550) carried in UDP/IPv4 instead of a bespoke
raw-Ethernet tag protocol, so a real tool (`ffmpeg`, `gstreamer`, any RTP
client) can send to or receive from `fos` directly, not just another fos
instance. `tapes/tape-net/src/lib.rs` gained a matching `RtpPacket` type
(parse/encode over borrowed slices/caller-owned buffers) and a
`sequence16_at_or_after` wraparound helper; see `NETWORKING.md`'s "RTP
scope"/"`tapes/audio-rtp`" sections for the full wire-format design.
Format is adaptive, not fixed: the tape maps the local device's actual
format to one of a small set of RTP payload types it understands (RFC
3551's `L16`/44100/stereo = PT 10 for ES1370; two documented dynamic-range
conventions for HDA's negotiable 48 kHz/16-bit and 48 kHz/24-bit) and
rejects anything else outright rather than generalizing. Scope is device 0
only per invocation: `listen`/`send`/`capture` each drive exactly one
direction; `duplex PEER_IP [PEER_PORT]` is the one mode that drives both
directions at once (still device 0 only), over one shared local UDP port
(always `DEFAULT_PORT`/5004).

Opus support (`tapes/opus-codec/`, this project's first vendored C
dependency, built from libopus 1.5.2's fixed-point, decode-and-encode
source tree by the root Makefile's `opus-codec-lib` recipe) adds a fourth
fixed dynamic payload type, `PAYLOAD_TYPE_OPUS` (98): `listen_mode`
decodes it via a persistent `opus_codec::Decoder`, and `send`/`capture`/
`duplex` accept `[opus] ... [BITRATE]` (default
`opus_codec::DEFAULT_BITRATE_BPS`, 96 kbps) to encode via
`opus_codec::Encoder`. Both directions are gated to HDA's negotiated
16-bit/48 kHz-family format specifically (ES1370's fixed 44100 Hz doesn't
match libopus's five accepted rates, and this vendored build has no float
API so it only ever produces/consumes 16-bit PCM). The decoder/encoder
state buffer is allocated through the tape ABI's own `alloc`/`free`
callbacks (`custom_support.h` redirects libopus's internal
`opus_alloc`/`opus_free`), and `config.h` uses libopus's
`NONTHREADSAFE_PSEUDOSTACK` scratch-allocation mode rather than
`VAR_ARRAYS`, since the encoder's SILK pipeline needs more scratch than
fos's kernel stack guard page tolerates when tapes run nested inside the
shell's own call chain.

Verified end to end by `tapes/test_audio_rtp_e2e.py` (raw PCM),
`test_audio_rtp_opus_e2e.py`/`test_audio_rtp_opus_send_e2e.py` (Opus
decode/encode against a real Intel HDA QEMU guest), and
`test_audio_rtp_duplex_e2e.py` (concurrent duplex). Confirmed against the
real TerraTec Aureon Dual USB card (via `usb-host` passthrough, the same
device the UHCI section above uses): this surfaced a real bug -- capture
code was reusing the *playback* format for a device whose two directions
differ (stereo playback, mono capture) -- fixed by `capture_local_format()`
sourcing from the tape ABI's dedicated `audio_capture_sample_rate`/
`audio_capture_channels_and_bits` callbacks instead of `local_format()`;
`duplex opus` against the real card now plays and captures correctly
labeled audio simultaneously. Real-hardware interop with a genuine,
unmodified `ffmpeg` session, and the 24-bit path this tape's format table
already accounts for, remain future work. Full detail (the Opus
stack-overflow investigation, every e2e test's verification methodology,
and the real-hardware duplex bug narrative) is in
`docs/history/audio-hda.md`.
