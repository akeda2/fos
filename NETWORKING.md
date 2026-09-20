# fos networking

This document describes the boundary between the protected-mode kernel and
networking tapes, and the shared protocol code in `tapes/tape-net/`. It is
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

The append-only rule is important. Existing tapes that declare only a leading
prefix remain binary-compatible. A tape that wants slot 16 must still declare
slots 1 through 15, even if it does not use them.

`nic_mac`/`nic_send`/`nic_receive` operate on the currently selected
interface. `nic_count` reports how many interfaces initialized successfully;
the `*_at` callbacks use a zero-based interface index and let a tape inspect,
send through, or receive from a specific interface concurrently. Existing
tapes remain on the selected-interface callbacks, while a tape that needs
several links can enumerate them and keep separate protocol state per index.
`nic_set_promiscuous_at` enables or disables receive-all filtering on one
interface, which is required by a layer-2 bridge that must receive frames
addressed to hosts behind another port.

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

The callbacks are a convention, not a protection boundary. Tapes run in ring
0 with no paging or memory isolation and can still touch hardware directly.
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

The optional `dhcp` word reserves NIC 1 (callback index zero) for the
`tape-net` DHCP client. Static triples then configure the other interfaces:

```text
tape run router dhcp 2 10.0.2.1 255.255.255.0
```

NAT is enabled by default. Add `no-nat` when the upstream network has an
explicit route back to the downstream subnets and should receive the original
source addresses; `nat` may be written explicitly to document or re-enable
masquerading.

DHCP requires the same bounded polling discipline as the router: it uses only
the indexed NIC callbacks, checks `should_abort`, retries for a finite timeout,
and reports whether the lease supplied an address, netmask, and router. If no
explicit default suffix is supplied, a DHCP router option becomes NIC 1's
default. At least two interfaces are required, NIC selections must be unique
and exist, and an explicit gateway must be on the selected interface's
subnet. Packets addressed to the router itself, IPv4 broadcast/multicast,
unknown networks without a default, TTL 1, and malformed/checksum-invalid
IPv4 packets are dropped. When enabled, the router performs TCP/UDP source NAT using a
4 MiB physical-memory-backed table when traffic leaves a non-primary NIC
through NIC 1, and reverses that mapping for replies. This provides tens of
thousands of flow slots and lets downstream DHCP clients reach hosts on the
upstream LAN without requiring those hosts to learn a route back to the
downstream subnet. If allocation is unavailable, it falls back to 32 slots.
Entries expire after ten minutes; new flows are rejected when the table is
full rather than evicting existing mappings. Non-TCP/UDP traffic is forwarded
without translation. Dynamic routing, ICMP
unreachable/time-exceeded errors, fragmentation/reassembly, IPv6, and a kernel
socket layer remain out of scope. `make test-tape-router` runs the
three-party QEMU/multicast test that verifies ARP, forwarding in both
directions, and TTL/checksum changes.

The router can also provide a small DHCP server with the optional
`dhcp-server` word:

```text
tape run router dhcp-server 1 10.0.1.1/24 2 10.0.2.1/24
```

Every configured interface except NIC 1 serves clients. Each pool is derived
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

If NIC 1 is configured with `dhcp`, DNS server addresses from the upstream
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

For repeatable setups, router arguments may be stored in an uppercase FOSFS
file and supplied with `@FILE`:

```text
# ROUTER.CFG
dhcp-server
dhcp-lease-file DHCP.LEASES
1 10.0.1.1/24
2 10.0.2.1/24
```

Run it with `tape run router @ROUTER.CFG`. The shell reads the file through
the kernel's FOSFS/BIOS disk path and passes its bytes through the existing
tape startup-argument ABI. The router's optional lease persistence uses the
high-level FOSFS tape callbacks rather than accessing raw sectors or BIOS.
Router configuration accepts newlines and `#` comments, and the file is
limited to 4096 bytes by the shell's fixed configuration buffer.

## Kernel Ethernet backends

`kernel/src/nic.rs` is the common facade used by the shell and all tape
callbacks. It preserves one hardware-independent contract while keeping
device-specific register programming in separate modules. `nic list` shows
every initialized interface and `nic use N` changes the active one (the shell
uses one-based numbering; tape callbacks use zero-based indices):

- `kernel/src/rtl8139.rs` supports the QEMU-default RTL8139 with its
  polling receive ring and overflow recovery.
- `kernel/src/threecom.rs` supports selected PCI 3C905/B/C identifiers using
  the card's windowed command interface and bus-master descriptor lists.
- `kernel/src/e1000.rs` supports QEMU's Intel 82540EM model through a
  memory BAR and legacy polling descriptor rings.

The 3C905 path is intentionally preliminary and has not been validated by
the repository's QEMU tests: the installed QEMU build has no 3C905 model.
Physical validation should begin with `nic`, then `nic send`/`nic recv` or
`term net`, and finally a protocol tape such as `tapes/ip-chat` or
`tapes/telnet`. A failed 3C905 initialization is reported separately from
the absence of any supported controller.

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

## `tapes/tape-net`

`tapes/tape-net` is a `#![no_std]` library, not a tape binary. It contains
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
are doubled before transmission, as required by Telnet framing.

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
a UTF-8 banner, checks the tape's negotiation replies and UTF-8 input, then
exercises abort and remote-shell recovery. This validates the protocol
boundary without requiring a TAP bridge or modifying the host's network
configuration; interoperability with a real Unix `telnetd` remains a separate
manual test.

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
cargo test --manifest-path tapes/tape-net/Cargo.toml
make test-tape-ip-chat
make test-tape-telnet
make kernel
python3 tapes/test_net_chat_e2e.py
python3 tapes/test_net_chat_e2e.py --nic e1000
python3 tapes/test_ip_chat_e2e.py
python3 tapes/test_telnet_e2e.py
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
