# History: networking, discovery, and idle-loop features

This file collects several detailed, chronological development notes moved
out of `AGENTS.md` to keep that file within its size budget. Each section
below is otherwise unedited from its original place in `AGENTS.md`.


## `poll_background_work`: one shared hook instead of four duplicated ones ("Tier 0" of the scheduler discussion) (full detail)

### `poll_background_work`: one shared hook instead of four duplicated ones ("Tier 0" of the scheduler discussion)

Investigating what a real preemptive/cooperative scheduler would cost
this project (per-task stacks, a context-switch primitive, and -- the
expensive part -- auditing every "this kernel is single-threaded"
assumption, most critically `disk.rs`'s BIOS real-mode trampoline, which
cannot tolerate any concurrent execution during its brief real-mode
window) surfaced a much cheaper, already-half-built alternative: this
kernel's several long-running blocking loops (`term_net`'s own session
loop, `nic sniff`, `nic discover`, `chat_command`, and the tape-callback
polling functions `poll_remote_tape_abort`/`poll_remote_tape_input`
described just above) already each poll hardware/keyboard state once per
iteration -- so "background work" here doesn't need real concurrency at
all, just a handful of small, cheap, non-blocking checks called from
every one of those loops instead of only some of them. This is
explicitly scoped to kernel-native recurring work only, not to
backgrounding a *tape*: a tape is structured as one big imperative
blocking loop with its state on the stack, and making one of those
(e.g. `tapes/router`) genuinely resumable without a real per-task stack
would mean hand-rewriting it into an explicit coroutine/step function --
harder, not easier, than just giving it a real stack and one `yield()`
call once a full scheduler exists. No tape or tape ABI changed for this
work.

Auditing the four independent places this ad hoc pattern had grown
(`relay_pending_tape_output`/`poll_remote_tape_abort` for `term_net`'s
own relay+network polling, `poll_remote_tape_input`'s near-identical
copy of the same for `read_key`, and `term_net`'s own session loop
inlining the identical relay-drain a third time) also surfaced a real,
previously invisible functional gap: `nic.watchdog()` -- the mechanism
that notices and recovers a latched, documented adapter fault (see
"`nic` status and ordinary foreground NIC traffic run a bounded
watchdog" above) -- was, before this change, only ever invoked from
`nic`/`nic list`/`nic watchdog`'s own command handlers, never from
inside any of these long-running loops. A fault occurring mid-capture,
mid-chat, mid-discovery, or mid-`term_net`-session (including while
still idle waiting for a client to connect) simply sat unrecovered until
that loop happened to return and a human thought to run `nic status`
afterward.

`poll_background_work(nic: Option<&nic::Nic>)` (`shell.rs`) is the one
shared hook consolidating all of this: it drains the print-relay queue
(`relay_pending_print_output`, a no-op unless a printer session is
actually active), relays queued `term_net` output to the connected peer
if this call is running nested inside an active session
(`console::relay_enabled()` plus `ACTIVE_TERM_PEER`/`ACTIVE_TERM_SOURCE`,
exactly the gating `poll_remote_tape_abort`/`poll_remote_tape_input`
already used), and calls `nic.watchdog()`. It is cheap enough to call
unconditionally on every iteration of every loop it was added to:
`watchdog()` is a handful of register/atomic reads unless a fault is
actually latched (see `Rtl8139::watchdog`), and each relay check is a
single peek against an otherwise-empty queue.

It is now called from: `term_net`'s outer "waiting for a `HELLO`" loop
and its inner `'session` loop (replacing that loop's own inlined
relay-drain duplicate); `nic_sniff`'s capture loop; `nic_discover`'s
probe/listen loop; `chat_command`'s input/receive loop; and
`poll_remote_tape_abort`/`poll_remote_tape_input` (replacing their own
bespoke relay-plus-network-poll logic with one call to this same hook,
called unconditionally before either function's own
`console::relay_enabled()`-gated logic runs -- so a tape's watchdog
recovery and print-relaying now also work when the tape was started
*without* an active `term_net` session, which the old, `term_net`-only
gated logic in those two functions never covered). No externally
observable behavior of the tape ABI (`should_abort`/`read_key`
semantics, or any tape's own output) changed; the tape e2e suite
(`tapes/test_calc_e2e.py`, `tools/test_term_net_tape_e2e.py`,
`tapes/test_net_chat_e2e.py`, `tools/test_term_net_e2e.py`) and `make
test` all pass unmodified after this refactor, confirming the
consolidation is behavior-preserving apart from the intended new
watchdog coverage.



## `term net` stays resident after a remote disconnect (full detail)

### `term net` stays resident after a remote disconnect

Originally, *any* session-ending event -- local F12, a remote
`TERM_TAG_BYE`, or the remote end simply going away -- caused `term_net`
to print a message and `return`, falling all the way through to the local
shell. Real-hardware testing exposed the problem this caused: once the
automatic boot-time `term net` wait's first session ended for any reason,
the machine dropped to the local prompt and stayed there -- a remote
operator had no way back in without a local operator physically retyping
`term net`, defeating the point of an unattended, screen/keyboard-less
machine being reachable by default.

`term_net` was restructured so the entire "wait for `HELLO`, then run a
session" body lives inside an outer `loop`. Each iteration tracks whether
*local* escape (not a remote `TERM_TAG_BYE`) is what ended the session,
via a `local_escape` flag set only in that one code path. Local escape
still sends `TERM_TAG_BYE` to the peer and `return`s out of the whole
function, exactly as before -- this is the only way to reach the local
shell now, whether at boot or from a manually typed `term net`. Every
other way a session can end (a remote `TERM_TAG_BYE`, or a fresh `HELLO`
implicitly superseding a stale one) instead prints
`"Remote terminal disconnected; waiting for a new one to connect ..."`
and loops back to the top of the outer `loop`, re-entering the
"waiting for a `HELLO`" state with no local action required. Both call
sites (`run`'s automatic boot-time call, and `term_command`'s manually
typed `"term net"` dispatch) share this one function, so the fix applies
uniformly to both.

One consequence worth flagging: since a serial-only operator has no way
to trigger "local escape" (there is deliberately no serial equivalent,
same limitation covered below), the old workaround this project's own
tests and a serial-only human operator both relied on -- have any peer
connect and then disconnect, which used to fall through to the local
prompt -- no longer works, since that disconnect now just re-arms the
wait instead. A serial-only machine's only ways back to a local prompt
are now driving it entirely over `term net` itself, or briefly attaching
a local keyboard to press one of the "give up" keys once. The affected
test scripts (`tapes/test_primes_e2e.py`,
`tapes/test_hello_rs_e2e.py`, `tapes/test_primes_vga_e2e.py`,
`tools/test_term_net_tape_e2e.py`) were all updated to drive their
commands over one continuous `term net` session (via `TERM_TAG_DATA`
frames) instead of relying on that trick -- console output always mirrors
to local VGA/serial regardless of relay state (`console::write_bytes`
writes both unconditionally), so their existing `read_until()` assertions
against QEMU's local serial stream kept working unchanged.



## `term net` at boot, and more "give up" function keys (full detail)

### `term net` at boot, and more "give up" function keys

`shell::run` now calls `term_net` once, automatically, before it ever
enters the ordinary interactive prompt loop -- so every fos machine waits
for a remote terminal client by default at every boot, with no manual
step required. This matters most for a machine with no attached
screen/keyboard: it is reachable over the network the moment it finishes
booting. `term_net` already tolerated "no NIC" gracefully (it just prints
a message and returns via `nic_or_report`), so this needed no change to
`term_net` itself, only one new call site in `run`. A local operator
watching a real screen who wants the ordinary prompt immediately, without
waiting, presses one of the "give up" keys below; if a remote client
connects first instead, it does *not* fall through to the local shell on
disconnect (see "`term net` stays resident after a remote disconnect"
above) -- only local escape does that, whether during this automatic wait
or a manually invoked `term net` session.

There is deliberately no serial equivalent of "give up" for this
automatic wait (mirrored from `term_net`'s own pre-existing convention):
reading one byte here to mean "give up" would silently eat the first
character of whatever a serial operator was about to type. A serial-only
operator can no longer fall back on "have any peer connect and then
disconnect" either, now that `term_net` re-arms itself instead of
returning on a remote disconnect -- see the note above about this
project's own e2e tests (`tapes/test_primes_e2e.py`, `tools/
tapes/test_primes_vga_e2e.py`, `tapes/test_hello_rs_e2e.py`, and
`tools/test_term_net_tape_e2e.py`) being updated to drive their commands
over one continuous `term net` session instead of relying on that trick.

Separately, `keyboard::ESCAPE_SCANCODES` (`kernel/src/keyboard.rs`) grew
from a single F12 scancode to six: F12, F4, F6, F7, F8, and F10. F12 was
always a deliberate choice for being a function key (same physical
position/scancode on every keyboard layout this project targets,
unreachable through Shift/AltGr, so nothing about the current locale
matters) rather than an ordinary printable character that would need
distinguishing from real typed text. The problem in practice: some
terminal emulators, KVMs, and remote-desktop clients intercept F12
themselves before it ever reaches a guest OS, so a fos operator using one
of those had no working "give up" key at all. Recognizing several
function keys instead -- whichever one actually reaches the guest works,
with nothing to remember or configure -- fixes this without weakening the
original rationale for using a function key in the first place. F1/F3/
F5/F9/F11 were deliberately left out since some terminals bind those to
their own emulator-level features (new tab, fullscreen, etc.); F12 stays
first in the list and in every user-facing message since it remains the
most commonly documented, most muscle-memory choice. F2 was later given
its own distinct meaning (see the parallel-port driver discussion below)
rather than being added to this "give up" list, since it needs to mean something
narrower ("abandon the line I'm typing without executing it," only inside
`term_printer`) than every other key here.


`kernel/src/parallel.rs` is a deliberately narrow legacy-ISA
parallel/Centronics port driver, discovered via the BIOS Data Area
(`0x408`-`0x40E`, LPT1..LPT4) the same way `console::detect_ports` finds
COM1..COM4 -- there is no PCI parallel-card discovery, matching this
project's "ISA only" convention for the 3C509. It implements only the
original IBM PC "SPP" (Standard Parallel Port) register set (no
bidirectional/EPP/ECP modes); `Printer::write_byte` writes a byte, pulses
Strobe, and polls the Busy status bit for up to `BUSY_POLL_ATTEMPTS`
(200,000) reads before giving up and reporting a `PrinterFault`
(`NotSelected`/`PaperOut`/`ErrorLine`/`Busy`). Status/control bit polarity
is an explicit, documented real-hardware caveat: several Centronics signals
are active-low, and this driver's polarities follow the most commonly cited
convention but have only been checked against QEMU's `-parallel` emulation.

`shell.rs` adds `term printer`/`term LPT1`..`LPT4` (an exclusive terminal
mode, mirroring `term COM1`/`term_net`) and a `print` command pair
(`print [diskN] NAME`, `print run CMD...`), backed by a second, independent
print-relay queue in `console.rs` (`PRINT_RELAY_QUEUE`) fed from the same
`relay_push` tap point the network relay queue already uses. Typed input in
`term printer` is echoed live, teletype style (written to the physical
printer the instant it is typed, not buffered until Enter); F2
(`KeyEvent::KillLine`) abandons the local input buffer and prints a fresh
line break, since already-inked characters cannot be un-printed. `help`
refuses to run while a `term printer` session is active (to avoid burning
paper), except via `print run help`. `print run CMD...`/`print [diskN] NAME`
are nesting-aware: `console::print_relay_active()` is checked first so a
command run from inside an already-active `term_printer` session doesn't
split one physical print job into two. A tape's own live output also
reaches the printer via `relay_pending_print_output()`, called from
`combined_tape_should_abort`/`poll_remote_tape_input`, mirroring the
identical `term_net` fix. Real-hardware follow-ups (a false "out of paper"
report traced to a dead CMOS battery resetting the BIOS parallel-port
config; dot-matrix printers cannot show true character-by-character
teletype output since they buffer a full line internally; an interactive
tape's output was initially batched until it exited) are recorded in full,
along with the user's physical Panasonic KX-P1083 as the real-hardware
validation target, in `docs/history/development-notes.md`.



## A numbered peer address book for `send_tape.py`/`term_net.py` (full detail)

### A numbered peer address book for `send_tape.py`/`term_net.py`

Both tools already broadcast the same `CHAT_TAG_DISCOVER_REQUEST` probe
`nic discover` sends and pick the destination automatically when exactly
one fos machine replies. With more than one on the wire, though, each
was previously a dead end: print every MAC found and ask for `--mac`
spelled out in full -- every single run, since a short-lived Python
process has nowhere of its own to remember "peer 2 was that RTL8139 at
`52:54:00:...`" the way the fos kernel's own `PEERS` table
(`kernel/src/shell.rs`) does for a whole boot session, letting `chat`/
`tape send` accept a small `PEER_NUMBER` instead.

`send_tape.py` now keeps a small on-disk address book mirroring that
table -- `load_known_peers`/`save_known_peers`/`remember_known_peer`/
`resolve_peer`/`print_known_peers`, all reused by `term_net.py` (which
already imports `send_tape` for its wire-format helpers) and the new
`tools/peers.py`. It's a JSON file, by default `~/.cache/fos_peers.json`
(`PEERS_CACHE_FILE`, overridable via the `FOS_PEERS_FILE` environment
variable so tests can point it somewhere disposable instead of a real
user's cache): every discovery either tool performs merges newly seen
peers into it in first-seen order, exactly like `remember_peer`, and
`--mac` on either tool now accepts either a full MAC address or a
1-based index into this list, exactly like `chat`/`tape send` accept
`PEER_MAC|PEER_NUMBER`.

`tools/peers.py` is the standalone equivalent of `nic peers`/
`nic discover`: no arguments just prints the cached list (no network
traffic); `--discover` broadcasts one probe, merges replies in, and
prints the resulting list; `--forget` empties it. The one real
difference from the kernel's own table is persistence itself: the
kernel's starts empty every reboot, so a stale entry there is
self-limiting, while this one survives indefinitely across separate
tool invocations and can go stale (a MAC that answered yesterday might
be a different machine, or offline, today) -- hence `--forget`, so a
person notices and resets it deliberately rather than either tool
trying to guess when an entry has gone stale on its own.



## Hostname-based NIC discovery, and an always-on ambient discovery loop (full detail)

### Hostname-based NIC discovery, and an always-on ambient discovery loop

Every peer this project's discovery mechanisms (`nic discover`/`nic
peers`, `chat`'s own discovery, `tools/peers.py`'s address book) tracked
was previously identified only by MAC address -- functional, but not
memorable across a LAN with more than a couple of fos machines on it.
`kernel/src/shell.rs` now loads an optional `hostname` file the same way
`locate_start_file` looks for `start`, but checking *every* attached
disk rather than stopping at the first one with a FOSFS volume:
`load_hostname` tries `disk1` first, then every secondary disk in order
(`0..disk::secondary_count()`), since a hostname is just as likely to
live on a small config disk as a boot-time script is, and any error
short of "found a valid, differently-sized file" is folded into "no
hostname here," mirroring `locate_start_file`'s own `disk2`-is-commonly-
blank reasoning. The file's contents are trimmed of trailing whitespace
(`trim_trailing_whitespace`, distinct from the wire-payload
`trim_trailing_zero` below) and capped at `HOSTNAME_CAPACITY` (16 bytes);
an oversized file is reported and ignored rather than silently
truncated. A machine with no `hostname` file anywhere behaves exactly as
it always has -- this is purely additive.

Because one machine can have several NICs, and a bare hostname could not
tell two interfaces on the same host apart, `interface_hostname(index,
out)` formats the configured base name with a 1-based interface number
appended (`myhost1`, `myhost2`, ...) -- consistent with every other
1-based NIC numbering already in this project's UI (`nic list`/`nic use
N`). This formatted, per-interface name is what actually gets sent and
displayed everywhere below; the bare on-disk `hostname` file content is
only ever a template for it. `nic list` prints a machine's own
`hostname: NAME` line under each configured interface once a hostname
exists.

**Carrying a hostname over the wire.** `send_chat_control` (the one
function behind every `CHAT_TAG_DISCOVER_REQUEST`/`REPLY` frame this
project sends -- confirmed to have no other caller) grew an
`interface_index` parameter and now appends that interface's formatted
hostname (or nothing, if unconfigured) as a payload right after the
existing tag byte. This is safe on the wire for the same reason
`CHAT_TAG_TEXT`/`TERM_TAG_DATA` payloads already are: neither
`tools/send_tape.py`'s `discover()` nor `tapes/net-chat`'s frame handler
validate an exact/maximum frame length, only the tag byte itself, so an
older sender's shorter (no-payload) discovery frame and a newer
receiver's longer one interoperate unchanged in both directions. The
receiving side trims trailing zero padding (`trim_trailing_zero`,
matching the identical convention already used for `CHAT_TAG_TEXT`) since
Ethernet's 60-byte minimum-frame padding must never be mistaken for part
of the name.

**The peer table now remembers names, not just MACs.** The former
`PEERS: [[u8; 6]; MAX_PEERS]` table became `[Peer; MAX_PEERS]`, `Peer`
holding a MAC plus an optional fixed-size hostname buffer/length.
`remember_peer(mac, hostname)` only *fills in* a hostname for an entry
that does not have one yet -- it never overwrites an already-known
hostname with a later, possibly different one from the same MAC. This
was a deliberate simplicity choice (first-seen name wins for the rest of
that boot), not a live-rename feature; a machine whose `hostname` file
changes mid-session will not update an already-recorded peer's displayed
name until that peer entry is naturally re-created (e.g. after a
reboot). `print_peers`/`nic peers` show `MAC (name)` once a hostname is
known, falling back to a bare MAC otherwise, exactly like before.

**Always-on ambient discovery, not just the dedicated `nic discover`
command.** The original ask was "can discovery just run all the time?",
which this project's single-threaded, no-scheduler, busy-polling
architecture (see "Scheduler" discussion elsewhere in this document)
answers narrowly: there is no background execution at all, only
"whichever loop happens to be running right now can also poll for this."
Every existing long-running command loop (`chat`, `term net`, `nic
discover`, a tape's own `should_abort`/`read_key` polling) already
replied to an incoming discovery probe unconditionally -- those needed no
change beyond the hostname-payload plumbing above. The one genuine gap
was the *idle local prompt loop* in `run()`: it never touched the NIC at
all between commands, so a fos machine sitting at its own prompt (the
most common steady state) was invisible to discovery and could not
receive one either, unless an operator happened to be running `nic
discover`/`chat` at that exact moment.

`poll_background_work()` -- the existing Tier-0 unification point shared
by many short command loops -- was deliberately *not* extended to
auto-broadcast a probe, since doing so would make every one of its many
callers start transmitting extra frames, which was not the ask. Instead,
a new `ambient_discovery_poll()` is called only from the idle prompt
loop, once per iteration. Two independent behaviors, arrived at after
explicit user steering across several iterations of this design:

- **Reply and remember, always, silently.** Every iteration, it drains
  and answers any incoming `CHAT_TAG_DISCOVER_REQUEST`/`REPLY` on every
  interface exactly like `nic_discover`'s own loop does, but with no
  `println!` output at all -- unlike the dedicated command, this runs
  underneath an interactive prompt display and must never corrupt it.
- **Self-announce on a schedule, not on every idle iteration.** The
  first design considered ("broadcast a probe every idle iteration") was
  rejected by the user as too noisy; the next ("only reply, never
  broadcast") was rejected in turn since two machines that both only
  reply and never initiate would never discover each other at all
  without a human running `nic discover` on at least one side. The
  design actually built: broadcast one immediate self-announce the
  first time the idle loop starts (fast convergence right after boot),
  then continue re-announcing every `AMBIENT_BOOT_BROADCAST_TICKS` (~3s,
  the same cadence `nic_discover`'s own rebroadcast already uses) for
  `AMBIENT_BOOT_BURST_TICKS` (~30s) after that first call -- explicitly
  to cover two machines booting around the same time but not perfectly
  in sync, so each one's boot-time burst window has a good chance of
  overlapping the other's -- and settle to a much slower
  `AMBIENT_STEADY_BROADCAST_TICKS` (~60s) afterward, since a machine that
  has been up and idle for a while has little need to keep re-announcing
  aggressively. `nic_discover`'s own command keeps its separate,
  unmodified, faster fixed-interval rebroadcast loop -- the two are
  intentionally not unified, since a human explicitly running `nic
  discover` should still get its existing fast convergence regardless of
  ambient discovery's own slower steady-state cadence.

Verified end to end with a new, dedicated two-guest test,
`tools/test_hostname_discovery_e2e.py` (not part of `make test`,
mirroring `tapes/test_net_chat_e2e.py`'s structure): each guest is built
from its own tiny image carrying a distinct `hostname` file (`alpha`/
`beta`) via `build_image.py --file hostname ...`, both are driven to
their local idle prompt (F12 out of the automatic boot-time `term net`
wait, never running `term net`/`nic discover` themselves), and after a
short wait for a couple of ambient-discovery rounds, `nic peers` on each
side shows the other's formatted name (`(beta1)`/`(alpha1)`) -- proving
the whole chain (hostname file -> wire payload -> peer table -> display)
works purely through the idle loop's own ambient polling, with no
dedicated discovery command ever invoked by either guest.



## NETFS: a simple network filesystem over discovered peers (full detail)

### NETFS: a simple network filesystem over discovered peers

Once every machine on a LAN segment reliably shows up in `nic peers` by
name, the natural next question was whether a file could be fetched
straight from a peer's own disk without first bouncing it through a
disk that both machines share -- motivated directly by the target use
case `copy hostname1:disk2/file.name disk2`. NETFS is the small,
deliberately unauthenticated protocol this project ended up with, reusing
`CHAT_ETHERTYPE` (the same one `chat`/`nic discover`/`tape`/`term net`
already share) and the same design philosophy as every other protocol in
this codebase: no retransmission, no security, and an honestly documented
accepted risk where corners were cut, rather than hidden complexity to
paper over them.

**Wire protocol.** Ten new tags continue the numbering after
`TERM_TAG_BYE`: `NETFS_TAG_GET_REQUEST`/`GET_START`/`GET_DATA`/
`GET_STOP`/`GET_ERROR` and `PUT_REQUEST`/`PUT_DATA`/`PUT_STOP`/`PUT_ACK`/
`PUT_ERROR`. A GET request carries a one-byte remote disk number plus the
file name; the server replies with a `_START` frame declaring the exact
length and a 16-bit checksum, then a `TAPE_TAG_DATA`-shaped burst of
`_DATA` frames (same sequence-number-and-declared-length discipline as
`tape load net`'s own chunked transfer, for the same reason: a binary
file's own trailing zero bytes must never be mistaken for Ethernet's
60-byte minimum-frame padding), then `_STOP`. A PUT request instead
carries the length and checksum up front (the client already has the
whole file in memory before sending), followed by the same `_DATA`/
`_STOP` shape, and the server replies with `_ACK` or a one-byte
`_ERROR` code. Nine error codes cover not-found, disk-unavailable,
too-large, invalid-name, directory-full, data-full, a checksum mismatch,
a generic disk error, and `BUSY` -- the last one new to this feature: a
peer already mid-PUT rejects a second concurrent `PUT_REQUEST` outright
rather than letting two transfers corrupt the one shared staging buffer
(see below).

**The checksum.** Every transfer computes the standard RFC 1071 16-bit
Internet checksum (`internet_checksum`, the same algorithm
`tapes/tape-net`'s IPv4/UDP/TCP layers already use) over the *entire*
file, verified only once the whole thing has been reassembled -- there is
no partial-frame verification, matching this protocol's "no
retransmission, abort cleanly on any detected problem" philosophy
throughout. GET carries the checksum in `_START`, since the server only
knows it once the file has actually been read from disk; PUT carries it
in `_REQUEST`, since the client already holds the whole file in memory
up front. A mismatch on either side aborts before any write happens (a
GET never installs a corrupt tape or writes a corrupt file locally; a PUT
server never calls `fosfs::write` with data that failed verification).

**Peer references never accept a raw MAC address.** `resolve_peer` (used
by NETFS's `parse_remote_host_token`/`parse_remote_token`) accepts only
an `nic peers` index number or a known hostname -- deliberately not a
colon-separated MAC address, unlike `chat`/`tape send`'s existing peer
resolution, because a MAC's colons would be genuinely ambiguous against
`HOST:diskN`'s own colon. This is a one-line, deliberate scope narrowing
for NETFS specifically, not a change to `chat`/`tape send`'s own
resolution.

**Command syntax**, layered onto every existing command that already
moves a whole file through the shared `pmem::TAPE_BASE`/`TAPE_LEN`
staging buffer:

```
copy HOST[:diskN]/NAME DST_DISK [NEW_NAME]     remote source (NETFS GET)
copy SRC_DISK NAME HOST[:diskN] [NEW_NAME]     remote destination (NETFS PUT)
tape load HOST[:diskN] NAME                    remote source, straight into the tape buffer
tape save HOST[:diskN] NAME                    remote destination, from the loaded tape
tape run @HOST[:diskN]/NAME                    remote source, as a startup-argument file
```

A remote *source* is always written as one `HOST[:diskN]/NAME` token
(the same reasoning `tape run`'s pre-existing `@diskN/NAME` already
established: FOSFS names never contain `/`, so it's an unambiguous
separator), because it needs to carry a file name alongside the host. A
remote *destination* has no file name of its own to combine with -- the
name always comes from the source side (or an explicit `NEW_NAME`) -- so
it's just `HOST[:diskN]`, the same shape as a local `DST_DISK`/`diskN`
selector; `parse_copy_destination`/`parse_save_destination` each try a
local `diskN` selector first, falling back to a peer reference.
`tape run`'s existing `TapeArgumentSource` enum gained a `RemoteFile`
variant fetched into the small stack-resident `file_buffer`
(`TAPE_ARGUMENT_FILE_CAPACITY`, 4096 bytes) that argument already used
-- deliberately *not* `pmem::TAPE_BASE`, since a tape is already resident
there by the time `tape run` parses its arguments, and clobbering it
mid-fetch would corrupt the very tape about to run. `copy`/`tape load`'s
remote-GET call sites, which have no such conflict, pass the full
`pmem::TAPE_BASE`/`TAPE_LEN` region as `netfs_get`'s destination buffer
instead -- `netfs_get` takes that buffer as a caller-supplied slice
specifically so both call sites can share one function with no
special-casing; it learns the real length from the `_START` frame and
errors out before writing anything if it would overflow the caller's
buffer. Attempting `copy HOST1:.../NAME HOST2:...` (two remote endpoints
at once) is explicitly rejected with a message to copy through a local
disk in two steps instead -- NETFS moves a file to or from *this*
machine's own disk, never directly between two other peers.

**Serving is scoped to the idle prompt loop only**, mirroring ambient
discovery's own scoping decision above and for the same reason: this is
the one loop already proven safe to extend with extra, silent background
polling without corrupting an interactive display. `ambient_discovery_poll`
now also recognizes and dispatches the four request-carrying tags
(`GET_REQUEST`/`PUT_REQUEST`/`PUT_DATA`/`PUT_STOP`) to
`serve_netfs_get_request`/`serve_netfs_put_request`/`serve_netfs_put_data`/
`serve_netfs_put_stop` -- unlike routine discovery traffic, NETFS activity
*is* printed, since it is a much rarer, more consequential event than an
ordinary discovery probe/reply. A GET is served synchronously within one
`ambient_discovery_poll()` call (read the file, compute the checksum,
send the whole paced burst) -- an accepted blocking cost, the same
trade-off `tape send` already makes. A PUT is necessarily stateful across
many idle-loop iterations instead (one frame arrives at a time), tracked
in a small `NetfsPutTransfer` held in a `static mut` -- a second,
concurrent `PUT_REQUEST` while one is already in flight is rejected with
`BUSY` rather than silently clobbering it. **A fos machine must be
sitting idle at its own local prompt to serve a NETFS request at all** --
not inside `chat`, `nic discover`, `term net`, or a running tape's own
polling loop, all of which keep working exactly as they did before this
feature and simply do not answer NETFS traffic.

**Accepted shared-buffer collision risk.** NETFS serving reuses the same
`pmem::TAPE_BASE`/`TAPE_LEN` staging buffer that a locally typed
`copy`/`tape load` command already uses. Because a PUT transfer can span
several real seconds across many idle-loop iterations, a local operator
typing a `copy`/`tape load` command on the *serving* machine at the same
moment could corrupt both. This is an explicit, documented limitation of
fos's single-threaded, no-locking design (see `NetfsPutTransfer`'s own
doc comment in `kernel/src/shell.rs`) -- not something this feature
guards against, consistent with how little else in this kernel guards
against concurrent misuse of shared state.

Verified end to end with `tools/test_netfs_e2e.py` (not part of `make
test`, mirroring `test_hostname_discovery_e2e.py`'s two-guest structure,
each guest additionally carrying its own blank second disk and the
prebuilt `calc.bin` tape): a remote GET (`copy HOST:disk2/NAME disk2`), a
remote PUT (`copy disk1 NAME HOST:disk2`), `tape load HOST NAME`, `tape
run @HOST:diskN/NAME` (evaluating `3 4 +` through the `calc` tape to
prove a fetched startup-argument file reaches a real tape), and a clean
`NOT_FOUND` error for a nonexistent remote file all pass.



## HLT-on-idle: letting the CPU actually rest at the idle prompt (full detail)

### HLT-on-idle: letting the CPU actually rest at the idle prompt

Every polling loop in this kernel busy-spins by design -- there is no
scheduler, so "poll hardware, then loop" is how anything gets noticed at
all. But the one loop that is *always* running whenever nothing else is --
`shell::run`'s own idle prompt loop -- was spinning the CPU at full speed
even while genuinely idle (the common steady state, especially for a
debug-mode "unattended, always up" machine sitting at its own prompt
between remote sessions). `arch::x86::halt_until_interrupt` (distinct from
the existing `halt_forever`, which `cli`s and never returns -- this
executes exactly one `hlt` and returns once any interrupt wakes it) lets
that loop stop burning cycles/power/heat between polls, called only when a
whole iteration found nothing at all to do.

The reasoning for *not* needing the textbook `sti; hlt` atomic idiom here
(which exists to close the race where a wakeup becomes pending between the
caller's last check and `hlt` itself executing): every caller reaches this
function with interrupts already unconditionally enabled for the entire
life of the loop, and the PIT (IRQ0) is never masked and fires at a fixed
100 Hz regardless of anything else -- so even a "missed" wakeup right at
the call boundary only costs one more PIT tick (~10ms), never an
indefinite hang. This bounded worst case is a property of this specific
loop's environment, not a general guarantee `halt_until_interrupt` makes
on its own; a caller anywhere PIT ticks could ever be masked, or that
might reach this function with interrupts off, would need the full
`sti;hlt` idiom instead.

This is deliberately scoped to *only* `shell::run`'s idle loop, not
`term_net`'s session loop, `nic_sniff`, `nic_discover`, `chat_command`, or
any tape network-transfer loop (`tape_load_net`, `netfs_get`/`put`,
`nic_discover`'s own loop). Every one of those is actively polling NIC
receive with no interrupt backing at all (every NIC backend's own ISR only
acks/counts/flags a diagnostic `work_pending` nothing actually waits on --
`receive()` always polls hardware directly regardless), so none of them
are "idle" in the sense this feature targets, and halting there would add
the same latency for zero benefit.

The one loop this does apply to also happens to be the *only* one that
serves NETFS GET/PUT and KUP traffic (`ambient_discovery_poll`, see
"NETFS" above and "KUP" under "Debug mode" below), which arrives in paced
chunks only a few milliseconds apart (`tools/send_kernel_update.py`'s
default `--delay 0.003`). Halting the instant a single iteration finds
nothing would halt right after draining one chunk while the next is still
in flight, adding up to one PIT tick of latency *per chunk* instead of
just once per idle stretch -- a real, roughly 3x slowdown of an
already-working feature, not a rounding error. `ambient_discovery_poll`
therefore returns whether it actually received a frame this call, and
`run`'s loop only halts once `AMBIENT_IDLE_GRACE_TICKS` (2 ticks, ~20ms --
comfortably above that ~3ms chunk pacing) have passed with no NIC activity
*and* no local keyboard/serial byte. An active transfer keeps refreshing
that deadline faster than it can elapse, so the loop never halts
mid-transfer; only genuine idle time triggers it.

One accepted, bounded caveat: COM1 (serial input) has no interrupt behind
it at all (IRQ4 is never unmasked), so once the machine is truly idle and
halting, a serial-typed byte can incur up to one PIT tick (~10ms) of added
latency before it's noticed, versus effectively 0ms before this change.
Verified with `make test` (its bounded waits are 10-second-scale, utterly
unaffected) and `make test-debug-mode` (KUP is served from this exact
loop), plus a manual re-run of `tools/test_netfs_e2e.py` and
`tools/test_hostname_discovery_e2e.py` to confirm no throughput regression
in the paced-transfer traffic this change is riskiest against.

