---
name: fos-debug
description: Debugs Fosfor OS (fos) kernels, tapes and tests - SysRq state dumps, the trace ring, crash records and the watchdog, mf NMI CPU capture with backtraces and lock owners, shell diagnostics, QEMU monitor and gdb probes of a hung guest, symbolising a RIP, and chasing flaky end-to-end tests. Use when a machine or tape hangs, crashes or misbehaves, when a test fails or flakes, or when asked how to debug fos.
---

# Debugging fos

Run from the repo root (`/home/akeda/dev/fos`). Look first, change later: gather evidence with the tools below
before editing code. Background: `docs/design.md` "Debug tools", `docs/config.md` (the `debug` file), and
`docs/history/` (postmortems; `nested-service-shell-line.md` is a worked example of this whole method).

## 1. Which kernel, which context

- Embedded kernel (`kernel/`, x86 and Pi 4): **cooperative**. Context 0 is the shell or the foreground tape; context n
  is service n-1 (`run NAME ... &N`). Switches happen only inside tape calls (`should_abort`, `read_key`,
  `wait_for_event`, `audio_*`, 200 us slices) and in the shell loop. Any tape call may drain the NICs and keyboard, so
  code reached from `handle_frame` runs in *whatever context made the call*. Ask "which context runs this?" first.
- Server kernel (`kernel-mf/`): preemptive, SMP, ring 3, threads. Same commands, SMP back end: NMI capture of
  every CPU, backtraces, lock owners, answers SysRq even when stuck (section 5).
- `version`/`about` says which kernel a machine runs ("server kernel (mf)").

## 2. A machine that seems stuck (embedded kernels)

1. **SysRq**, without stopping the tape: `python3 tools/fosadm.py --iface IFACE sysrq HOST`
   (or `tools/term_net.py --iface IFACE --mac N --sysrq`; `sysrq` at the console prints the same). It shows:
   - each context's state, saved stack pointer and stack range: a context running on *another's* stack is a nesting
     bug;
   - per-context call counts (`key abort wait ain aout touch blit sw`): a tape that never calls `key` gets no keys, a
     service whose `aout` stopped growing is frozen;
   - the foreground tape's name and its input ring's head/tail (head moving, tail not: nobody reads keys);
   - who holds each loopback end, and the reads/writes refused for being in the wrong context;
   - the last 16 trace events.
   It answers only when something drains the NICs (shell loop or a tape call). No answer means a loop without tape
   calls or interrupts off: use the serial console and the watchdog.
2. **`trace`** (`trace [N|all]`, `trace on|off sched|input|tape|audio|net|all`, `trace clear`): the last 1024 kernel
   events with time and context. `sched` (every switch) is off by default; turn it on only briefly. Add an event with
   `crate::trace::event(Category::X, "name", a, b)` (static name, two numbers) when chasing a new problem.
3. **`ps`**: services, owned NICs/audio devices, call counts.
4. Other shell diagnostics: `dmesg` (boot log), `nic list` (counters, MPC/RNBC), `nic sniff [N]`, `nic peers`,
   `usb`, `usb debug` (PORTSC and port events), `audio` / `audio help`, `irqstat`, `pci [read ...]`, `memmap`,
   `cpuinfo`, `mmio r32 ADDR`, `hexdump ADDR LEN`, `sector diskN LBA`, `fsck`, `debug slots`.
5. A lone ESC through term net stops a foreground tape:
   `(printf '\x1b'; sleep 2) | ~/.local/bin/python3-netraw tools/term_net.py --iface IFACE --mac N --interactive`.

## 3. Crashes and hangs that reset the machine

- The `debug` file on the boot volume (`docs/config.md`): `reboot-on-crash`, `reboot-delay N`, `watchdog [N]`,
  `kernel-update`. The crash record lives in RAM at `0x5000` and is printed by the **next boot** ("Previous boot
  crashed": vector, error code, RIP, RSP, CR2, registers). A watchdog reset reports vector 240 with the RIP it caught.
- `debug crash` / `debug hang` test the path. Five crashes before the boot milestone stop the reboot loop.
- A pushed kernel that comes back on the *other* slot was reverted by A/B fallback: `debug slots`, `dmesg`.
- **Symbolise a RIP.** The kernel is linked at 0 and placed by the loader; the serial log shows `Kernel at 0xBASE`.
  ```
  K=build/cargo-target/x86_64-fos-kernel/release/fosfor-kernel   # Pi: aarch64-unknown-none-softfloat/release/
  addr2line -f -C -e $K $(printf '%#x' $((RIP - BASE)))
  nm -C $K | grep SYMBOL        # a static's offset; its address is BASE + offset
  ```
  Only valid for the exact build that crashed; a rebuild changes the offsets.

## 4. QEMU: catching a failing state

For bugs only a test reproduces. Copy the test to a temporary `tools/_NAME.py` (delete it afterwards) and loop it.
- Parallel runs: `FOS_TEST_SLOT=N` moves the multicast/TCP ports (`tools/testslot.py`); each `Machine` has its own
  disk copy. This workstation has 4 cores; more than about 4 QEMUs at once cause timing failures of their own.
- QEMU monitor (`machine.monitor = Path(tmp) / "monitor"`, `test_boot.monitor(path, cmd)`): `info registers`,
  `xp /8gx ADDR` (kernel addresses are physical, identity-mapped), `screendump`. Read statics at `BASE + nm offset`
  for scheduler state (`CURRENT`, `MAIN_RSP`, `SERVICES`), input rings and queues.
- gdb: add `-s` (gdbstub on :1234) to the QEMU command line, then
  `gdb -batch -ex 'target remote :1234' -ex "add-symbol-file $K -o BASE" -ex bt -ex 'info registers'`.
  A stopped guest breaks the test's timing: probe, then continue.
- On failure keep the serial text (`machine.text()`), the screendump and the dumps; compare a passing and a failing run.
- Simpler than memory dumps now: send SysRq from the test (`machine.wire.send(machine.guest, 0x24)`, wait for
  `--- end sysrq ---`), as `tools/test_kakelfix.py` does.

## 5. Server kernel (mf)

Same front end: `fosadm sysrq HOST`, `term_net.py --sysrq`, `sysrq` and `trace` in the shell (`make test-mf-sysrq`).
Code: `kernel-mf/src/sysrq.rs` (dump, rescue), `debugcpu.rs` (CPU capture, backtraces, call records), `sync.rs`
(`CPU_LOCKS`), `net.rs` (`rescue_drain`, `write_sessions`), `sched.rs` (`write_threads`).

**Reading the dump**, top to bottom:
- Header: uptime and the answering CPU. "service thread last went round N ms ago" (slow), "service thread silent for N
  ms: answered from the timer interrupt" (stuck, interrupts on somewhere), "the clock (cpu0's timer) stopped N ms ago"
  (CPU 0 hung with interrupts off: the tick count is CPU 0's alone).
- One block per CPU, stopped by an NMI (x86, IST 2) or SGI 1 (aarch64):
  `cpuN: thread T (name), kernel at k+OFF | in its process, interrupts on|OFF`, then `bt:` (frame-pointer walk,
  kernel frames only), `holds the lock it took at FILE:LINE`, and `WAITS N ms for a lock at X` with
  `held by cpuM, taken at Y` (a deadlock reads as two CPUs each waiting for the other's lock).
  - "interrupts OFF" on a CPU other than the dumping one: a deadhang, or a long spin under a SpinLock.
  - "no answer to the capture": the CPU did not take the NMI/SGI (aarch64 with IRQs masked; x86: hard-wedged).
  - The dumping CPU shows itself inside `capture_all`/`write_dump`: ignore its frames.
- `threads (N switches)`: id, name, state, `cpuN` if running, `pid`, futex/wait `key` if blocked, CPU ticks, and
  ` in NAME N ms (C calls)` (a tape call in progress: a call stuck for seconds is the lead) or ` last NAME`.
  If the scheduler lock is held, the line says by which CPU and where, instead of the list.
- `memory:` (or "frame allocator locked"/"heap locked": someone holds it; look at the CPU blocks), sessions, then the
  last 16 trace events with `cpu/tthread`.

**Symbolise** `k+0xOFF` (offset into the image, independent of where the loader put it; only for that exact build):
```
addr2line -f -e build/cargo-target/x86_64-fos-kernel/release/fosfor-kernel-mf 0xOFF   # x86
addr2line -f -e build/cargo-target/aarch64-unknown-none-softfloat/release/fosfor-kernel-mf 0xOFF  # Pi 4
```
Names are mangled (`_RNv...5shell3run` = `shell::run`); `rustfilt` or `c++filt` is not needed to read them.

**Crash records** (after a reset, "Previous boot crashed"): also `backtrace:` of the crashed CPU and `cpuN: thread T
at k+OFF` for each other CPU (x86: page `0x5000` + 512, magic `FOSCRSH2`; aarch64: appended to the crash file text).
The deadlock detector (`sync.rs`) prints the owner CPU and both call sites before it resets.

**trace on mf**: events carry CPU and thread. On by default: `tape spawn/exit`, `futex-wait/wake`, `net sysrq`;
`trace on sched` adds `sched switch` (current, next): thousands a second, so turn it on briefly, reproduce, `trace 200`.
Add an event with `kernel_core::trace::event(Category::X, "name", a, b)`. Never from an NMI handler, `sync.rs` or the
rescue path: on x86 without TSC `micros` takes a lock.

**From a test**: a `test_mf_sessions.Client` sends tag 0x24 and waits for `--- end sysrq ---` (see
`tools/test_mf_sysrq.py` `sysrq()`); works while the guest's service thread hangs. To hang on purpose:
`debug hang now` (service thread spins, interrupts on), `debug deadhang now` (interrupts off), `debug deadlock now`.

**When adding code**: a new lock is a `SpinLock` (it records itself); dump-path code must use `try_lock` and say what
is held rather than wait (`pmm::try_stats`, `heap::try_stats`, `sched::write_threads`). Nothing on the NMI path may
touch `percpu::this()` (GS may be the user's) or take a lock.

Limits: a one-CPU mf machine hung with interrupts off cannot answer (use the serial console and hardware watchdog);
whether a real board delivers the NMI to a CPU with interrupts off is only checked in QEMU so far.
Also `ps`, `threads`, `top [S] [once]`, `dmesg`, `debug slots|revert|crash now|hang now|deadhang now|deadlock now`.

## 6. Tests that fail or flake

- `make test-NAME` for one; `python3 tools/run_tests.py JOBS test-A test-B ...` for several (a failure is retried
  alone; logs in `build/test-logs/NAME.failed.log`). "PASS (alone)" means load-sensitive, not necessarily fine.
- Full suite: on landsbro with `gb -of fos`, not locally.
- Before calling something a flake, measure its rate (20-30 runs) and look for a real race: the kakelfix "flake" was a
  kernel bug. Known load-sensitive: `test-mf-boot`, `test-bootcode-uefi`, `test-nettapes`, `test-audio-net`,
  `test-nettest`.
- Wait for the prompt before typing, use bounded waits, and remember that `term net` drops trailing NUL bytes.

## 7. Real hardware

- Raw frames need `~/.local/bin/python3-netraw` (a human grants CAP_NET_RAW once; never do it yourself). Always pass
  `--iface` (the wired NIC, `enp0s31f6` on the maintainer's workstation).
- Serial console: `fosadm --serial PORT run serial "dmesg"`; the only view of a machine that never drains its NICs.
- QEMU accepts orderings real chips do not, and after a reset even a register read can hang: every wait is bounded.
  Say in the report what only the hardware can confirm.
- A console line costs about 10 ms on real hardware: never print inside an audio send loop while debugging; collect
  values and print them afterwards (or use `trace`).

## Pitfalls already paid for

- A shell line must never run inside a service (`lineedit::defer`); a service must not start a foreground tape.
- `read_key` and `should_abort` drain the same input: a tape uses one or the other.
- Loopback ends belong to the context that started them; calls from another context return 0 (see the refused counts).
- Rust 2024 denies references to `static mut`: copy into locals (`let (a, b) = unsafe { (A, B) };`) before printing.
- Diagnostic output must fit 79 columns (BIOS text mode).
