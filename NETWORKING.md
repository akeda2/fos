# fos networking

This document describes the boundary between the protected-mode kernel and
networking tapes, and the shared protocol code in `crates/tape-net/`. It is
intentionally more detailed than the command table in `README.md`: networking
is a teaching path through several layers of hardware and protocol state, not
a general socket implementation.

## Design boundary

The kernel owns all supported Ethernet devices. It initializes every
Realtek RTL8139, 3Com 3C905/B/C-compatible controller, and Intel e1000
function it can bring up, selecting the first initialized interface as the
default for backward-compatible callbacks and shell commands. A tape never
performs PCI discovery, allocates DMA memory, programs receive-ring or
descriptor registers, or repairs a device-specific receive failure. The
kernel exposes only the small services needed by a protocol implementation:

```rust
nic_mac(out_ptr: u32) -> u32
nic_send(frame_ptr: u32, length: u32) -> u32
nic_receive(out_ptr: u32, capacity: u32) -> u32
nic_max_frame_len() -> u32
ticks() -> u64
nic_count() -> u32
nic_mac_at(index: u32, out_ptr: u32) -> u32
nic_send_at(index: u32, frame_ptr: u32, length: u32) -> u32
nic_receive_at(index: u32, out_ptr: u32, capacity: u32) -> u32
nic_set_promiscuous_at(index: u32, enabled: u32) -> u32
terminal_position() -> u32
fs_file_length(name_ptr: u32, name_len: u32, out_len_ptr: u32) -> u32
fs_read(name_ptr: u32, name_len: u32, out_ptr: u32, capacity: u32, out_len_ptr: u32) -> u32
fs_write(name_ptr: u32, name_len: u32, data_ptr: u32, length: u32) -> u32
fs_remove(name_ptr: u32, name_len: u32) -> u32
```

These are trailing additions to the existing cdecl tape ABI. The complete
current order is:

1. `putc`
2. `should_abort`
3. `alloc`
4. `free`
5. `read_key`
6. `args_ptr`
7. `args_len`
8. `nic_mac`
9. `nic_send`
10. `nic_receive`
11. `nic_max_frame_len`
12. `ticks`
13. `nic_count`
14. `nic_mac_at`
15. `nic_send_at`
16. `nic_receive_at`
17. `nic_set_promiscuous_at`
18. `terminal_position`
19. `fs_file_length`
20. `fs_read`
21. `fs_write`
22. `fs_remove`

The append-only rule is important. Existing tapes that declare only a leading
prefix remain binary-compatible. A tape that wants slot 22 must still declare
slots 1 through 21, even if it does not use them; parameters cannot be skipped
in the middle because that would shift later cdecl stack positions.

Slots 23 through 35 are the audio callbacks (see `AUDIO.md`). Slot 36,
`wait_for_event() -> ()`, puts the CPU to sleep until the next interrupt: call
it after a polling pass that found no frame, key, or timer work, instead of
spinning at 100% CPU. Every NIC's receive interrupt, the keyboard, and the
100 Hz timer tick all end the sleep, so it costs a received frame no extra
latency, and it may also return early, so simply poll again. It returns at
once (no sleep) if any NIC lacks a working receive interrupt. Most
networking tapes call it (through `tape_rt::set_wait_hook`/`tape_rt::idle`);
sleep only after a pass that found no frame *and* no key. A tape that never
calls it behaves exactly as before.

`nic_mac`/`nic_send`/`nic_receive` operate on the currently selected
interface. `nic_count` reports how many interfaces initialized successfully;
the `*_at` callbacks use a zero-based interface index and let a tape inspect,
send through, or receive from a specific interface concurrently. Existing
tapes remain on the selected-interface callbacks, while a tape that needs
several links can enumerate them and keep separate protocol state per index.
`nic_set_promiscuous_at` enables or disables receive-all filtering on one
interface, which is required by a layer-2 bridge that must receive frames
addressed to hosts behind another port.

`terminal_position` returns the kernel's packed, 1-based ANSI cursor position
(row in the high 16 bits and column in the low 16 bits). It lets a tape answer
terminal cursor-position requests without reaching into the console's private
state. The four filesystem callbacks operate on complete lowercase FOSFS names
and contiguous multi-sector files: `fs_file_length` reports a file's byte length,
`fs_read` copies file bytes into a caller-owned buffer, `fs_write` replaces or
creates a file, and `fs_remove` deletes it. They are used by tapes such as
`tapes/ed/` and by the router's optional DHCP lease persistence.

`nic_mac` copies the six-byte hardware address into tape-owned memory.
`nic_send` accepts one complete Ethernet II frame, including the 14-byte
header, and returns non-zero only when the selected backend reports a
successful transmission. Frames shorter than Ethernet's 60-byte wire minimum
are padded by the driver; the four-byte FCS is added by hardware and is not
part of the callback length.

`nic_receive` and `nic_receive_at` are non-blocking. They return one complete frame without its FCS,
or zero when no frame is ready or the operation cannot proceed. The kernel
keeps a four-frame FIFO for packets temporarily separated from `term net`
input. If a tape polls with a buffer smaller than the received frame, the
frame remains queued for a later retry instead of being consumed silently.
Tapes should call `nic_max_frame_len()` once and use that value for their
receive buffer.

`ticks` is the kernel's monotonic PIT counter. It is the only clock a
networking tape should use for retransmission, ARP expiry, and TCP deadlines.
It is a tick count, not wall-clock time; the current kernel programs the PIT
at 100 Hz. A protocol library receives the callback explicitly so unit tests
can replace it with a deterministic fake clock.

The callbacks are a convention, not a protection boundary. In the default
image, tapes run in ring 0 with only the kernel's supervisor identity map,
not a private address space, and can still touch hardware directly. The
optional `make image-unsafe-memory` teaching image disables that map entirely;
its networking callback ABI is unchanged, but it has even less accidental
pointer-fault coverage.

In the default image, the e1000 driver's memory BAR is not permanently
reachable merely because it occupies a conventional PCI address. The driver
registers the small register range it actually uses through
`paging::map_mmio` before its first volatile access; unsupported or excess
high BAR ranges fail NIC initialization instead of becoming silently mapped.
The convention keeps ordinary networking examples focused on protocol logic
instead of duplicating either NIC driver.

## Layer-2 bridge tape

`tapes/bridge/` is the first consumer of the per-interface callbacks. It
accepts one-based interface numbers in the user-facing `tape run` command
(matching `nic list`), or bridges every initialized interface when no numbers
are supplied. Internally the tape converts them to the callbacks' zero-based
indices, enables promiscuous mode on the selected ports, and restores normal
filtering before returning.

The bridge learns each unicast source MAC in a fixed-size table. Known
destinations are sent only to their learned egress port; unknown, broadcast,
and multicast destinations are flooded to every selected port except ingress.
Frames are otherwise forwarded byte-for-byte. There is no ARP, IPv4 routing,
TTL/checksum handling, VLAN support, STP, or loop prevention. Do not connect
the selected ports in a physical Ethernet loop. `make test-tape-bridge`
exercises both directions across two isolated QEMU e1000 segments.

## IPv4 router tape

`tapes/router/` is a fixed-buffer layer-3 consumer of the per-interface
callbacks. It reuses the bridge's selected-port polling shape, but each
configured port is assigned an IPv4 address and netmask:

```text
tape run router 1 10.0.1.1 255.255.255.0 2 10.0.2.1 255.255.255.0
```

The interface number is one-based, matching `nic list`; the optional leading
`router` word is accepted for readability. Compact CIDR notation is also
accepted, so the equivalent shorter form is
`tape run router 1 10.0.1.1/24 2 10.0.2.1/24`; the separate
`1 10.0.1.1 /24` form is accepted too. Dotted netmasks remain supported.
The tape answers ARP requests for
each configured address, learns sender mappings in one bounded ARP cache per
interface, chooses a directly connected egress with longest-prefix matching,
decrements IPv4 TTL, recomputes the IPv4 header checksum, and emits the
packet with the egress interface's MAC address. ARP resolution polls all
configured interfaces with a bounded timeout so the router can keep answering
requests on either side while resolving a next hop.

The optional `default NIC GATEWAY` suffix selects a one-based egress NIC and
the next-hop IPv4 address for destinations that do not match a directly
connected subnet:

```text
tape run router 1 10.0.1.1 255.255.255.0 2 10.0.2.1 255.255.255.0 default 1 10.0.1.254
```

The optional `dhcp` word remains the backward-compatible spelling that
reserves NIC 1 (callback index zero) for the `tape-net` DHCP client. `dhcp N`
adds one-based NIC `N` as another DHCP uplink, in priority order; static
triples configure the remaining downstream interfaces:

```text
tape run router dhcp 2 10.0.2.1 255.255.255.0
```

An optional `health A.B.C.D` supplies one ICMP echo target shared by DHCP
uplinks. A usable uplink must answer both a fresh gateway ARP probe and that
echo request. Probes are bounded and periodic; after three missed cycles the
router chooses the next healthy declared uplink, and automatically fails back
when a higher-priority uplink recovers. NAT and static inbound forwards use
only that active uplink. Existing NAT and forwarded flows can break when it
changes; there is deliberately no state migration.
When DHCP uplinks receive leases on the same upstream subnet, only the active
one participates in directly connected routing for that subnet. This prevents
the original, unhealthy uplink from winning a longest-prefix route lookup
after failover; static downstream routes remain directly connected.

The router prints event-only diagnostics by default: upstream leases, new
downstream DHCP offers/bindings/releases, and new TCP/UDP/ICMP-echo NAT mappings. It
does not print ordinary forwarded packets or retransmissions. Add `quiet` to
the router startup arguments to suppress the routine DHCP/NAT lifecycle
messages on an unattended deployment; configuration errors and critical
failover/no-uplink warnings remain visible.

NAT is enabled by default. Add `no-nat` when the upstream network has an
explicit route back to the downstream subnets and should receive the original
source addresses; `nat` may be written explicitly to document or re-enable
masquerading.

Permanent inbound services use a bounded static port-forward list. Put each
rule before the interface entries, using:

```text
forward tcp EXTERNAL_PORT INTERNAL_IP [INTERNAL_PORT]
forward udp EXTERNAL_PORT INTERNAL_IP [INTERNAL_PORT]
```

The active uplink (NIC 1 for bare `dhcp`, otherwise the `dhcp N` or `default N`
NIC) is the external side. A rule DNATs packets addressed to its
public address and named TCP/UDP port to the configured host, and rewrites
that host's matching replies back to the uplink's address and external port.
Omitting `INTERNAL_PORT` preserves the external port. The router accepts at
most 16 rules, rejects duplicate protocol/external-port pairs and ambiguous
duplicate internal endpoints, and requires each target to be a unicast
address on a configured non-uplink subnet (NIC 1 is never accepted as a
target, even when the uplink is another NIC). Rules are printed at startup.
Unsolicited traffic that matches neither a static rule nor an existing
outbound-NAT flow remains dropped.

DHCP requires the same bounded polling discipline as the router: it uses only
the indexed NIC callbacks, checks `should_abort`, retries for a finite timeout,
and reports whether the lease supplied an address, netmask, and router. If no
explicit default suffix is supplied, a DHCP router option becomes the default
route of the DHCP uplink (the active one, when several are declared). At least two interfaces are required, NIC selections must be unique
and exist, and an explicit gateway must be on the selected interface's
subnet. Packets addressed to the router itself are answered with an ordinary
ICMP echo reply when they are an echo request (so pinging a configured
interface address, including the active uplink's, works like pinging any
other host) and otherwise dropped; IPv4 broadcast/multicast, unknown
networks without a default, TTL 1, and malformed/checksum-invalid IPv4
packets are also dropped. When enabled, the router performs TCP/UDP source NAT and ICMP echo identifier
translation using a
4 MiB physical-memory-backed table when traffic leaves a non-primary NIC
through the active uplink, and reverses that mapping for replies. This provides tens of
thousands of flow slots and lets downstream DHCP clients reach hosts on the
upstream LAN without requiring those hosts to learn a route back to the
downstream subnet. If allocation is unavailable, it falls back to 32 slots.
New mappings use a run-varying IANA dynamic source port rather than preserving
the inside source port, so a fixed-port client reconnecting after a router
restart does not immediately collide with a remote server's still-live TCP
tuple. Entries expire after ten minutes; new flows are rejected when the table is
full rather than evicting existing mappings. Other ICMP messages and non-TCP/UDP
traffic are forwarded without translation; a TCP/UDP-labeled segment too short
to contain its own port pair is not this passthrough and is dropped instead of
leaking the downstream host's private source address unNATed. Any packet whose
more-fragments flag or fragment offset is nonzero is dropped rather than
misread as a complete transport header, since fragment reassembly remains out
of scope. Dynamic routing, ICMP
unreachable/time-exceeded errors, fragmentation/reassembly, IPv6, and a kernel
socket layer remain out of scope. `make test-tape-router` runs the
three-party QEMU/multicast test that verifies ARP, forwarding in both
directions, and TTL/checksum changes.

The router can also provide a small DHCP server with the optional
`dhcp-server` word:

```text
tape run router dhcp-server 1 10.0.1.1/24 2 10.0.2.1/24
```

Every configured interface except a DHCP uplink serves clients. Each pool is derived
from that interface's subnet and excludes the network address, broadcast
address, and router address. The router allocates one in-memory lease slot for
every usable address in the subnet, using the tape allocator rather than a
small fixed lease limit. A configuration can therefore use the complete
address range of an ordinary subnet; extremely large subnets can still be
refused if the machine cannot provide the required lease-table memory.
Leases last seven days, with renewal at 3.5 days and rebinding at 6.125 days,
and the server handles the normal DISCOVER/OFFER and REQUEST/ACK exchange
plus DECLINE and RELEASE. Replies include subnet mask, router, broadcast
address, server identifier, lease time, and renewal timers.

If an uplink is configured with `dhcp`, DNS server addresses from the upstream
lease are copied into downstream DHCP option 6. A `dns SERVER [SERVER]`
directive can override those values when the upstream interface is static or
when a specific external resolver should be advertised:

```text
dhcp
dhcp-server
dns 192.168.1.53 1.1.1.1
2 10.0.2.1/24
```

The router does not run DNS. Lease persistence is optional: add
`dhcp-lease-file NAME` alongside `dhcp-server` to save bound MAC-to-address
assignments in FOSFS. Omitting it keeps the router diskless; an unavailable
disk also falls back to memory-only operation. Persisted assignments restart
their seven-day lease timer at the next boot because tapes have no
battery-backed wall-clock callback. A /31 or /32 interface has no usable pool
and is skipped. This is deliberately a DHCP service for the router's directly
connected LANs, not a general DHCP relay or network-management daemon.

For repeatable setups, router arguments may be stored in a lowercase FOSFS
file and supplied with `@FILE`:

```text
# router.cfg
dhcp-server
dhcp-lease-file DHCP.LEASES
forward tcp 8080 10.0.15.2 80
1 10.0.1.1/24
2 10.0.2.1/24
```

Run it with `tape run router @router.cfg`. The shell reads the file through
the kernel's FOSFS/BIOS disk path and passes its bytes through the existing
tape startup-argument ABI. The router's optional lease persistence uses the
high-level FOSFS tape callbacks rather than accessing raw sectors or BIOS.
Router configuration accepts newlines and `#` comments, and the file is
limited to 4096 bytes by the shell's fixed configuration buffer. If
`ROUTER.CFG` lives on an attached secondary disk instead of the boot disk,
name it explicitly: `tape run router @disk2/router.cfg`; a bare `@NAME`
always means the boot disk.

## Kernel Ethernet backends

`kernel/src/nic.rs` is the common facade used by the shell and all tape
callbacks. It preserves one hardware-independent contract while keeping
device-specific register programming in separate modules. `nic list` shows
every initialized interface and `nic use N` changes the active one (the shell
uses one-based numbering; tape callbacks use zero-based indices). The active interface
is what `nic sniff` defaults to and what the single-interface calls of `net-chat`,
`ip-chat` and `telnet` use (the tape ABI's `nic_active`, version 4); tapes that
choose interfaces themselves (`router`, `bridge`, `nettest`) pass an index:

- `kernel/src/rtl8139.rs` supports the QEMU-default RTL8139 with its
  polling receive ring and overflow recovery.
- `kernel/src/threecom.rs` supports selected PCI 3C905/B/C identifiers using
  the card's windowed command interface and bus-master descriptor lists.
- `kernel/src/e1000.rs` supports QEMU's Intel 82540EM model through a
  memory BAR and legacy descriptor rings.

All three backends retain foreground polling for frame parsing. When firmware
assigns a usable legacy PCI IRQ, the kernel first installs its IDT gate and
unmasks that PIC line, then enables only the backend's acknowledgement path.
The ISR acknowledges/counts its hardware cause and marks work pending; a
normal `nic_receive` poll still reads descriptors and copies frames. This
keeps router/bridge protocol code in foreground context, preserves operation
on cards without a legacy line, and makes `nic`/`irqstat` diagnostics useful
without introducing a background network service.

### Foreground NIC watchdog

`nic` status, `nic watchdog`, and normal shell NIC traffic run a bounded
foreground health check; no timer or device ISR performs recovery. Drivers
record foreground Rx/Tx completions, pending recovery, successful recoveries,
and failed recoveries. Silence is never considered a stall. Recovery is only
armed by a documented hardware condition: RTL8139 Rx/FIFO overflow or
transmit timeout, e1000 Rx overrun or transmit timeout, and 3C905 Host Error
or download timeout. Each backend resets and rebuilds its already allocated
DMA/ring state, restores its interrupt mask and promiscuous setting, and
reports the result through `nic`; it neither allocates nor frees DMA frames.
If that bounded reset fails, polling remains available and the failure counter
is visible rather than claiming that a link or cable was repaired.
`make test-nic-watchdog` builds a test-only RTL8139 image that starts with
this recovery latch set, then verifies in QEMU that foreground status performs
the in-place reset and reports one successful recovery.

The 3C905 path is intentionally preliminary and has not been validated by
the repository's QEMU tests: the installed QEMU build has no 3C905 model.
Physical validation should begin with `nic`, then `nic send`/`nic recv` or
`term net`, and finally a protocol tape such as `tapes/ip-chat` or
`tapes/telnet`. A failed 3C905 initialization is reported separately from
the absence of any supported controller.

When diagnosing DHCP on a physical multi-NIC machine, run
`tape load disk net-dhcp.bin` followed by `tape run` before changing the
router or DHCP implementation. The tape tries every initialized interface
independently, reports transmit/receive/parser/DHCP counters, and does not
advance until that NIC binds or the operator presses an escape function key.
Each successful lease also reports the DNS server addresses carried in DHCP
option 6, or explicitly reports that no DNS server was advertised. This is a
diagnostic only: the tape does not perform DNS queries or install a resolver.
After all interfaces bind, it remains in a periodic status display until the
operator presses an escape function key.
Pass `tape run HOST [INTERVAL_SECONDS]` to add ICMP echo checks after binding;
the tape ARPs the host directly when it is on the leased subnet and uses the
DHCP gateway otherwise. With no startup arguments, it performs no ping checks.
If a replacement NIC acquires a lease immediately on the same cable and
network segment while the original one remains silent, that isolates a flaky
NIC or its physical path; do not treat it as evidence that DHCP parsing,
router policy, or the other interface drivers need changing.

The 3C905 transmit descriptor format differs between the original 905
Boomerang cards and the 905B/C Cyclone/Tornado revisions. The driver selects
the appropriate frame-start header from the PCI device ID; B/C cards require
the transmit-indication and roundup-defeat flags, with the payload length
stored only in the final-fragment field. A single transmit descriptor is used
by this polling driver, so a busy or non-completing card drops a frame rather
than blocking a bridge indefinitely.

Initialization also selects the card's EEPROM/connector media mode in window
3, normally choosing 10/100BASE-T autonegotiation on 905B/C TXM cards, and
uses the network-only global reset so the PHY is not unnecessarily reset out
from under autonegotiation. `nic` reports the 3Com media link-beat bit as a
basic physical-link diagnostic; a down result means discovery cannot work yet
and should be debugged as a cable, switch/peer, PHY, or media-selection issue
before investigating DMA descriptors.

3Com 3C509 cards are not covered by `threecom.rs`. They are ISA devices with
a different EtherLink III FIFO/windowed protocol and need a separate
`threec509` backend with ISA Plug-and-Play or legacy-port detection.

The Intel backend is intentionally scoped to the 82540EM/e1000 model first.
It uses 32-bit memory BARs, legacy 16-byte descriptors, and polling; it does
not yet cover e1000e/82574L, MSI/MSI-X, checksum offload, or advanced
descriptors. `make test` runs the ordinary no-NIC smoke path and a second QEMU smoke path
with `-device e1000`, exercising initialization and transmission without
changing existing tape behavior. The appended per-interface callbacks are
covered by the same network-tape ABI boundary and do not change the first
interface selected by older tapes.

## Receive ownership with `term net`

The kernel is single-threaded. While a tape is running inside a `term net`
session, the outer terminal loop is paused, so the tape callbacks must poll
the selected NIC backend directly. The active interface is the one associated
with that terminal session; `nic_receive_at` on another interface bypasses
terminal-frame classification and exposes that interface's raw frames
directly to the tape.

The kernel classifies frames from the connected terminal peer:

- `TERM_TAG_DATA` bytes and terminal disconnect/abort frames become
  `read_key` events.
- Discovery probes remain raw frames so a networking tape can own its peer
  table and reply behavior.
- Other Ethernet frames remain available through `nic_receive`.

This prevents a terminal command from stealing an ARP reply or TCP ACK from a
tape. The four-frame deferred FIFO is deliberately small: it reduces
accidental loss between the two consumers but does not pretend to be a
general interrupt-driven network queue. The RTL8139 remains polling-only and
can still overflow when a sender transmits an unpaced burst; use the existing
3 ms pacing convention for large tape transfers.

## `crates/tape-net`

`crates/tape-net` is a `#![no_std]` library, not a tape binary. It contains
protocol building blocks that can be linked into multiple tapes:

- Ethernet II header parsing and frame construction.
- ARP request/reply parsing, construction, and a fixed-size cache.
- IPv4 header parsing/building and Internet checksum calculation.
- ICMP echo request/reply packets.
- UDP datagram parsing and construction.
- A bounded single-connection TCP state machine.

The library has no heap, background task, interrupt handler, or hidden global
device. A tape supplies the raw-frame send/receive callbacks and a tick
source, then explicitly polls the library. Buffers are caller-owned fixed
arrays. IP configuration is also explicit: the tape supplies its own local
IPv4 address, netmask, gateway, and remote endpoints rather than relying on
an undocumented kernel default.

The protocol layers are intentionally separable. Ethernet and ARP can be
tested without IPv4; IPv4 and ICMP can be tested with synthetic frames; UDP
does not depend on TCP; and TCP's state machine can be driven by a fake
device and fake tick counter in unit tests.

The crate's main public building blocks are:

```text
EthernetFrame::parse / encode
ArpPacket::request / reply / parse / encode
ArpCache<N>
Ipv4Header::new / parse / encode
Ipv4Packet::parse / encode
IcmpEcho::request / reply / parse / encode
UdpDatagram::new / parse / encode
TcpSegment::parse / encode
TcpConnection<RX>::listen / active / start_connect / on_segment
```

All parsers borrow their input and all encoders write into caller-provided
buffers. `TcpConnection` models one connection and keeps one outstanding
segment for retransmission. The caller supplies the current tick to
`start_connect`, `on_segment`, `send_data`, and `close`, then checks
`retransmission_due` and calls `retransmit` when necessary. This explicit
shape makes the protocol state visible in a tape and makes each transition
straightforward to drive with a fake clock in a unit test.

### ARP and IPv4 scope

ARP is Ethernet/IPv4 only. The cache is bounded and uses replacement rather
than heap allocation. An unresolved IPv4 next hop produces an ARP request;
the caller polls until a reply is received or its own deadline expires.
The library also contains a bounded DHCPv4 client. A consuming tape chooses
whether to use it; the kernel does not store or apply an IP configuration.

IPv4 supports Ethernet II frames, ordinary unfragmented packets, and protocol
dispatch by the header's protocol byte. Fragment reassembly, options, IP
forwarding, and routing tables are intentionally out of scope.

ICMP currently provides the teaching-friendly echo request/reply path. It is
useful as the first end-to-end protocol because it exercises ARP, IPv4,
checksums, receive filtering, and a timeout without introducing ports or a
connection state machine.

### UDP scope

UDP is a datagram layer over IPv4. The library builds and parses source/dest
ports, length, and checksum fields, but does not provide sockets or a
background listener. A tape chooses when to poll, where to place payload
bytes, and what to do when its fixed receive buffer is full.

### RTP scope

`RtpPacket` implements the fixed 12-byte RTP header (RFC 3550) as a UDP
payload format, the same relationship `IcmpEcho` has to `Ipv4Packet` one
layer up. `parse` is genuinely RFC 3550-tolerant: it correctly skips a
CSRC list and an extension header, and trims trailing padding, rather
than rejecting any packet that uses them. `encode` only ever emits the
simple common case (no CSRC, no extension, no padding), since that is
the only form any sender built on this crate needs to produce.

There is no RTCP, no SDP, and no stateful session/jitter-buffer type in
this crate: sequence-number gap/reorder accounting is deliberately left
to the calling tape (see `sequence16_at_or_after`, the 16-bit-width
counterpart to `tick_reached`/`seq_at_or_after`'s wraparound-safe
comparison idiom), exactly like `ArpCache`/`DhcpClient`/`TcpConnection`
own their state but a bare `UdpDatagram` owns none.

Payload type selection is a small, documented, *unnegotiated* convention
rather than SDP: RFC 3551 has exactly one static entry this project's
fixed-format audio hardware can ever match (`L16`/44100 Hz/stereo, PT
`10`); anything else (48 kHz, 24-bit) uses a dynamic-range PT (96+) this
project documents itself, in `AGENTS.md`'s `audio-rtp` section, rather
than negotiating it out of band.

### DHCP scope

DHCPv4 belongs in `tape-net` because its wire format and state machine are
reusable protocol logic, while the consuming tape should decide what to do
with the resulting lease. The client starts with the required bootstrap
addresses (`0.0.0.0` and `255.255.255.255`), sends the standard
`DISCOVER -> OFFER -> REQUEST -> ACK` exchange, and uses the kernel `ticks`
callback for bounded retries and lease deadlines. The implementation has no
heap, background task, or hidden NIC ownership.

A DHCP-using application tape should expose mode selection as structured
arguments rather than treating the whole command line as one opaque flag:

```text
tape run dhcp
tape run static 192.0.2.10 255.255.255.0 192.0.2.1
```

`dhcp` selects negotiated configuration. `static` requires an explicit local
address, netmask, and gateway, making the fallback deterministic and avoiding
an undocumented built-in address. The application tape owns argument parsing,
printing, and whether it proceeds to ICMP/UDP/TCP after configuration;
`tape-net` owns DHCP packet construction, option parsing, retry state, and
renewal/rebind timing.

The implementation is exposed through `DhcpClient<const OPTION_CAP: usize>`.
The application calls `poll(now)` to obtain a pending
`DhcpTransmit`, sends that Ethernet/IPv4/UDP payload, and passes received
payloads to `receive(now, packet)`. `state()`, `lease()`,
`retransmission_due(now)`, and `retransmission_deadline()` let the tape
drive the non-blocking loop without hidden timing or task state. The client
states are `Init`, `Selecting`, `Requesting`, `Bound`, `Renewing`,
`Rebinding`, `Expired`, and `Failed`.

### TCP scope

The TCP implementation is intentionally bounded rather than production
complete. It models one active connection and includes:

- three-way connection establishment;
- sequence and acknowledgement tracking;
- bounded application receive storage;
- one outstanding retransmission deadline at a time;
- retransmission of the last unacknowledged segment;
- orderly FIN exchange;
- reset and timeout failure states.

It does not provide a heap, multiple simultaneous connections, congestion
control, selective acknowledgements, urgent data, IP fragmentation, or a
general socket API. The implementation is intended to make the state machine
readable and testable, not to compete with a host operating system's TCP.
Applications must choose a conservative segment size that fits the raw
Ethernet callback and must poll often enough to avoid overflowing the NIC
ring.

## `tapes/telnet`

`tapes/telnet/` is the first TCP application built on the library. It is a
single-connection terminal client for generic Unix-like Telnet servers. Static
configuration uses:

```text
tape run static LOCAL_IP/PREFIX GATEWAY SERVER_IP SERVER_PORT
```

Dotted netmasks remain accepted for compatibility, as does the separate
`LOCAL_IP /PREFIX` form.

DHCP can supply the local IPv4 configuration while the server remains
explicit:

```text
tape run dhcp SERVER_IP SERVER_PORT
```

The tape resolves the server or gateway next hop with ARP, drives the shared
TCP connection state machine, and polls the kernel-managed NIC callbacks until
the session ends. In DHCP mode it drives the bounded
DISCOVER/OFFER/REQUEST/ACK exchange from `tape-net` before resolving the
Telnet server. Its application buffer is fixed and caller-owned; there is no
heap, DNS, or multiple connection support. It advertises an 80x25 `xterm`
terminal and sends NAWS window-size information.
The server port is explicit so the same tape can connect to the conventional
port 23 or to a non-privileged test service.

The Telnet parser is incremental because TCP can split any Telnet command
across segments. It handles `IAC IAC` literal bytes,
`WILL/WONT/DO/DONT` negotiation, terminal-type (`xterm`) and NAWS
subnegotiation, and `IAC SB ... IAC SE` across TCP boundaries. BINARY, ECHO,
SUPPRESS-GO-AHEAD, TERMINAL-TYPE, and NAWS are accepted in their appropriate
directions; unknown options are refused. Application bytes containing `0xff`
are doubled before transmission, as required by Telnet framing. All other
input bytes are preserved, including Tab (`0x09`), so shell tab-completion
remains usable through the Telnet tape.

Unicode is deliberately not decoded in the tape. UTF-8 is a byte encoding,
so forwarding the exact bytes preserves server output and local input for an
attached UTF-8 terminal without adding a character decoder, normalization
policy, or terminal-width calculation. A generic Telnet server's login
prompts, credentials, and line endings remain ordinary terminal bytes; this
client does not implement authentication policy or DNS. Common ANSI/VT100 control
sequences are kept intact for serial and `term net` clients. The local VGA
callback now interprets a bounded VT100 subset: cursor movement and
positioning, erase display/line, SGR 16-color attributes plus common
256-color indexes including hidden text, cursor-shape requests, save/restore
cursor, scroll regions, cursor visibility, and the alternate screen buffer.
It answers `CSI 5n` (OK) and `CSI 6n` (1-based cursor position) from the
kernel-owned cursor state. When a Telnet tape returns, the kernel resets both
its VGA parser/hardware state and the attached terminal's common modes, so
programs cannot leave the next `fos>` prompt with stale colors, visibility,
cursor shape, alternate-screen, bracketed-paste, or wrapping state. Local VGA
remains fixed-buffer and reports 80x25 to the server.

The focused `tapes/test_telnet_e2e.py` test uses the existing unprivileged
QEMU `socket,mcast=` Ethernet fixture and a small host-side
DHCP/ARP/IPv4/TCP peer. The peer serves a lease, sends Telnet negotiation plus
a UTF-8 banner, checks the tape's negotiation replies, UTF-8 input, and Tab
forwarding, then exercises abort and remote-shell recovery. This validates
the protocol
boundary without requiring a TAP bridge or modifying the host's network
configuration; interoperability with a real Unix `telnetd` remains a separate
manual test.

## `tapes/audio-rtp`

`tapes/audio-rtp/` streams real-time raw PCM audio over genuine RTP
(RFC 3550) carried in UDP/IPv4 -- the standards-based counterpart to
`tapes/audio-net/`'s bespoke raw-Ethernet protocol. Any real RTP tool
(`ffmpeg`, `gstreamer`, ...) can send to or receive from this tape
directly; nothing about the wire format is fos-specific except the
payload-type convention described in "RTP scope" above. `listen` mode
accepts one fixed Opus payload type, decoded through `crates/opus-codec`'s
vendored fixed-point libopus decoder; `send`/`capture` can now *encode*
Opus too (`send opus`/`capture opus`, see the startup forms below). Both
directions rely on the same vendored decoder/encoder; see `AGENTS.md`'s
"Real-time audio streaming over RTP/UDP" section for the full design
(vendoring rationale, the allocator override, why both directions are
HDA-only, and a real stack-overflow bug the encoder's own internal SILK
scratch allocation caused and how it was fixed). Opus FEC/DTX and any
SDP/RTCP negotiation remain explicitly out of scope.

Startup forms put network configuration first, mirroring `tapes/telnet`'s
own convention rather than `audio-net`'s mode-first shape (which has no
network-config step at all):

```text
tape run static LOCAL_IP/PREFIX GATEWAY [PORT]
tape run static LOCAL_IP/PREFIX GATEWAY send [opus] NAME PEER_IP [PEER_PORT] [BITRATE]
tape run static LOCAL_IP/PREFIX GATEWAY capture [opus] PEER_IP [PEER_PORT] [BITRATE]
tape run dhcp [PORT]
tape run dhcp send [opus] NAME PEER_IP [PEER_PORT] [BITRATE]
tape run dhcp capture [opus] PEER_IP [PEER_PORT] [BITRATE]
```

`PORT`/`PEER_PORT` default to `5004`, a common (if not IANA-registered)
convention several real-world RTP tools already use unprompted. `opus`
selects Opus encode in place of raw PCM; `BITRATE` (bits/sec) is only
meaningful alongside `opus` and defaults to `opus_codec::DEFAULT_BITRATE_BPS`
(96 kbps) when omitted. Complexity is fixed at libopus's maximum and the
application at `OPUS_APPLICATION_AUDIO`, neither exposed as a choice --
see `AGENTS.md` for why.

**Format is adaptive, not fixed.** The tape queries the local device's
actual format (`audio_sample_rate_at`/`audio_channels_and_bits_at`,
device 0 only -- see the scope note below) and picks the matching RTP
payload type from a small table:

| Rate | Bits | Channels | Payload type |
| --- | --- | --- | --- |
| 44100 Hz | 16 | 2 | `10` (RFC 3551 static `L16`/44100/stereo) |
| 48000 Hz | 16 | 2 | `96` (dynamic range; documented convention) |
| 48000 Hz | 24 | 2 | `97` (dynamic range; `L24` per RFC 3190's naming) |

Any other combination is rejected outright (a clear error, not a
resampling/requantization attempt) -- the same "don't build untestable
generality" scope choice this project makes everywhere else. `listen`
mode additionally recognizes a fourth, fixed payload type, `98`
(`PAYLOAD_TYPE_OPUS`), independent of this table -- it is not a format a
local device natively produces, it is a wire encoding decoded to
whatever PCM format the local device actually needs; `send opus`/
`capture opus` send with this same payload type in the other direction.
Opus is only available when the local device is 16-bit at one of
libopus's five accepted rates (8/12/16/24/48 kHz), in either direction;
see `AGENTS.md` for why that makes it HDA-only today.

**Listen mode** locks onto the first sender's SSRC ("first sender wins,"
matching `audio-net`/`tape_load_net`'s own convention) and tracks its
sequence number: a gap resyncs forward and counts as loss (no
reorder/jitter buffer beyond the kernel's own `PcmRing` -- matching
`audio-net`'s accepted best-effort philosophy), a late/duplicate packet
is dropped and counted separately. Every accepted packet's payload is
byte-swapped from RTP's big-endian wire format to the kernel ABI's own
little-endian format before `audio_submit_at` -- the one real
translation step in an otherwise raw-copy pipeline. For 24-bit audio
specifically, the wire format (`L24`, RFC 3190) is 3 tightly-packed
bytes per sample, while this project's own HDA driver uses a 4-byte
container per sample (see `AGENTS.md`'s HDA notes) -- so 24-bit also
needs a repacking step, not just a byte-swap, and that repacking's exact
container convention is a documented assumption pending real-hardware
confirmation, same as the driver-side choice it mirrors.

**Send mode** replays a raw, headerless PCM file already on FOSFS
(`fs_file_length`/`fs_read`, matching `audio-net`'s own file-format
assumption), resolving the peer's MAC via ARP first. **Capture mode** is
the live-ADC counterpart, streaming `audio_capture_at`'s output in real
time with no extra pacing loop -- the real ADC period rate already paces
it, exactly as `audio-net`'s own capture mode reasons. With `opus`, both
instead encode fixed 20 ms frames through a persistent `opus_codec::
Encoder` before sending: `send`'s own tick-based pacing keys off samples
encoded rather than bytes copied, and `capture` accumulates
`audio_capture_at`'s "whatever's ready" chunks into a small buffer until
a complete 20 ms frame is available (Opus frames are fixed-duration; the
raw path has no such alignment requirement at all). Both answer incoming
ARP requests for their own configured address while running, copied in
shape from `tapes/ip-chat`'s `process_frame`/`send_arp_reply` (also
mirrored by `tapes/router`) -- without this, no external sender could
ever resolve this tape's MAC well enough to deliver it a unicast packet
at all.

**Packetization is capped at 1440 bytes of PCM payload per packet**, not
the 20 ms/~3.5 KB real-world convention audio-over-RTP tools often use:
this project's IPv4 stack has no fragmentation anywhere (`kernel/src/
nic.rs`'s `MAX_FRAME_LEN` is a hard, unfragmentable ceiling), so
`1514 - 14 (eth) - 20 (ip) - 8 (udp) - 12 (rtp) = 1460` bytes is the real
ceiling, ever, per packet -- 1440 is a clean divisor comfortably under
it, not a style preference.

**Scope: device 0 only, deliberately.** One RTP session models exactly
one media stream (one SSRC, one UDP 5-tuple) -- there is no RTP
equivalent to `audio-net`'s single-session, in-band-tagged multi-device
multiplexing, and this shell's one-foreground-tape-at-a-time model rules
out true simultaneous multi-device sessions regardless of protocol. A
second device means invoking this tape again on a second port.

`tapes/test_audio_rtp_e2e.py` hand-builds real, spec-compliant RTP/UDP/
IPv4/Ethernet frames in Python (never importing anything from the
kernel/tape side), proving genuine interop-shaped correctness rather
than fos-talking-to-fos: a real ARP request/reply round trip in both
directions, a deliberate sequence gap and a deliberate stale/duplicate
packet (exercising the loss/reorder accounting), a captured WAV verified
for the *correct pitch* (not just non-silence -- a byte-swap bug would
still produce loud, wrong-pitch audio a peak-amplitude-only check would
wrongly pass) windowed against ES1370's own built-in ~441 Hz test tone
(which the capture also contains until the tape's first submit call
engages network mode), and a send-mode round trip verified byte-for-byte
against the original source file. Capture mode's own transport
correctness is covered by the same code path `send_mode` already proves;
QEMU's silent default ADC content gives it nothing further to verify.
`tapes/test_audio_rtp_opus_e2e.py` is this file's Opus-decode sibling: it
sources its RTP packets from a real `ffmpeg`+`libopus` encode rather than
hand-building them, and runs against an Intel HDA guest (48 kHz/16-bit,
one of libopus's accepted decode rates) instead of ES1370.
`tapes/test_audio_rtp_opus_send_e2e.py` covers the reverse direction
(Opus encode): a known tone seeded into FOSFS, encoded for real by
`send opus`, and decoded back by a small host-side build of this
project's own vendored decoder (compiled fresh at test time) to verify
pitch and duration -- reusing the exact decoder the other test already
proved, rather than depending on a second implementation to cross-check
against. Both Opus tests use `-accel kvm` when available; see
`AGENTS.md` for why.

A manual, non-automated sanity check against a real tool (e.g.
`ffmpeg -re -i tone.wav -ar 44100 -ac 2 -f rtp rtp://<fos-ip>:5004`
against listen mode, or the reverse direction receiving from send/
capture mode) is the natural next step once real hardware is available,
mirroring this project's existing "documented but not automated"
real-interop convention.

## Testing

The library's unit tests use synthetic frames and a deterministic tick source.
They cover:

- byte order and Ethernet/ARP serialization;
- IPv4 and transport checksums;
- ICMP echo request/reply parsing;
- UDP length and payload handling;
- TCP SYN/SYN-ACK/ACK transitions, data acknowledgement, retransmission
  deadlines, FIN, and reset paths.

The existing `tapes/test_net_chat_e2e.py` remains the raw Ethernet/kernel
integration regression. It proves the callbacks work through two emulated
RTL8139 devices and that the shell remains usable after a networking tape
returns. Pass `--nic e1000` (or run `make test-e1000-net`) to run the same
two-guest receive/transmit test through QEMU's Intel 82540EM backend. The
ordinary `make test` smoke suite additionally initializes and transmits
through one emulated e1000 device. It does not exercise the 3C905 backend
because QEMU cannot emulate that card here. A future protocol tape should
add an end-to-end test at the first protocol it demonstrates, preferably
ICMP before UDP and UDP before TCP.

Run the focused checks with:

```sh
cargo test --manifest-path crates/tape-net/Cargo.toml
make test-tape-ip-chat
make test-tape-telnet
make test-tape-audio-rtp
make kernel
python3 tapes/test_net_chat_e2e.py
python3 tapes/test_net_chat_e2e.py --nic e1000
python3 tapes/test_ip_chat_e2e.py
python3 tapes/test_telnet_e2e.py
python3 tapes/test_audio_rtp_e2e.py
python3 tapes/test_audio_rtp_opus_e2e.py
python3 tapes/test_audio_rtp_opus_send_e2e.py
```

The normal `make test` remains required for kernel or boot changes. Real
hardware testing of large transfers should use the documented capability
enabled Python interpreter and `tools/send_tape.py`'s default pacing. Both
current backends are intentionally polling-only and cannot guarantee
lossless wire-speed bursts.

## Deliberate future work

`tapes/ip-chat/` is the first application built on the library. It uses UDP
over IPv4, supports both `static` and `dhcp` configuration, resolves the
next-hop address with ARP, and keeps line-oriented chat policy in the tape.
Its static mode is the deterministic integration-test path; DHCP mode is
intended for a network with an actual DHCP server.

IPv6, routing, congestion control, multiple TCP connections, and a kernel-side
socket layer remain deliberately deferred.
Moving those into the kernel would make the educational protocol code harder
to inspect and would exceed the current project's intentionally small
hardware boundary.

RTCP and SDP-based negotiation remain deliberately deferred for
`tapes/audio-rtp/`. Opus (both decode and encode, plain FEC/DTX aside)
is no longer deferred -- see `AGENTS.md`'s "Real-time audio streaming
over RTP/UDP" section for `crates/opus-codec`'s vendored fixed-point
libopus codec. Two-way duplex (simultaneous `listen`+`send` in one
invocation, multiplexing one incoming and one outgoing RTP stream over
one UDP 5-tuple) is the next planned step, building on the same combined
polling-loop pattern `shell.rs`'s `chat` command already uses to merge
receive-and-print with input-and-echo -- not yet built.

## Router tape detail (moved from AGENTS.md)

This is the original, unedited text of AGENTS.md's `tapes/router/` bullet.

- `tapes/router/` is a fixed-buffer layer-3 tape. It reuses the bridge's
  polling shape, answers ARP per configured interface, and forwards IPv4
  packets between directly connected subnets while updating TTL and header
  checksum. Its syntax is `tape run router [dhcp [NIC]]... [health A.B.C.D] [dhcp-server] [dhcp-lease-file NAME] [nat|no-nat] [forward tcp|udp EXTERNAL_PORT INTERNAL_IP [INTERNAL_PORT]] NIC IP/PREFIX ... [default
  NIC GATEWAY]` (legacy dotted `NIC IP NETMASK` and separate `NIC IP /PREFIX`
  forms remain accepted); NIC numbers are one-based, `dhcp` configures NIC 1
  (callback index zero), while `dhcp N` adds an ordered DHCP uplink. `health`
  requires both fresh gateway ARP and ICMP echo success, fails over after
  three missed bounded polls, and fails back on recovery. NAT and static
  forwards are limited to the active uplink, so existing flows may break on a
  change. Boot-time acquisition for a configured DHCP uplink is bounded but
  non-fatal to the whole router: an uplink that gets no usable offer within
  its own boot budget is left unconfigured and unhealthy rather than
  aborting startup, and its `DhcpClient` keeps retrying in the background
  (the same restart-on-`Expired`/`Failed` machinery that already drives
  ordinary lease renewal) until it does acquire a lease, at which point the
  ordinary failover machinery adopts it automatically. A router is only
  refused startup for an explicit operator abort or a NIC/callback-level
  error, never for one uplink alone failing to hear back from a DHCP
  server -- the entire point of configuring more than one uplink is that
  another one may still work. An explicit or active DHCP-supplied gateway handles unmatched
  destinations. NAT is enabled by default; `no-nat` disables it for
  deployments where upstream routing reaches the downstream subnets, while
  `nat` explicitly enables it. When enabled, TCP/UDP traffic leaving
  non-primary NICs through the active uplink is source-NATed through a 4 MiB
  physical-memory-backed table and reverse-translated for replies, with a
  32-slot fallback if allocation is unavailable, so upstream hosts do not
  need a route back to the downstream subnet. `dhcp-server` serves every
  configured interface
  except DHCP uplinks using an automatically derived subnet pool with one in-memory
  lease slot per usable address, and seven-day leases. Upstream DHCP DNS
  values are propagated to downstream clients; a `dns SERVER [SERVER]`
  directive overrides them. The server has no DNS implementation or
  persistence. It also accepts `tape run router @router.cfg`: the shell reads the lowercase
  FOSFS file and passes its contents through the existing startup-argument
  ABI; `#` comments and newlines are accepted, with a 4096-byte fixed limit.
  `dhcp-lease-file NAME` optionally persists bound DHCP assignments through
  the tape filesystem callbacks; omitting it keeps the router diskless. An
  ICMP echo request addressed directly to one of the router's own configured
  interface addresses (as opposed to a downstream host reached *through* it)
  gets an ordinary echo reply, including on the active uplink -- answered
  before that interface's inbound NAT/port-forward check, which otherwise
  treats any packet addressed to the active uplink's own address as inbound
  NAT traffic needing a matching mapping and would silently drop an
  unsolicited ping to it. A packet that misses the ARP cache is held (one frame, `HELD_FRAME`, built
  with a blank destination MAC and expiring after 3 s) and sent by `process_arp`
  when the reply arrives, instead of being dropped and left to the sender's
  retry -- previously the first ping of every conversation was lost or a second
  late (`make test-tape-router-arp`). The tape deliberately has no dynamic routing,
  ICMP errors, fragmentation, or IPv6.
