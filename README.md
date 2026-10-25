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
| `fterm` | Serial terminal on COM2-COM4 (`run fterm list`, `run fterm 1 115200 8n1`); Ctrl-A x exits. `run fterm telnet IP|NAME [PORT] [dns IP]` (names go through DNS; the Telnet window is the real console size) and `run fterm net NAME\|MAC` do Telnet and `term net` with the same keys. A bare `run fterm` opens a settings panel (P picks a machine from the announced ones, C connects, W saves `fterm.cfg`, `run fterm @fterm.cfg` repeats it). USB serial adapters (the Moxa UPort 1110, which needs `moxa-1110.fw` on a FOSFS volume and a `usb rescan`, and Prolific PL2303 cables) appear as `USB1`... after the UARTs. |
| `fpcheck` | Checks that a hard-float tape (SSE2, AVX2/FMA when the CPU has them) runs; the template for DSP tapes (`make test-fp`). |
| `gfxcheck` | Checks the tape graphics calls (`gfx_*`): colour bars, stride blit, clipping; `gfxcheck [secs=N]` (`make test-gfx`). |
| `gonio` | Goniometer: phosphor scope, Nordic PPM / dBFS meters with peak hold and clip lamp, correlation and balance bars, `theme=` / `k` colour themes, LUFS panel (M/S/I/LRA, target and EBU +9/+18 scale, history), adapts to the screen; `chan=A,B`, `,` `.` step the channel pair, `src=test` built-in stereo tone; hard-float; `w` saves the settings in `gonio.cfg` (`@NAME` loads another file); `gonio help` (`make test-gonio`). |
| `clock` | Wharton-style studio clock: 60 red seconds dots, yellow 24 h (or `hours=12`) HH:MM with the seconds below, the date; time from the best source (`source=auto`, the default: PTP over Ethernet, PTP over UDP, NTP, then the machine clock; PTP silent for 30 s falls back to NTP) or one only: NTP (`source=ntp ntp=dhcp\|ADDR\|NAME`, DHCP option 42, resynced every `resync=` s) or a PTP master (`source=ptp [l2] domain=N`, `l2` = Ethernet, no IP needed), sets the machine clock and RTC; `tz=+2` (else guessed), `utc`, `lead=N` (cue: the next 5-second mark lit ahead of the count), `h` `u` `n` `k` `l` keys, `w` saves `clock.cfg`; hard-float (`make test-clock`). |
| `kaffe` | A coffee board for a kitchen screen (the fos version of radiokannan's touch page): big red clock, a "Kaffet klart!" button that stamps the time ("Kaffet nybryggt kl. HH:MM", kept in `kaffe.cfg`, still there after a restart) and "Rensa"; keys Enter/Space and `c`; a USB or Pi DSI touch screen presses the buttons (ABI 24 `touch_read`); hard-float, x86 and Pi (`make test-kaffe`). |
| `organ` | A Hammond-style drawbar organ for demonstrating MIDI, DSP and graphics: nine drawbars, six presets, percussion, scanner vibrato/chorus, Leslie (Stop/Slow/Fast; the MIDI mod wheel picks the speed), overdrive and reverb, 61 keys (`crates/organ`). Played from a USB-MIDI keyboard (ABI 25 `midi_read`), the touch screen (drawbars, switches, keys, sliding across the keyboard) or typed keys (`z s x d c ...`, Left/Right shift the octave, Tab toggles the Leslie speed); scope and peak meter on screen; float or 24-bit output; `latency=MS`, `rate=HZ`, `out=N` (left channel of the output pair on a multichannel card, e.g. `out=9`); hard-float, x86 and Pi (`make test-organ`; MIDI and touch need real hardware). |
| `strings` | A divide-down string machine in the Solina mould (`crates/strings`): a top-octave generator divided down by octaves, six sections (Violin, Viola, Cello, Bass, Brass, Piano) with a fader each, attack/release, three-voice ensemble chorus, phaser, tape echo (`crates/tape-echo`), reverb and the electric piano's wah (`fx::Wah`: Vowel, Q, Range, an envelope follower, a Wah pedal slider or expression pedal CC11). USB-MIDI (notes, mod wheel, sustain pedal CC64), touch screen (faders, sliders, keys) or typed keys; the mod wheel sweeps the phaser speed, the ensemble depth, both, or the wah (on-screen switch); the on-screen keyboard is hidden while a MIDI keyboard is connected; same `device=`/`rate=`/`latency=`/`out=` arguments as `organ`; hard-float, x86 and Pi (`make test-strings`; MIDI, touch and the sound itself need real hardware). |
| `tape-echo` | A tape echo in the Roland RE-201 style as an effect box (`crates/tape-echo`): live audio in, a tape loop with three playback heads, repeat rate with pitch glide, intensity, wow/flutter, tape saturation, bass/treble, echo pan, Freeze, tap tempo and a spring reverb (`fx::Spring`). Twelve sliders and head/Freeze/Tap buttons on the touch screen, MIDI controllers 20-31, 64, 65, 102-104; `device=`/`outdev=`/`rate=`/`latency=`/`in=`/`out=`; hard-float, x86 and Pi (`make test-tape-echo`; latency, sound, touch and MIDI need real hardware). |
| `epiano` | A Rhodes-style electric piano for playing live (`crates/epiano`): 20 voices of struck tines (fundamental, bell and ting; velocity brings out the bell), hammer thump, a pickup per tine (Bark: hard hits bark, soft ones stay sweet), a vowel filter that a hard hit opens and that closes as the note dies (Vowel, Glide, Q, Range; an expression pedal or the mod wheel works it as a wah), tube overdrive, chorus, tremolo auto-pan, phaser, the tape echo and a reverb. Pitch wheel runs the echo's tape faster or slower, mod wheel (CC 1) goes to tremolo / echo feedback / bark / all / wah, sustain pedal, 88 keys from USB-MIDI; touch keys strike harder lower on the key. Twenty-two sliders (Wah: an on-screen wah pedal; Soft: a velocity floor for keyboards that play loud), six tine models (Bar, Tine, FM, Hybrid, Metal, Chime) and a vowel/sweep switch on the touch screen; default latency 10 ms (`latency=`), MIDI polled before every audio top-up; hard-float, x86 and Pi (`make test-epiano`; tone, latency and MIDI need real hardware). |
| `phono` | A phono equaliser (`crates/phono`): a flat cartridge or preamp signal in, 21 record equalisation curves (RIAA, IEC, Columbia LP, NAB, AES, London, Teldec, RCA, and the 78 rpm ones: IEC/CCIR/Decca/Columbia/RCA/Euro/Blumlein/flat-500/American), mono, polarity (invert L/R), a rumble filter, an adjustable bass boost (0-12 dB at 60-150 Hz, Q 0.3-3) and trim. Touch screen shows the live response over the analogue curve, a button per curve, sliders and Save (writes `phono.cfg`, the file `phono service` reads); `service` runs headless as a foreground tape (quiet, until Escape, fits `start`); settings also as `curve=` `mono=` `polarity=` `rumble=` `bass=` `bassfreq=` `bassq=` `trim=` words or `@FILE`; hard-float, x86 and Pi (`make test-phono`; the sound needs real hardware). |
| `ltas` | Long-term average spectrum of the audio input, live on the screen (1/3, 1/6, 1/12 octave or FFT; live, exponential, LTAS and peak curves; A/C/Z weighting) with a noise generator (white, pink, sine, bursts) on the output; hard-float, AVX2 FFT where present; `ltas help` (`make test-ltas`); `theme=dark|classic|light` (`k` cycles; light is for paper), `s` saves a CSV and a PNG to an exFAT disk. |
| `ptp` | PTPv2 (IEEE 1588, the clock AES67 uses) over UDP, or over Ethernet with `l2` (ethertype 0x88F7, no IP; `ptp4l -2`): `tape run ptp [master] [l2]`; `master both` serves UDP and Ethernet slaves at once. As a slave it measures this machine's clock offset, path delay and rate against a master. `master ntp=dhcp\|A.B.C.D` first asks an NTP server the time (DHCP option 42, else the gateway) so the master's time is real; `settime tz=+2` on a slave then sets that machine's clock and RTC from it and ends (`date` shows it); the NIC is the one chosen with `nic use N`. |
| `router`, `bridge`, `nettest` | A layer-3 router (NAT, DHCP, health-checked uplinks), a layer-2 learning bridge, a link-impairment and traffic generator. |
| `play`, `record`, `recorder`, `audio-net` | WAV playback and recording, a field recorder, audio streaming between hosts (raw Ethernet or RTP, Opus). RTP follows a PTP clock when one is found (`ptp=auto\|l2\|off`; raw and AVTP use Ethernet PTP, `ptp l2`) and a receiver shows the one-way delay. `priority=wifi\|safe\|balanced\|low\|live\|auto` picks ring and packet time (40 ms/2 ms ... 3 ms/0.125 ms). `bench peer=NAME` (panel key T; the other machine runs `benchserve`) measures the link both ways and recommends the lowest `ptime`/`target`. |

**USB MIDI** (kernel, class-compliant devices on xHCI): `midi` lists devices, `midi monitor` prints incoming messages until Escape, `midi send [#N] 90 3c 64` sends one 1-3 byte message. Verified on real hardware with a cable looped out to in; QEMU has no USB-MIDI model, so `make test-midi` only covers the no-device paths.

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

A quiet tape can run in the background as a service that owns one NIC: `run ptp master ip=dhcp &1`; `ps` lists services; `kill N`, `kill NAME` or `kill all` ends them (see `docs/design.md`, "Services").

Between machines, `tapesync [HOST] [apply]` pulls newer tapes from one peer, or from all discovered ones, that share the `synckey` file (see `docs/design.md`).

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
