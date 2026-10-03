# nettest: a network test lab in one tape

`nettest` turns a fos machine with several network ports into a small test lab.
It can sit between two ports and make the link behave badly (lose, delay,
slow, duplicate, reorder or damage frames), generate numbered traffic at a
chosen rate, and measure what arrives: throughput, loss, reordering, one-way
latency and jitter. All of it runs in one tape, so a single machine with four
ports can test a link emulation against its own sink, and a machine with two
ports can test a router.

This manual covers the whole tool. The tape's own source comment
(`tapes/nettest/src/main.rs`) and `README.md`'s tape command table are short
references; the numbers quoted here come from real runs (QEMU and the Skylake
test machine).

Contents: [the picture](#the-picture) · [before you start](#before-you-start-know-your-ports) ·
[quick start](#quick-start) · [running it](#running-it) · [impairments](#the-impairment-link) ·
[presets](#link-presets) · [generator and sink](#generator-and-sink) ·
[routable traffic](#routable-udpip-traffic) · [sweeps](#rate-sweeps) · [both directions](#both-directions-at-once) ·
[results](#results-summaries-histograms-and-report-files) · [recipes](#recipes) ·
[performance and limits](#performance-and-limits) · [troubleshooting](#troubleshooting) · [tests](#testing-the-tool-itself)

## The picture

```
   generator            impairment link              sink
   (NIC 2) ----cable----> (NIC 3)   (NIC 4) ----cable----> (NIC 5)
        "gen=2"          "nettest 3 4"                    "sink=5"
```

* The **impairment link** is two ports. A frame arriving on one is carried out of
  the other after being run through the configured faults. Each direction is
  configured separately: `ab` is port A to port B, `ba` the reverse. Both ports
  listen promiscuously. It does no routing and learns nothing; it is a
  two-port, Ethernet-only pass-through (like a cable with bad manners).
* The **generator** sends frames from its NIC at a constant bit rate.
* The **sink** receives them on its NIC and keeps the books.

Any of the three can be used without the others:

| You give | You get |
| --- | --- |
| `3 4` only | an impairment link between NICs 3 and 4 (default `1 2` if nothing at all is given) |
| `gen=2 sink=3` | a generator and a sink, no link: tests a cable, a switch, a router |
| everything | the full lab above |
| `bidir=on` | the sink's NIC also generates traffic back, so both directions are loaded |

Every role needs a NIC of its own. Frames to or from the tape's own NICs are
treated as management traffic (for example a `term net` session) and are never
impaired.

## Before you start: know your ports

A machine with four or five identical ports has no labels that mean anything.
Ask the machine:

```
fos> nic map
Probing 5 NIC(s) for links and cable connections...
NIC 1: 18:66:da:25:1f:73 (active) link 1000M full
    sees 1 other host(s)
NIC 2: a0:36:9f:be:42:78  link 1000M full
    sees NIC 3 -- direct cable
NIC 3: a0:36:9f:be:42:79  link 1000M full
    sees NIC 2 -- direct cable
NIC 4: a0:36:9f:6b:fc:5a  link 1000M full
    sees NIC 5 -- direct cable
NIC 5: a0:36:9f:6b:fc:5b  link 1000M full
    sees NIC 4 -- direct cable
```

Every NIC in turn broadcasts a few probe frames while the others listen. "direct
cable" means exactly one other NIC of this machine heard it and nothing else did
on either end; "same network" means they share a switch (or there are other hosts
around). The link line comes from the driver's status register where there is
one (`down` is shown when the driver can tell); a card that cannot report shows
`unknown`. If nothing is heard in the first round it retries up to twice while the
links come up.

Cable plan used in this manual: NIC 1 stays on the LAN for the management
session (`term net`), NIC 2 to NIC 3 and NIC 4 to NIC 5 are patch cables with
nothing else on them.

**Never connect both ends of the impairment link to the same switch or network.**
The tape forwards everything it hears between its two ports, so doing that makes a
loop. Use direct cables or separate segments.

## Quick start

All three examples assume the cable plan above and a management session on a NIC
that is not part of the test (here NIC 1).

**1. Self-test: the whole lab on one machine.**

```
fos> tape run nettest selftest 2 3 4 5
```

`selftest GEN LINK_IN LINK_OUT SINK` means: generator on NIC 2, the impairment
link between NICs 3 and 4, sink on NIC 5. By default it sends 100 Mbit/s of
1000-byte frames for 10 seconds, starting after 3 seconds, then prints a summary.
Add settings after it to bend the link:

```
fos> tape run nettest selftest 2 3 4 5 rate=50000 loss=1 gen.time=5
...
generator finished: sent 62500, failed 0
summary: sent 62500, got 32614 (50001 kbit/s over 5.2 s), loss 47.81%
  latency us: median <500000, 95% <500000, 99% <500000, max 224374, jitter 15
```

(100 Mbit/s offered into a 50 Mbit/s limiter with a 1% random loss: the sink
measures the limiter's 50001 kbit/s; the deep default queue shows up as a
quarter of a second of latency.)

**2. Router test: a router between two ports.**

```
fos> tape run nettest routertest 2 5
```

Generator on NIC 2, sink on NIC 5, both directions at once, UDP/IP frames, so a
router cabled between the two ports has to ARP and forward them. It assumes the
router is `10.0.1.1` on NIC 2's side and `10.0.2.1` on NIC 5's side; the
generator is `10.0.1.2` and the sink `10.0.2.2`. The router under test can be a
fos machine running `tape run router no-nat 1 10.0.1.1/24 2 10.0.2.1/24`, or any
other router set up that way.

**3. Just a bad link, by hand.**

```
fos> tape run nettest 3 4 preset=badwifi
nettest> view on
nettest> loss ba 20
nettest> quit
```

NICs 3 and 4 become a cable with weak-signal WiFi in it; the prompt changes it
live.

## Running it

### Command line

```
tape run nettest [A B] [settings...] [gen=N] [sink=N] [gen.KEY=V ...] [sink.KEY=V ...]
tape run nettest selftest GEN LINK_IN LINK_OUT SINK [settings...]
tape run nettest routertest GEN SINK [settings...]
tape run nettest @FILE
```

NIC numbers are one-based, as in `nic list`. Numbers before any setting are the
two impairment ports. Settings are `name=value` words; `name.ab=` / `name.ba=`
apply to one direction. A word without `=` starts an ordinary command that takes
the rest of its line (`;` or a newline starts a new line). At most 24 words per
line. The leading word `nettest` is optional.

`@FILE` reads the settings from a FOSFS file (4096 bytes at most, `#` starts a
comment, one setting or command per line). `nettest.cfg` on every image is an
annotated example. A config file is the right way to run the same test
unattended: add `gen.save=NAME` and the report is on disk when you come back.

### The prompt

While it runs the tape shows `nettest> ` and reads the keyboard (or the `term net`
session). Commands change the running test. **Escape stops the tape** (over `term
net`, a lone Escape key press). `quit` does too. Counters print when it stops.

Typing is polled about once a millisecond while traffic is flowing, so it never
starves the forwarding, but the console output of a command can be slow on a slow
link (a `term net` session on 100 Mbit/s, a serial line). Keep the management
session on a NIC that is not under test.

### The statistics panel

`view=on` at startup or `view on` at the prompt reserves the top rows of the
screen for a live panel, redrawn twice a second, and keeps the prompt scrolling
below it. `view off` removes it. It uses only ANSI sequences, so it looks the same
on the local screen and over `term net` or serial (in a real terminal). It shows,
per impaired direction, the settings, frames in/out with rates, losses, queue
drops and a queue-depth bar; for the generator its settings and state; for the sink
the counters, the current and average rate, and latency; and the reverse stream's
numbers in bidirectional mode.

## The impairment link

Settings (startup `name=value`, or the same word as a command; `ab`/`ba` selects a
direction, default both):

| Setting | Meaning |
| --- | --- |
| `loss=PCT[:BURST]` | Drop `PCT`% of frames (steps of 0.1). `BURST` makes each loss take that many frames in a row: `loss=5:4` drops about 5% of frames in runs of four. |
| `delay=MS[:JITTER]` | Hold every frame `MS` milliseconds, plus or minus a random `JITTER` ms. Jitter never reorders frames; use `reorder` for that. |
| `rate=KBIT` | Link speed in kbit/s; 0 is unlimited. Frames take their serialization time and queue behind each other. |
| `queue=N` | Frames allowed in flight (default: all the storage, about 1365 per direction with the big pool, 256 without). A full queue drops new frames (tail drop). |
| `dup=PCT` | Deliver a frame twice. |
| `reorder=PCT` | Hold a frame back so the next one overtakes it (it is released at most 50 ms later if nothing follows). |
| `corrupt=PCT` | Flip one random bit in the payload, after the Ethernet header, so the frame still reaches the right station but fails a checksum above it. |
| `preset=NAME` | A whole profile, see below. |
| `seed=N` | Fixes the random choices, so a run repeats exactly. |

Things worth knowing:

* **Time has 1 microsecond resolution** on CPUs with an invariant time-stamp
  counter (all recent x86), 10 ms on older ones.
* **A `rate` plus a deep `queue` is bufferbloat.** Latency under overload is
  `queue × frame size ÷ rate`: a 100-frame queue of 1000-byte frames behind 50
  Mbit/s adds 16 ms; the default 1365-frame queue adds 220 ms. Set `queue` the way
  a real device would.
* **Faults apply in this order:** loss, queue limit, delay and rate timing,
  reordering, then corruption and duplication of what was queued. A lost frame is
  never also corrupted.
* The impairment link is Ethernet-level and protocol-blind; it forwards ARP,
  IPv4, IPv6, anything.

### Link presets

`preset=NAME` (or `preset ab NAME` at the prompt for one direction) sets every
field at once; later settings override. `presets` lists them.

| Name | Loss | Delay | Rate | Queue | Dup / reorder |
| --- | --- | --- | --- | --- | --- |
| `badwifi` | 10%, bursts of 6 | 40 ± 60 ms | 3 Mbit/s | 64 | 0.5% / 2% |
| `wifi` | 3%, bursts of 4 | 15 ± 10 ms | 20 Mbit/s | 128 | 0.2% / 0.5% |
| `cellular` | 1% | 70 ± 30 ms | 8 Mbit/s | 128 | |
| `satellite` | 1% | 600 ± 20 ms | 10 Mbit/s | 512 | |
| `dsl` | | 20 ± 2 ms | 2 Mbit/s | 48 | |

### Commands

```
loss [ab|ba] PCT [burst N]    delay [ab|ba] MS [JITTER]    rate [ab|ba] KBIT
queue [ab|ba] N               dup|reorder|corrupt [ab|ba] PCT
preset [ab|ba] NAME           presets          seed N
show    stats    reset [stats]    view on|off    help    quit
```

`reset` clears the faults, `reset stats` the counters. `show` prints the settings,
`stats` the counters.

## Generator and sink

`gen=N` makes NIC N the generator and `sink=N` makes NIC N the sink. Each frame
carries a sequence number and a transmit timestamp, so the sink can count loss,
duplicates and reordering from the sequence numbers, and measure latency by
comparing the timestamp with its own clock. **Because the generator and the sink
are in the same tape they share one clock, so the latency is exact one-way
latency**, with no clock synchronization to get wrong.

Generator settings:

| Setting | Meaning |
| --- | --- |
| `gen.rate=KBIT` | Offered load in kbit/s, counting the whole Ethernet frame (header included; no preamble or FCS), so 1000 kbit/s of 1000-byte frames is 125 frames a second. Default 1000. |
| `gen.size=BYTES` | Frame size, 60 to 1514. Default 1000. |
| `gen.to=MAC` or `sink` | Destination MAC in raw mode. The default is the local sink's MAC, or broadcast without one. |
| `gen.count=N` / `gen.time=S` | Stop after N frames / S seconds (0 = no limit). |
| `gen.run=on` | Start sending when the tape starts. |
| `gen.delay=S` | …after S seconds. Links need a moment after a tape starts promiscuous mode; 3 s is a safe default (QEMU's `e1000` model needs the full second). |
| `gen.save=NAME` | Write a report file when the test finishes. |

Commands: `gen start`, `gen stop`, `gen show`, and `gen rate|size|to|count|time
VALUE`. `sink`, `sink show`, `sink reset` and `sink hist` show or clear the sink.

Roles can be re-pointed at the prompt without quitting: `gen nic N|off`,
`sink nic N|off` and `link A B|off` (one-based NIC numbers; each NIC may have
only one role). Moving the generator stops it, moving the sink clears its
counters, and `link` clears the queues and moves promiscuous mode with it.

The generator never sleeps while running, so expect 100% CPU on that machine; it
sends everything that has come due each pass, up to 16 frames, and counts the
times it fell too far behind its schedule ("fell behind schedule 4x" in the
counters means the offered load is more than the tape can generate).

### What the sink counts

* `rx` frames received, `avg` rate between its first and last frame, `now` rate
  over the last panel refresh.
* `lost`: sequence gaps. A late frame that arrives after its gap was counted
  cancels it, so reordering shows as `reord`, not as loss.
* `dup`: the same sequence number twice. `reord`: arrived after a higher one.
* Latency: minimum, average, maximum, and the RFC 3550 jitter estimate. The
  histogram (24 buckets from 50 µs to 5 s) gives the median, 95th and 99th
  percentiles, as bucket bounds.

## Routable (UDP/IP) traffic

Raw frames cannot cross a router. Give the generator addresses and it sends
Ethernet/IPv4/UDP datagrams (TTL 64, valid IP and UDP checksums, don't-fragment):

| Setting | Meaning |
| --- | --- |
| `gen.ip=A.B.C.D` | The generator's own address (source of the datagrams; it answers ARP for it). |
| `gen.dst=A.B.C.D` | Where the datagrams go. |
| `gen.gw=A.B.C.D` | Next hop when the destination is not on the generator's subnet. Default: the destination itself. |
| `gen.port=N` | UDP destination port (default 9000; the source port is 9001). |
| `sink.ip=A.B.C.D` | The sink accepts UDP to this address and **answers ARP for it**. |
| `sink.port=N` | The sink's UDP port. |

The generator ARPs for its next hop twice a second until it gets an answer and
sends nothing before that (`gen show` says "ARP pending"). The sink's ARP answers
are what let a router find it, so a sink behind a router must have `sink.ip` set.
Frames that are not for the sink's address and port are ignored.

Example, testing a router with its two sides on NIC 2 and NIC 5:

```
tape run nettest gen=2 sink=5 gen.ip=10.0.1.2 gen.dst=10.0.2.2 gen.gw=10.0.1.1 sink.ip=10.0.2.2 gen.rate=20000 gen.run=on gen.time=10
```

That is one-way; `routertest` is this plus `bidir=on` with the reverse stream
routed back through `10.0.2.1`.

## Rate sweeps

To find where a link, limiter or router stops keeping up, sweep the offered rate:

```
tape run nettest selftest 2 3 4 5 gen.size=1500 gen.sweep=100000:400000:100000:3 rate=250000 queue=300
sweep 200000 kbit: sent 49999 rx 49998 loss 0.00% got 199991 kbit lat 162/8766 us
sweep 300000 kbit: sent 74999 rx 62316 loss 16.91% got 249264 kbit lat 14379/14682 us
sweep 400000 kbit: sent 100000 rx 62500 loss 37.50% got 250000 kbit lat 14431/25110 us
```

(That is a 250 Mbit/s limiter on the Skylake's I350 ports: the delivered rate stops
at the limit, the loss climbs, and latency jumps to the queue's depth once it fills.)

`gen.sweep=START:END:STEP:SECONDS` (kbit/s, seconds) at launch, or `gen sweep START END
STEP SECONDS` at the prompt. One line per step as it finishes: offered rate,
frames sent and received in that step, loss, delivered rate, average and maximum
one-way latency. The generator stops after the last step. The loss figure
compares frames sent and received within the step, so frames still in flight at a
step boundary count against it slightly; use steps of a few seconds.

## Both directions at once

`bidir=on` (or `gen bidir on`) makes the sink's NIC also generate traffic to the
generator's NIC, which also receives it, with the same rate, size and limits as the
forward stream, so one pair of ports loads both directions. In UDP mode the
reverse stream goes from `sink.ip` to `gen.ip`; if it must cross a router add
`rev.gw=ADDR` (the router's address on the sink's side). `stats`, `summary`, the
panel and the saved report show a `rev` section. A symmetric limiter
(`rate=2000`, no direction given) therefore caps each way separately: at 3000
offered each way the sink measured 2001 kbit/s forward and 2001 reverse.

## Results: summaries, histograms and report files

* When a timed or counted test finishes the tape waits one second for the last
  frames and prints a summary by itself:

  ```
  summary: sent 125000, got 94538 (150983 kbit/s over 5.0 s), loss 24.36%
    latency us: median <15000, 95% <15000, 99% <15000, max 17174, jitter 19
  ```

  (200 Mbit/s offered into a 150 Mbit/s limiter with a 200-frame queue and a
  2 ± 1 ms delay.)
* `summary` prints it again at any time; `stats` prints all counters; `sink hist`
  draws the latency histogram as bars (non-empty buckets only).
* `save NAME` writes the settings, summary, counters and histograms to a FOSFS
  file (up to 3 KB; the name is up to 16 of `a-z 0-9 . _ -`). `gen.save=NAME`
  does it automatically at the end of every test. Read it back with `cat NAME`.

## Recipes

**Check a rate limiter against an independent counter.** The sink should read what
the limiter allows.

```
tape run nettest selftest 2 3 4 5 gen.rate=100000 rate=50000 queue=100 gen.time=6
→ sink measured 50001 kbit/s; 49.86% dropped; average latency 15.9 ms (the queue: 100 × 1000 B at 50 Mbit/s)
```

**Does the link survive loss and delay?** Fixed seed for a repeatable run:

```
tape run nettest selftest 2 3 4 5 gen.rate=20000 delay=20:5 loss=2 seed=7 gen.time=6
→ 2.02% lost, latency 17.3–30.5 ms
```

**Where does my router stop forwarding?** Sweep through it:

```
tape run nettest gen=2 sink=5 gen.ip=10.0.1.2 gen.dst=10.0.2.2 gen.gw=10.0.1.1 sink.ip=10.0.2.2 gen.size=1000 gen.sweep=10000:100000:10000:4
```

**How does a TCP client cope with a bad WiFi link?** Put the `nettest` machine
between the client and the server on two of its ports and run `preset=badwifi`;
change it live with `loss`, `delay`, `rate` at the prompt. (There is no generator in
this case; real traffic is the load.)

**Test a cable or a NIC pair.** No link, just a generator and a sink on the two
ends:

```
tape run nettest gen=2 sink=3 gen.rate=1000000 gen.size=1500 gen.time=6 gen.run=on
→ on the Skylake machine: 491,536 of 491,536 frames, 982,003 kbit/s, 0 lost
```

**Unattended run with a report.** A config file `lab.cfg`:

```
# lab.cfg
selftest 2 3 4 5
preset ab wifi
gen.rate=30000
gen.time=60
gen.save=wifi-30m
```

`tape run nettest @lab.cfg`, come back later, `cat wifi-30m`.

**Look at the result as it happens:** add `view=on` to any of them.

## Performance and limits

* The tape polls: while the generator runs, or frames are queued, it never
  sleeps (100% CPU on that machine). With nothing to do it sleeps until the next
  interrupt like every other tape.
* **Speed.** With an invariant TSC the loop keeps up with gigabit line rate; the
  Skylake machine passed 982 Mbit/s of 1500-byte frames (82,000 frames a second)
  through generator, impairment link and sink with no loss. Without one (an old
  CPU, or QEMU's default model) the microsecond clock reads the PIT and the loop
  tops out around 40,000 frames a second, which the generator reports as "fell
  behind schedule". Queueing delay near line rate (about a millisecond) is the
  receive rings filling, not the link.
* **Buffers.** Receive rings hold 128 frames (64 on the RTL8168 and 3C905); a
  burst beyond that is dropped by the NIC and shows up as `receive overrun
  event(s)` in `nic list`.
* The impairment queue storage is 4 MiB from the big pool; on a machine with
  128 MiB of RAM or less the pool is empty and a static 256-frame queue per
  direction is used.
* One stream per direction. Layer 2 and IPv4/UDP only: no TCP, no IPv6, no
  ICMP, no fragmentation, no VLAN tags added.
* The management session should not be on a test NIC. If it has to be, keep its
  traffic light: a session NIC also feeds the tape's keyboard path.
* The QEMU `e1000` model delivers nothing for about a second after promiscuous
  mode is enabled (the `e1000e` model does not): that is why `gen.delay` exists.

## Troubleshooting

| Symptom | Likely cause |
| --- | --- |
| `sink` shows `rx 0` | Wrong NIC numbers or cabling: run `nic map`. The generator's destination MAC must be the sink's NIC (default when `sink=N` is on the same tape). In UDP mode `sink.ip` must equal `gen.dst`. |
| `gen show` says "ARP pending" | The next hop does not answer ARP: wrong `gen.gw`/`gen.dst`, a router that is not at that address, or no `sink.ip`. |
| `nettest: each role needs a NIC of its own` | A NIC number is used twice (a link port as generator, for instance). |
| Large loss at line rate with `failed 0` | The sink's receive ring overran (look for `receive overrun event(s)` in `nic list`), or the tape fell behind (`fell behind schedule`). Lower `gen.rate` or use frames of 1000 bytes or more. |
| Latency jumps to ~1 s at the start (QEMU) | The QEMU `e1000` promiscuous quirk: raise `gen.delay`. |
| Nothing at all through the link | Both ports promiscuous but one cable down: `nic map` shows `link down`. |
| "too many words on one line" | More than 24 words: split the line with `;` or a newline. |
| The prompt is slow to respond | Console output over a slow link; use `view` sparingly on serial or 100 Mbit/s sessions. |

## Testing the tool itself

| Command | What it checks |
| --- | --- |
| `cargo test --manifest-path tapes/nettest/Cargo.toml --lib` | The engine's unit tests: faults, queueing, generator pacing, sink bookkeeping, UDP/IP framing and checksums, sweeps, histograms. |
| `make test-tape-nettest` | Those, then a QEMU guest running the link between two e1000 NICs (loss, bursts, delay, rate, queue, dup, reorder, corrupt, presets, the panel), a burst into each emulated NIC family, and the four-NIC self-test below. |
| `make test-tape-nettest-router` | Two QEMU guests: the generator and sink in one, the `router` tape in the other; UDP/IP in both directions through it. |
| `python3 tapes/test_nettest_selftest_e2e.py` | The four-NIC loop in QEMU: the sink's measurements against the link's settings (clean, limited, lossy, delayed, UDP, sweep, bidirectional, the `selftest` keyword, a report file). |
| `make test-nic-map` | `nic map` against virtual cables. |

The same four-port loop was run on real I350 hardware (the Skylake machine,
cabled NIC 2↔3 and NIC 4↔5); the numbers in the recipes above are from that run.
