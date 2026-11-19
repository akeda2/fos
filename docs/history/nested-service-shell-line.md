# A shell line run inside a service (the test-kakelfix flake), 2026-10

`make test-kakelfix` failed about one run in twelve: kakelfix drew its screen and followed the mouse, but ignored every key,
and the `play` service feeding loop0 went silent. Nothing printed an error.

## How it was found

The failing state was caught with QEMU monitor memory dumps and a gdb probe from a looping copy of the test:

- The x86 tape input ring's head grew (9, 10, 11) as keys arrived, but its tail stayed 0: nobody called `tape_key`.
- Service 1 (`play`) had a saved stack pointer inside the foreground tape's 1 MiB stack, not its own.
- `CURRENT` read 1 (the service) while the CPU ran kakelfix code on the foreground tape's stack.

## Cause

`term::feed` runs a finished shell line synchronously, in whatever context is draining the NICs. With `play` running as a
service and the shell idle, the service's `audio_write` reached `term::tape_poll` -> `tape_service` -> `drain_nics_to_fifo`
-> `handle_frame` -> `feed('\n')`, which ran `run kakelfix` nested inside service 1. kakelfix's `read_key` then saw
`in_service()` true and took the service branch (no keys for services), and `play` stayed frozen under it until kakelfix
ended. The mouse kept working because touch input is not context-checked. The race window was the test's
`run kakelfix` line arriving while `play`, not the shell loop, drained the NIC.

A related bug: `tape_service` moved local keyboard keys into the tape ring even when no tape ran, so a key typed at the shell
while a service drained the keyboard was lost. The Pi had the same shape: `netservice::session_frame` is also called from a
tape's `nic_receive` and fed `shell::feed_byte` directly.

## Fix

Shell bytes received inside a service are queued (`lineedit::defer`) and fed by the shell's own loop; a tape that starts adopts
them as input. `tape::run` refuses to start from a service on both ports, and `tape_service` takes keyboard keys only in tape
mode. Afterwards the embedded part of the test passed 32 runs out of 32 (four in parallel; two failures in the mf half were
load-related: a 3 s mouse window, and the shell's "is process N" line interleaved with the tape's output).

## Tools that came out of it

What took memory dumps to see is now one command: `fosadm sysrq HOST` prints each context's stack and call counts, the input
ring and the loopback owners while the tape keeps running, and `trace` keeps the recent kernel events (`docs/design.md`,
"Debug tools").
