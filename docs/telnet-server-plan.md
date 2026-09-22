# Plan: a telnet-server tape

Status: **design only, nothing implemented yet.** This captures the decisions
made so far so the work can be picked up later without re-deriving them.

## Goal

Let someone `telnet` into a running fos machine and interact with tapes
remotely, without giving the kernel any TCP/IP awareness it doesn't already
have. Motivating case: put the load-and-run commands in the `start` boot
file so the telnet server is listening as soon as the machine boots,
unattended.

## Constraints that shaped this design

- A tape cannot load or run another tape itself. There is no such callback
  in the `TapeEntry` ABI (`kernel/src/shell.rs:3650`), and it would be unsafe
  even if there were one: every tape loads into the same fixed 4 MiB
  `pmem::TAPE_BASE` region and executes in place with no relocation
  (`kernel/src/pmem.rs:48-49`, `kernel/src/shell.rs:2606-2616`). Only the
  *kernel's* loader may safely overwrite that region, and only once the
  previously running tape has returned.
- The kernel has no TCP/IP stack and is not getting one. `kernel/Cargo.toml`
  has zero dependencies; NIC drivers only do raw Ethernet frame send/receive
  (`kernel/src/nic.rs:75,84`). All ARP/IPv4/TCP logic lives in the shared
  `tapes/tape-net` crate, linked per-tape (see `NETWORKING.md`). A telnet
  server therefore has to be a tape, not a kernel feature, and it has to
  carry its own TCP handling.
- The existing nested-tape pattern (`term net`, `term printer`) only works
  because the *outer* loop is kernel/shell code living outside TAPE_BASE
  (`shell.rs:1113`, `:1857`), not a tape — that's what makes it safe for it to
  load a fresh tape and jump into it. A telnet-server tape doesn't have that
  luxury: it *is* the thing running from TAPE_BASE, so handing off to a
  workload tape needs a different mechanism (below).
- The fos interactive shell (`tape run`, `nic`, `term net`, ...) is
  kernel-resident code (`execute()`, `shell.rs:324`), not a tape. Nothing in
  this design gives telnet users that shell — they get whatever command
  surface the telnet-server tape itself implements.

## The chain-load ABI addition

Add one out-parameter to the tape ABI (not a callback): a pointer to a
kernel-owned scratch struct the tape can fill in before returning normally.

```rust
#[repr(C)]
pub struct ChainRequest {
    want_chain: u8,       // 0 = normal exit, nonzero = chain to another tape
    next_tape: [u8; 64],  // FOSFS filename, NUL-padded
    args: [u8; ARGS_CAP], // opaque payload, becomes the next tape's args
    args_len: u32,
}
```

This struct lives in kernel memory, outside TAPE_BASE, so it survives the
region being overwritten when the next tape loads. The kernel zeroes it
before each call and passes `&mut ChainRequest` as a new trailing arg to
`TapeEntry`.

`tape_run` (`shell.rs:3583`) becomes a loop instead of a single call:

```rust
fn tape_run_chain(mut path: TapeFileName, mut args: Vec<u8>) {
    loop {
        load_tape_from_disk(&path);
        reset_tape_session_state();
        let mut req = ChainRequest::zeroed();
        call_tape_entry(args.as_ptr(), args.len(), &mut req, /* ... */);
        if req.want_chain == 0 { break; }
        path = TapeFileName::from_bytes(&req.next_tape);
        args = req.args[..req.args_len as usize].to_vec();
    }
}
```

Safe because the requesting tape has already returned/unwound before the
kernel touches TAPE_BASE again. This primitive is general — not
telnet-specific — and existing tapes are unaffected (they just leave
`want_chain` zeroed, which is the default).

## TCP connection handoff data

What a telnet-server (or menu loop) tape puts in `ChainRequest.args` when
handing an established connection to a workload tape:

```rust
#[repr(C)]
struct TcpHandoff {
    local_mac: [u8; 6],  peer_mac: [u8; 6],
    local_ip: [u8; 4],   peer_ip: [u8; 4],
    local_port: u16,     peer_port: u16,
    seq: u32,            ack: u32,
    window: u16,
}
```

Pure data, no pointers — the workload tape reconstructs its own
`tape_net::TcpConnection` from these fields and continues the connection
where the telnet-server left off, driving it itself via the `nic_send`/
`nic_receive` callbacks it already has in the ABI.

**Scope note:** this only works for *telnet-aware* workload tapes — ones that
link `tape-net` and speak TCP/telnet framing themselves. It does not make an
arbitrary existing console tape (one that just calls `putc`/`read_key`)
transparently network-accessible the way `term_net` does, because that
transparency comes from kernel-resident protocol code, which we're
deliberately not adding. Revisit if/when a specific non-network-aware tape
needs to be reachable this way.

## Session model: menu, not one-shot

Decided: a connection is a persistent session, not "one tape run per TCP
connection."

1. Telnet-server tape accepts a connection, does telnet option negotiation
   (see "line mode" below), prompts for the password.
2. Reads the password file's contents via its own `fs_read` callback (path
   comes in as `args`, see below) and compares against what the client sent.
3. On success, shows a small prompt (`fos telnet> `) and reads commands —
   at minimum something like `run <tapename>`.
4. On `run <tapename>`, fills a `ChainRequest` with `next_tape = <tapename>`
   and `args` = the current `TcpHandoff`, and returns.
5. The workload tape runs, using the live connection. When it's done (client
   quit, or its own logic decides to return), it fills its own
   `ChainRequest` back to the menu tape (`next_tape = "telnet-server"`, args
   = updated `TcpHandoff` for the *same* still-open connection — this is not
   a fresh accept).
6. On client disconnect entirely, the resident tape's loop notices (FIN/RST)
   and chains back to `next_tape = "telnet-server"` with empty args, which
   goes back to accepting new connections.

This gives multi-tape sessions over one connection, closer to the real
console experience, at the cost of the menu tape needing its own small
command dispatcher (deliberately not the full kernel shell — see
constraints above).

## Password

- Command-line convention already exists and is reused as-is: tape args are
  a single raw string, unparsed by the kernel (`shell.rs:3685-3686`) — the
  tape splits it itself, same as e.g. `router`'s `.cfg` parsing.
- `tape run telnet-server /telnetpw.txt` → `args` is literally the path
  `/telnetpw.txt`. The telnet-server tape reads that file once at startup
  via its `fs_read` callback and holds the expected password in memory for
  comparison.
- Explicitly a low-security "simple password," not real auth. No existing
  auth pattern anywhere else in the repo to be consistent with (checked —
  greenfield).

## Line mode vs character mode: going with line mode

- The menu is fundamentally a line-oriented prompt, which is exactly what
  telnet's default line-buffered mode gives for free: client buffers and
  echoes locally, sends a whole line (CRLF-terminated) per read. No need to
  negotiate character mode (`IAC WILL ECHO` / `WILL SUPPRESS-GO-AHEAD`) or
  implement per-keystroke echo-back in the telnet-server tape.
- Trade-off accepted: the password is echoed in clear text by the client
  while typing, since we're not suppressing local echo. Fine for a "simple
  password," not fine if this ever needs real secrecy.
- Not a global constraint forever: telnet allows renegotiation mid-connection.
  A specific workload tape that needs true per-keystroke interaction (a
  game, cursor keys, etc.) can negotiate into character mode itself when it
  takes over, and back to line mode when control returns to the menu. No
  redesign needed for that later.

## Handling a second connection while one is active ("busy")

Only one tape session runs at a time (sequential design, no scheduler). Two
options considered:

1. **Simple (recommended starting point):** the telnet-server's own menu
   loop watches for and politely rejects a second inbound SYN with a short
   message before closing, but only while *it* (not a workload tape) is
   resident. A second connection attempt that lands while a workload tape is
   running just gets a normal TCP timeout — no nice message, but no extra
   code in every workload tape either.
2. **Consistent:** factor the "notice a foreign SYN, reply busy, close"
   logic into a small shared helper next to `tape-net` that every
   telnet-participating workload tape calls once per poll iteration, so the
   busy message works no matter what's currently resident.

Decision: start with (1). Revisit (2) once there's a real workload tape to
test against.

## Boot integration via the `start` file

The `start` boot mechanism already does exactly what's needed:
`START_FILE_NAME = "start"` (`shell.rs:17`), read at boot and each line run
through the same `execute()` the interactive shell uses
(`run_start_file`/`execute_start_file`, `shell.rs:36`, `:147`, `:181`).

```
tape load disk telnet-server
tape run telnet-server /telnetpw.txt
```

Caveats to remember:

- `tape run` blocks, and this tape's chain loop is meant to run forever
  (menu ⇄ workload ⇄ menu ⇄ ...), so this must be the **last line** in
  `start` — nothing after it will ever execute.
- This also means the local physical console gets absorbed into the
  session indefinitely unless the resident tape's poll loop *also* watches
  local `read_key`/`should_abort` as an escape hatch — reuse the same
  dual-polling pattern `term_net`/`term_printer` already use for
  local+remote input (`combined_tape_should_abort`, `combined_tape_read_key`,
  `shell.rs:2861`, `:3313`) so a local operator isn't locked out.

## Open / deferred, not yet decided

- Exact menu command syntax beyond `run <tapename>` (help text, listing
  available tapes, etc.).
- Chain-request validation/guardrails: what happens on a malformed or
  missing `next_tape` file, or an accidental chain loop — needs some bound
  or sanity check in the kernel's chain loop.
- The telnet-server tape's internal structure/module layout hasn't been
  sketched yet.
- Whether any workload tape actually needs character-mode renegotiation —
  no concrete workload picked yet.
- Busy-message option (2) above, deferred until there's a real workload to
  test against.
