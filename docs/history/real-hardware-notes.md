# History: confirmed real-hardware bugs and fixes

This is the detailed record of behaviors that only ever surfaced on
physical fos machines (never in QEMU), moved out of `AGENTS.md` to keep
that file within its size budget: A20 enable ordering, PIC/PIT command
timing, the protected-mode IRQ window during BIOS disk calls, spurious
IRQ7/IRQ15 handling, several RTL8139 quirks (interframe gap, CR/RCR/TCR
write order, receive-ring wedges, buffer overflow, link-recovery), and the
predicted risk list for `reboot soft` on real hardware. See `AGENTS.md`'s
own "Real hardware notes" section for a condensed summary.

QEMU is this project's primary validation environment, but a few behaviors
have only ever surfaced on physical machines; both are recorded here so a
future contributor doesn't have to rediscover them from scratch:

- **VGA cursor and boot narration** -- writing to the `0xb8000` text
  framebuffer does not move the blinking hardware cursor by itself; the
  kernel drives the CRTC cursor-position ports (`0x3d4`/`0x3d5`) explicitly
  after every write, and continues from wherever the real-mode loader's own
  BIOS-teletype printing left the cursor rather than clearing the screen at
  boot, both confirmed necessary on a real Pentium 4 machine booted from
  USB (see `README.md`'s "Prompt" section).
- **A20 enable order** -- `enable_a20` (`boot/stage2.S`, duplicated in
  `boot/netrom.S`) used to only write the "fast A20 gate" system control
  port (`0x92`). That worked fine in QEMU, and this reordering is a real,
  worthwhile robustness fix regardless, but it turned out *not* to be the
  cause of the AMD Athlon 64 X2 reset loop below -- trying the BIOS
  (`INT 15h`, `AX=2401h`) and the 8042 keyboard-controller gate before
  falling back to port `0x92` (re-verifying A20 after every attempt rather
  than trusting any method's own reported success) is still worth keeping,
  since some real chipsets have side effects on writes to that port beyond
  toggling A20 that a pure emulator won't reproduce -- it just wasn't
  what was resetting *this particular* machine.
- **PIC/PIT command timing** -- `program_pic`/`program_pit`
  (`kernel/src/interrupts.rs`) used to send their ICW/OCW command bytes to
  the 8259/PIT back-to-back. QEMU's emulated 8259 accepts that fine, but a
  real chipset can be slower to latch one command byte before the next
  arrives, silently leaving the PIC mis-programmed (there's no error
  status to check). The fix inserts a short `io_wait()` (a throwaway write
  to port `0x80`) after every PIC/PIT command byte, the standard technique
  for this exact class of bug. This did not turn out to be the cause of
  the AMD Athlon 64 X2 reset loop below (see the spurious-IRQ entry), but
  is still worth keeping as a general robustness fix.
- **Protected-mode IRQ window during BIOS disk calls** -- `disk::transfer`
  (`kernel/src/disk.rs`) must keep interrupts disabled after loading the
  real-mode IVT and BIOS PIC mapping, until `boot/stage2.S`'s
  `bios_disk_real` has actually switched the CPU to real mode. An earlier
  version executed `sti` while the processor was still in protected mode
  with `IDTR=0`; any timer or keyboard IRQ in that window interpreted the
  IVT's 4-byte far pointers as protected-mode IDT gates and triple-faulted
  back to reset. This was especially reproducible on the AMD machine during
  boot and during the many-sector disk load of `primes.bin`. The trampoline
  now enables interrupts only after entering real mode, where BIOS's timer
  tick and disk-timeout logic can run safely, and executes `cli` before
  returning to protected mode.
- **CPU exception handlers as a diagnostic tool** -- added alongside the
  PIC/PIT fix above, since that fix couldn't be confirmed without the
  original hardware: all 32 CPU exception vectors (`kernel/src/interrupts.rs`,
  `install_exception_handlers`) now have real IDT gates instead of being
  left `missing()`. Each prints the vector number, its name, the
  CPU-pushed error code if the vector has one, and the faulting `CS:EIP`,
  then halts. Verified in QEMU by deliberately executing `int3` right
  after `interrupts::initialize()` and confirming the diagnostic printed
  and the machine halted cleanly instead of resetting (then reverting
  that temporary trigger). This is exactly what let the AMD Athlon 64 X2
  reset loop below get diagnosed instead of staying a silent reset.
- **Spurious IRQ7/IRQ15 -- confirmed cause of the AMD Athlon 64 X2 reset
  loop** -- with the exception handlers above in place, the real hardware
  that had been resetting instead printed
  `CPU EXCEPTION 13 (General Protection Fault), error code 0x0000013b, at
  0008:0001218f` right after the "Physical memory:" line. The error code
  decodes (bit0 EXT=1, bit1 IDT=1, bits3-15 index = `0x13b >> 3` = `0x27`)
  to "IDT gate 0x27 is not a valid gate descriptor" -- vector `0x27` is
  `IRQ_BASE + 7`, i.e. IRQ7 on the master 8259. This kernel only ever
  unmasks IRQ0/IRQ1, and nothing is wired to IRQ7, but the 8259 hardware
  itself uses IRQ7 (and IRQ15 on the slave) as its own "spurious
  interrupt" signal: a device can assert then withdraw its interrupt
  request before the PIC latches it (electrical noise, a glitchy
  peripheral, ...), and the 8259 reports that as IRQ7/IRQ15 regardless of
  masking. QEMU's idealized 8259 essentially never generates one, so this
  never surfaced there. Because `IdtEntry::missing()` zeroes the
  `type_attributes` byte -- an invalid gate type, not just "not present"
  -- the CPU's own gate-type check (which runs before the present-bit
  check) raised `#GP` rather than `#NP`, and with no `#GP` handler
  previously installed, that cascaded into the double-/triple-fault reset
  loop. The fix gives IRQ7 and IRQ15 real IDT gates
  (`spurious_master_interrupt`/`spurious_slave_interrupt`) that read the
  relevant PIC's in-service register before deciding whether to send an
  EOI at all, per the standard spurious-IRQ protocol -- a genuine EOI-less
  spurious IRQ7 is now a normal, silent, roughly-never-noticed event
  instead of a fatal exception. `irqstat` reports how often it happens.
  **Confirmed fix for the originally reported bug.**
- **RTL8139 transmit interframe gap** -- `Rtl8139::init` (`kernel/src/rtl8139.rs`)
  used to program `TCR` (Transmit Configuration Register) as all zeros,
  leaving the Interframe Gap Time field (bits 25:24) at its minimum. This
  looked fine by every signal the driver itself checks (`nic send` always
  reported success, since the card's `TOK` status bit only reflects its
  own FIFO/abort state, not whether anything downstream accepted the
  frame), and QEMU's virtual NIC/hub happily ignored the too-short gap
  too -- but on real hardware, no other machine ever received anything a
  real fos host sent, including another fos host's own `nic sniff`, even
  though that same fos host's `nic sniff` correctly captured ordinary
  external traffic (confirming the receive path, RCR config, and ring
  buffer parsing were all fine). This is a well-known RTL8139 gotcha --
  Linux's historic `8139too.c` driver forces the same field to the
  standard IEEE 802.3 gap for the same reason. Fixed by writing
  `TCR_STANDARD_IFG` (`0b11 << 24`) instead of `0`. Found via real
  hardware bisection across three physical machines after ruling out the
  PIC/IRQ fixes above as the cause (the pre-AMD-work baseline commit had
  the identical bug -- it just had never been tested host-to-host on real
  NICs before). **Confirmed correct in principle (matches Linux's own
  constant/comment) but did not, on its own, resolve the reported
  symptom** -- see the next entry.
- **RTL8139 CR/RCR/TCR write order** -- still investigating the same
  "nothing a real fos host sends is ever received by anything" report
  after the interframe-gap fix above. Comparing against Linux's reference
  `rtl8139_hw_start` (`8139too.c`) turned up an explicit comment: "Must
  enable Tx/Rx before setting transfer thresholds!" -- it writes `CR`
  (`CmdRxEnb`/`CmdTxEnb`) *before* `RxConfig`/`TxConfig`, not after. This
  driver's `init` did the opposite: it wrote `RCR`/`TCR` (which carry
  FIFO/DMA threshold fields alongside their other bits) while Tx/Rx were
  still disabled, then enabled them afterward. QEMU's emulated RTL8139
  tolerates either order, which is why this was never caught by any
  QEMU-based testing this session. Fixed by moving the `CR` write (enable
  Tx/Rx) before the `RCR`/`TCR` writes. **Not yet confirmed against real
  hardware** -- if the AMD/P4/dev-host trio still can't exchange frames
  after this, the next thing to check is the exact hex bytes an external
  `tcpdump`/Wireshark capture sees leaving a fos host during `nic send`
  (a capture that's never actually been attempted yet in this
  investigation), since that would show directly whether the frame
  reaching the wire is well-formed, garbled, or simply never arrives at
  all -- narrowing between "chip mis-programmed" and "frame data itself
  is wrong" far more precisely than blind register-level guessing can.
- **RTL8139 receive ring could wedge permanently on one bad frame** --
  found while diagnosing "`tape load net` prints its `Receiving a
  N-byte tape from...` line and then never receives another frame again
  (and `nic discover` stops finding anyone either, in the same session)"
  on real hardware. `Rtl8139::receive` reads a 4-byte header out of the
  ring for the frame at the current read offset; if the card's own
  "receive OK" status bit was clear (a real CRC/alignment/runt error --
  much more likely to actually occur under a fast, back-to-back burst of
  frames like `tape`'s DATA chunks than the occasional single `chat`/
  `nic discover` frame), the old code returned `None` *without* advancing
  `rx_offset`/`CAPR` past that entry at all. Every later call then read
  the exact same bad header again, forever -- silently, with no error
  printed, since returning `None` is also the ordinary "nothing waiting
  right now" result. Because the ring offset lives in the one persistent
  `Rtl8139` instance rather than being reset per-command, one single bad
  frame during *any* network command permanently wedges receiving for
  the rest of the session, explaining why an unrelated command
  (`nic discover`) also stopped working once this happened. QEMU's
  virtual NIC essentially never reports a receive error, so this never
  surfaced in any QEMU-based testing. Fixed by always advancing past the
  ring entry using its header's length field whenever that length itself
  looks sane, regardless of the "receive OK" bit, and only skipping the
  copy-out (still returning `None` to the caller) when the frame itself
  was bad -- resynchronizing a *nonsensical* length field is still out of
  scope for this teaching driver (there is no sane byte count to advance
  past), but a bad-but-plausible header must never be left blocking the
  ring forever. **This fix alone was not sufficient** -- see the next
  entries for the actual root cause of the reported stall.
- **`tape load net` never answered discovery probes** -- found alongside
  the ring-wedge investigation above: a fos machine sitting in
  `tape load net` never replied to `CHAT_TAG_DISCOVER_REQUEST`, unlike
  `nic discover`/`chat`, because `tape_load_net`'s receive loop
  (`kernel/src/shell.rs`) only matched the `TAPE_TAG_*` tags and silently
  ignored everything else. This forced `tools/send_tape.py` to always be
  given an explicit `--mac` while a receiver waited in `tape load net`.
  Fixed by adding a `CHAT_TAG_DISCOVER_REQUEST` match arm mirroring
  `handle_incoming_chat_frame`'s (`remember_peer` + a unicast
  `CHAT_TAG_DISCOVER_REPLY`), so this host stays discoverable the whole
  time it waits for a tape, not just before/after a transfer.
- **RTL8139 Rx buffer/FIFO overflow -- the actual cause of the "stuck
  after `Receiving a N-byte tape from...`" reports** -- confirmed by
  reproducing against real hardware directly from a sandboxed dev
  environment that turned out to share the same LAN (raw `AF_PACKET`
  sockets need `CAP_NET_RAW`; granted via `setcap` on a dedicated
  `python3` copy rather than system-wide, see `tools/send_tape.py`'s
  usage). Sending `tapes/primes.bin` (~1 MB, 978 chunks) with
  `send_tape.py`'s default (no `--delay`) reliably reproduced the stall;
  the same tape with `--delay 0.003` (3 ms between chunks) reliably
  succeeded -- isolating the cause to the receive ring overflowing under
  an unpaced burst, something a real NIC sending at its own wire speed
  can do far more easily than QEMU's virtual NIC ever does (QEMU's own
  end-to-end test for this same tape, `tapes/test_primes_e2e.py`,
  needed the identical 3 ms pacing to avoid "Gap in tape data" failures
  even inside emulation). `Rtl8139::receive` never checked `ISR`'s
  `RXOVW`/`FOVW` bits at all, so once the ring overflowed, its read/write
  pointers were left desynchronized with nothing to resynchronize them.
  Three things changed in `kernel/src/rtl8139.rs` to address this:
  1. `RCR`'s Rx Buffer Length field (`RBLEN`, bits 12:11) was left at its
     reset default, the smallest ring size (8 KiB). Grown to 32 KiB
     (`RCR_RBLEN_32K`) -- **not** the largest option, 64 KiB: that was
     tried first and confirmed, in QEMU testing, to actually corrupt
     frame data, because at 64 KiB the ring already spans `CAPR`'s entire
     16-bit addressable range, leaving `RCR_CONFIG`'s `WRAP` bit (which
     works by letting a packet spill a little past the ring's nominal end
     into reserved slack space) with no well-defined place left to spill
     to. A bigger ring only reduces, and does not eliminate, how often an
     unpaced sender can overflow it.
  2. Added overflow detection and recovery (`recover_from_overflow`,
     called at the top of every `receive`): the first attempt -- disable
     `CR_RE`, reset `CAPR`/`rx_offset`, rewrite `RBSTART`/`RCR`, re-enable
     `CR_RE` -- looked sufficient in QEMU but, on real hardware, left the
     card completely unable to receive anything at all afterwards, in a
     way that outlasted leaving and re-entering `tape load net` and
     needed a full reboot to clear. Recovery was changed to a full
     software reset (the same `CR_RST` sequence `init` uses, factored out
     as `soft_reset`/`program_registers` so both share it) instead of a
     lighter register-level fixup; confirmed on real hardware that a
     paced (`--delay 0.003`) send now succeeds immediately after an
     unpaced send has overflowed the ring, with no reboot in between.
  3. `tape_load_net` now prints a periodic (~2s) progress line (bytes
     received so far vs. the declared total, plus
     `Rtl8139::overflow_count`) while a transfer is in flight, so a
     transfer that is still making progress is visibly distinguishable
     from one that has stalled, and a nonzero/growing overflow count
     points straight at this class of problem instead of leaving it a
     silent, undiagnosable hang.

  **Confirmed on real hardware**: an unpaced `send_tape.py` run against
  the fixed driver no longer hangs forever -- it now either completes, or
  cleanly aborts with a reported "Gap in tape data" once frames are
  actually lost to an overflow the bigger ring couldn't absorb, since
  this protocol has no retransmission (see `tape_load_net`'s own doc
  comment) -- and a paced (`--delay 0.003`, matching
  `test_primes_e2e.py`'s own pacing) run reliably succeeds, including
  immediately after a prior unpaced run has triggered the overflow path,
  without needing a reboot in between. **A polling-only driver with no
  interrupt and no hardware descriptor ring beyond one 32 KiB buffer
  fundamentally cannot keep up with a full wire-speed, completely unpaced
  burst from a real host NIC** -- this is an intentional teaching
  boundary (see "Deliberately deferred work" below), not a bug still to
  be chased further; pacing sends of any nontrivial tape remains expected
  practice, exactly as `send_tape.py`'s own `--delay` flag already
  documented before this investigation. `send_tape.py`'s `--delay`
  default was later changed from 0 to 0.003 (matching every value
  confirmed reliable above), since real hardware needs it essentially
  always, not just for unusually large tapes -- passing `--delay 0`
  explicitly still opts back into the old unpaced behavior.
- **RTL8139 receive ring could also wedge permanently on a nonsensical
  length field, with no recovery trigger at all -- suspected cause of a
  router NIC never rejoining fail-over after a cable reconnect** -- found
  while diagnosing a real-hardware report against `tapes/router`: after
  pulling and reconnecting one uplink's cable, the router's health check
  correctly failed over to the other uplink, but the reconnected NIC
  never recovered even after being physically reconnected for well over
  a minute -- no ARP reply from it was ever recognized again, so it never
  rejoined the fail-over rotation, and it also then failed a fresh DHCP
  attempt outright once it became the router's only uplink (after the
  other cable was pulled to test the reverse direction). The router's own
  health-check logic (`start_health_round`/`expire_health_timeouts`/
  `advance_uplink_health`, `tapes/router/src/main.rs`) was reviewed and
  confirmed to probe every uplink every cycle regardless of its current
  active/healthy status, using consistent tick units throughout -- so
  this is not a router-logic bug. `Rtl8139::receive` reads a 4-byte
  header out of the ring at the current read offset; the entry
  immediately above this one already fixed the case where the "receive
  OK" status bit is clear, but a *separate* branch -- reached when the
  header's own length field is nonsensical (`frame_len < 4` or greater
  than the ring size) -- still just returned `None` with nothing
  advanced and no fault flagged, exactly the same silent-forever-stuck
  shape the "receive OK" bug had. A link down/up cycle plausibly deposits
  link-training noise into the ring at its current offset, landing
  exactly there. Fixed the same way the two entries above already fixed
  their own trigger conditions: this branch now sets `recovery_pending`
  before returning `None`, so the very next `receive()` call's
  unconditional `recover()` performs the existing, already real-hardware-
  proven full soft-reset (not a lighter register fixup, per the overflow
  entry above) instead of leaving the ring wedged. **This fix alone did
  not resolve the reported symptom** -- see the next entry for the
  confirmed root cause.
- **RTL8139 never resumes receiving after a link recovery on its own --
  confirmed root cause of the router fail-back bug** -- real-hardware
  testing isolated the entry above's fix as insufficient by process of
  elimination: an all-3C905 router setup fails over *and back* correctly
  on a cable pull/replug, but swapping NIC 1 for an RTL8139 reproduces
  the original symptom exactly (fails over fine, never rejoins after the
  cable is reconnected, needs a reboot). The ring-wedge fix above only
  helps once garbled bytes actually land in the ring; with the link
  genuinely down, nothing lands there at all, so that fix's trigger
  condition never fires during a real cable pull. This driver never read
  the card's Media Status Register (`MSR`, offset `0x58`) at all, so it
  had no way to even notice a link recovery happened, let alone react to
  one. Linux's historic `8139too.c` documents the identical requirement:
  it polls this exact same `LinkB` bit and performs a full re-init
  (`rtl8139_hw_start`) on a down-to-up transition, rather than trusting
  the card to resume Rx by itself -- this chip does not. Fixed by adding
  `check_link_recovery`, called from both `receive` (the router/tape
  polling path) and `watchdog` (the shell's own path): it reads `MSR`
  each call, and on a down-to-up transition flags `recovery_pending` the
  same way the ring-wedge and overflow/TX-failure conditions already do,
  so the very next call performs the existing, already real-hardware-
  proven full soft-reset. **Pending final real-hardware confirmation**
  from the user repeating the cable-pull/reconnect sequence against the
  rebuilt image with an RTL8139 as the affected uplink.
- **`reboot soft` (see "reboot: a soft-reboot path..." above): not yet
  tested on real hardware, and several of its assumptions have never been
  exercised on real silicon before** -- every prior BIOS/A20/PIC code path
  in this project always ran exactly once per genuine power-on; a soft
  reboot re-enters `boot/stage2.S`'s `_start` without ever asserting the
  CPU's hardware RESET line, so this is the first time any of the
  following gets asked to run a *second* time against the same,
  never-power-cycled chipset state. None of this is confirmed broken --
  it is a predicted risk list to check first if `reboot soft` behaves
  differently on real hardware than in QEMU, the same way the historical
  A20/PIC/spurious-IRQ entries above were themselves predictable in
  hindsight from reading the code closely enough:
  - **A20 enabled a second time, with no reset in between.** `enable_a20`
    tries BIOS (`INT 15h`, `AX=2401h`), then the 8042 keyboard controller,
    then port `0x92`, re-verifying after each. Every one of those three
    mechanisms has now run once already, earlier in the very same power-on
    session -- toggling port `0x92`'s A20 gate bit (or pulsing the 8042's
    output port) a second time on real silicon it already succeeded on
    once is a genuinely new scenario for this codebase, not merely a
    repeat of a previously-tested path. If `reboot soft` fails specifically
    at "Enabling the A20 line..." on real hardware, suspect a chipset that
    tolerates this fine once per boot but not twice, and consider whether
    the *first* mechanism to succeed should be remembered and retried
    first on a soft reboot, rather than probing all three again from
    scratch.
  - **The kernel's own 100 Hz PIT is still running throughout the
    trampoline, not restored to BIOS's conventional ~18.2 Hz rate.** This
    is not new to soft reboot -- every ordinary post-boot BIOS disk call
    (`disk::call_trampoline`) already runs with the same faster PIT rate
    for the same reason -- but `load_kernel`'s BIOS EDD reads inside
    `soft_reboot_real`'s re-entered `_start` are the first place this
    project depends on BIOS's own disk-timeout logic (which polls the BDA
    tick counter IRQ0 advances) working correctly *immediately at boot*,
    on potentially slower or flakier real USB boot media, rather than
    mid-session against an already-proven-working disk path. A real BIOS
    whose timeout logic reads that counter as advancing ~5.5x faster than
    it expects could time out a disk read prematurely in a way QEMU's own
    BIOS emulation may not reproduce.
  - **Sibling bugs of the `%bp`-vs-`%ebp` E820 bug, elsewhere in
    `stage2.S`/`netrom.S`/`disk.rs`.** That bug existed for a long time
    unnoticed because a cold BIOS POST happens to leave a register's upper
    bits zero by luck; a soft reboot is the first code path that leaves
    genuinely different (Rust-kernel-derived) garbage in registers a cold
    boot never would. Any other "clear only the low 16 bits of a 32-bit
    register, then later compare/store the full 32 bits" pattern in this
    real-mode code is a plausible sibling, worth a deliberate audit pass
    rather than waiting to be tripped over the same way this one was.
  - **NIC/PCI device state is never power-cycled** (`reboot soft` never
    asserts PCIRST#, unlike a real hardware reset). A device's own
    re-`init()` should fully reprogram it regardless of prior state, but
    an interrupt latched, or a DMA transfer in flight, at the exact
    instant of the jump has never been exercised on real silicon this way
    before; if a NIC ever comes up in a stuck or half-configured state
    specifically after a soft reboot (but not after `reboot hard`), start
    here.
  - **Real BIOS/USB-boot-media tolerance of a second, POST-less round of
    EDD disk calls.** Every BIOS disk call this project has ever made
    before now happened exactly once per genuine power-on cycle (BIOS's
    own POST always ran first). `reboot soft` is the first time BIOS's
    disk services get invoked cold a second time without an intervening
    reset -- plausible territory for a real BIOS/USB-controller
    combination to hold some one-time-per-power-on internal state that
    does not expect this, even though nothing in this project's own code
    changes between the two calls.
  - Minor/cosmetic, not expected to cause a real failure: a stray PS/2
    scancode or a partially transmitted UART byte from the very keystroke
    that submitted `reboot soft` is not drained before the jump, so it
    could resurface oddly in the freshly-booted kernel's input queue.

If real hardware ever produces a boot loop, a screen that fails to update,
or output that only appears over one of VGA/serial, suspect this class of
gap first -- QEMU's emulated devices are close to real hardware but not a
perfect model of the more idiosyncratic chipset-level behavior above.

