# A second, larger kernel (idea, not started)

Question: can fos have two kernels, the current small one for embedded machines and a larger one (SMP, preemptive multitasking,
real memory management) that supports everything the small one does? Yes. This file is the plan and the trade-offs; nothing is built.

## Why it is feasible

- **The tape ABI is the contract.** Tapes only see `TapeApi` (`abi/src/tape.rs`, append-only). A kernel that fills in the same table runs
  every tape unchanged; "supports everything the embedded kernel supports" means implementing that table and the shell commands.
- **The loader already hands over a neutral `BootInfo`** (UEFI and BIOS, version 5), and the A/B slot machinery loads whichever ELF is in
  the slot. A second kernel is a second image, not a second boot path.
- **Services (`run NAME &N`) are already a concurrency concept**: a tape that keeps running while the shell is usable. A scheduler
  generalises that.

## What is hard (the embedded kernel's assumptions)

| Assumption today | What the big kernel needs |
| --- | --- |
| One CPU, drivers use `static mut` with no locks | Locks (spinlocks, IRQ-safe), per-CPU data, TLB shootdown, memory ordering reviewed in every driver |
| Polling loops with bounded waits, `idle()` in tapes | Blocking waits and wake-ups; `idle()` becomes a yield; interrupt-driven completion in the drivers |
| Identity-mapped RAM, tapes run in the kernel's address space | Per-process address spaces and, eventually, ring 3 with a syscall layer behind the `TapeApi` function pointers (pointer validation, copy in/out) |
| One physical allocator, `alloc` bump-style for tapes | A VM subsystem: page tables per process, demand paging optional, a kernel heap with free, guard pages, NX everywhere |
| `tape.rs` runs one tape in the foreground (plus services) | A scheduler with threads, timer tick, kernel stacks, a process table, `kill` that really kills |
| Boot CPU only (ACPI MADT is parsed, APs are not started) | INIT-SIPI-SIPI trampoline in low memory (not over `0x5000`, the crash record), per-AP stacks, GDT/IDT/APIC per CPU |
| Hardware access straight from tapes (`gfx` framebuffer, MMIO helpers, port I/O) | Either keep those tapes in ring 0 or give them mapped, checked windows |

## Structure options

1. **One source tree, two configurations (recommended start).** Split `kernel/` into a `kernel-core` library (drivers, FOSFS/FAT, net
   frames, tape ABI glue, shell commands) behind a small "OS services" trait (DMA memory, MMIO mapping, IRQ registration, time, wait/wake),
   plus two thin cores: `fos-embedded` (today's behaviour: no locks beyond IRQ masking, polling) and `fos-smp` (locks, scheduler, VM).
   Drivers are written once against the services trait; the embedded core makes locks no-ops. Cargo features can express the same
   thing at lower cost if the split is too big a first step.
2. **Two independent kernels.** Simplest to start, but every driver fix and every new tape-ABI call lands twice; they drift. Only
   worth it if the big kernel's driver model ends up fundamentally different (async, user-space drivers).
3. **Grow the existing kernel with features.** Add `smp`/`sched`/`vm` features; the embedded build compiles them out. Cheapest, but
   the features touch everything (locking in every driver), so it ends up as option 1 anyway.

## Cross-cutting points

- **Flavour marker.** The slot metadata or the kernel ELF should say which flavour it is (a note section like `.fos_help`) so KUP,
  `fosadm` and the A/B loader never put the embedded kernel on a host chosen for the big one or the reverse. `fosadm` needs a
  per-host flavour (and `status` must compare against the matching build); `make image` builds both, e.g. `build/fosfor-smp/`.
- **ABI.** Behaviour that changes meaning needs new calls, never new meanings for old ones (the append-only rule): e.g. a tape that
  blocks in `idle()` must still behave. Anything that only makes sense with threads (`thread_spawn`, futex-like waits) is a new call that
  the embedded kernel answers with "not supported" (check `version` and a capability bit).
- **aarch64.** The Pi 4 port is a third axis (arch x flavour). SMP on the Pi 4 means PSCI CPU_ON and GIC per-CPU setup; plan the
  services trait so it does not assume x86.
- **Tapes with timing rules** (audio-net, nettest: no printing between `audio_start` and the loop) rely on being the only thing on the
  CPU. On the big kernel they want a pinned, non-preempted thread (CPU affinity or an isolated core).
- **Tests.** QEMU `-smp N` for everything; races need stress tests with repeated runs, not single green runs. The existing `test-*`
  suite is the compatibility check: every test must pass on both flavours.

## A staged path (each stage is shippable)

1. Extract the drivers and FOSFS into `kernel-core` behind an OS-services trait; embedded kernel unchanged and all tests green.
2. Lock audit: replace `static mut` driver state by explicit per-device state behind IRQ-safe locks (no-ops on the embedded core).
3. SMP bring-up: start the APs, per-CPU GDT/IDT/APIC, APs idle; one CPU still runs everything.
4. Kernel threads and a scheduler (timer preemption, blocking waits); the shell and each tape become threads in the kernel
   address space (still ring 0). Interrupt-driven drivers replace polling one class at a time (NIC, disk, USB).
5. A real kernel heap and VM (free, per-process page tables); tapes get their own address space but stay ring 0.
6. Ring 3 for tapes with a syscall shim behind `TapeApi`; the shim validates every pointer. Tapes that need raw hardware stay privileged.
7. Optional extras: NUMA-aware allocation, CPU affinity for realtime tapes, a proper process/IPC model, `tapesync` flavour awareness.

## Cost and risk

- Stage 1-2 are the bulk of the risk: they touch every driver and have to keep the real-hardware machines working (e1000, xHCI,
  HDA, RTL8168 all have known timing quirks; `docs/history/` has the write-ups).
- A realistic order of magnitude: stage 1-3 weeks of focused work, stage 4 a few more, stages 5-6 several weeks (VM and user mode are
  where hobby kernels stall). Only real hardware shows interrupt and SMP bugs that QEMU hides; QEMU accepts orderings real chips do not.

## Answers (2026-11-01) and what they change

- **"Larger system" is a role, not a size**: a machine that runs a file, web or print server, or a batch queue. 1 to 36 cores (2 x 18
  sockets); `rack` or `skylake` is the first candidate. So the target is server workloads on up to two sockets: NUMA-aware allocation
  moves from "optional extra" to a real stage-5 item, and the scheduler must scale to 36 CPUs (per-CPU run queues, no global lock).
- **Isolation is required.** Ring 3 and per-process address spaces (stages 5-6) are mandatory, not optional: tapes on the big kernel
  run unprivileged behind the syscall shim, with pointer validation, resource limits (memory, open files, CPU time) and a kill that
  always works. Tapes that need raw hardware (`gfx`, MMIO, port I/O) either get a checked, mapped window or stay on the embedded kernel.
  Network and storage services should be separate processes talking through the kernel, so a crashed httpd cannot take the machine down.
- **Latency-sensitive tapes stay on the embedded kernel** (audio-net, recorder, nettest, anything with the no-printing-in-the-loop
  rule). On the big kernel only non-latency tapes are supported; core pinning is an option for them, not a promise of realtime.
  Consequence: the big kernel does not have to meet the audio timing rules, which removes the hardest realtime requirement.
- **First applications**: file server (NETFS/SMB-style over the existing FOSFS/FAT drivers), web server (`httpd` as a real
  multi-connection process: keep-alive, many clients, TLS later), print server, and a **shared batch queue for transcoding jobs**.
  The batch queue needs: a job table and spool on disk, worker processes (one per core or a limit), CPU-bound scheduling with priorities
  or nice levels, per-thread FP/SIMD state (tapes are hard-float: the kernel enables SSE/AVX2 per `fp_level`, so context switches save
  and restore XSAVE state), progress and cancel from the shell, and restart of jobs after a reboot. `opus-codec` and the APE decoder
  are the first transcoding workloads already in the tree.
- **Order of work this suggests**: stages 1-4 as planned, then VM and ring 3 (5-6) *before* any server role, since isolation is a
  requirement; a first useful milestone is "httpd as an unprivileged process on a multi-core `skylake`", then the batch queue.
- **Still open**: file-server protocol (extend NETFS, or SMB/NFS for other hosts?); print server details (which printers, which
  protocol, e.g. raw port 9100 or IPP); whether the batch queue is a shell feature or a tape/service; authentication for network
  services (none exists today; `deny-term-net` and `password` are the only controls); how the big kernel is installed (a stick, KUP
  with the flavour marker).

## Stage 1 survey (2026-11-01, branch `kernel-core`)

Measured on `main` at `2396171`; no code moved.

**Shape today.** `kernel/` is one `no_std` binary crate: 90 modules, about 40,000 lines. `main.rs` splices in one entry file per
architecture with `include!` (`main_x86_64.rs`, `main_aarch64.rs`), which declares the modules and re-exports the arch layer at the
crate root, so every driver says `crate::cpu`, `crate::paging`, `crate::memory`, `crate::timer`, `crate::msi`, `crate::println!`.
Hardware-free logic is already in crates with host tests: `fat`, `shell`, `hid`, `tape-sync`.

**The services surface is small and already exists as module paths.** Counting calls from drivers:
- memory: `free` 70, `allocate_contiguous` 61, `allocate` 6, `dma_pool`, `reserve`, `stats`.
- paging: `map_device` 34, `is_mapped` 8, `map_framebuffer`, `map_uncached`, `set_executable`.
- time: `timer::ticks` 119, `micros` 83, `wait_ticks` 40, `busy_wait_ms` 25.
- CPU: port I/O (`inb/outb/inl/outl/inw/outw`, about 80), `interrupts_save_and_disable`/`restore` (14 each), MSRs, `cpuid`.
- interrupts: `msi::allocate`, `msi::attach_intx`, `msi::claim`, `msi::release`, and a few `ioapic` routing calls.
That is about 30 functions: the OS-services trait is mostly a matter of naming what is already there.

**Concurrency today.** `tasks.rs` is cooperative (one CPU; a context yields only inside a tape call), so "the kernel needs no locks".
Driver state shared with interrupt handlers uses atomics and `interrupts_save_and_disable`. There are about 150 `static mut` items,
concentrated in `term` (28), `framebuffer` (19), `tape` and `tape_aarch64` (11 each), `netfs` (11), `debug` (10), `xhci` and `usb_hid` (8).
This is the stage-2 lock audit's workload; NIC drivers (e1000 23 atomics) are already closest to lock-free.

**Groups (lines).** USB stack about 6,500 (xhci 1474, ehci 1212, ohci 577, usb_audio 1635, usb_hid 828, others); audio about 4,000
(hda 1742, usb_audio, audio 609); NICs about 3,700 (e1000 1328, rtl8168 671, 3com 624, genet 600, rtl8139 510); storage about 2,000
(ahci, nvme, usb_storage, sdhci, virtio_blk, block); filesystems about 2,700 (fosfs 1241, fatvol, gpt, mounts, tape_fs); shell, tape
glue and terminal about 7,000 (term 1851, tape 1041, tape_aarch64 910, shellcmds, shell); Pi-only about 2,500 (i2c, gpio, i2s_pi, mailbox).

**What blocks a clean split (drivers reaching upward).** Nearly every driver calls `println!` (a services item: a `log` sink).
Real upward couplings: `usb_hid`/`usb_kbd`/`usb_serial`/`usb_audio`/`usb_midi`/`usb_pointer` reference each other and `netfs`,
`midishell`, `framebuffer`; `uart` -> `usb_serial`; `keyboard` -> `usb_pointer`; `nicshell` -> `netfs`; `netfs` -> `fosfs`,
`tape_fs`, `config`, `netservice`. The USB class drivers need an explicit "device appeared / class driver" interface before they can
leave the kernel crate. `tape.rs`, `tasks.rs`, `term.rs`, `shell*.rs` are core-specific (the MF core replaces the tape glue with a syscall shim).

**Recommended shape for stage 1.**
- 1a, in place, one crate: add a `hal` module (the ~30 functions plus a log sink) and make drivers use only `crate::hal::...` for
  arch, memory, time, interrupts and logging. Mechanical, a few commits, each building and testing on x86 and aarch64, no behaviour change.
  Cut the upward couplings listed above with small interfaces (a registry for USB class drivers; `netfs` and shell commands call
  down, drivers do not call up).
- 1b, lift into a library crate `kernel-core` (drivers, fosfs, fatvol, gpt, net frames) depending on a `fos-hal` crate; the `hal` module
  becomes a trait implemented by the embedded core (`fosfor-kernel`, unchanged name and outputs). Dispatch is a `&'static dyn Platform`
  set at boot for slow-path calls; hot ones (port I/O, `micros`) stay inline via generic or `#[inline]` functions in the platform crate.
  Move order: filesystems (host-testable) -> NICs -> storage -> USB -> audio. Shell, tape glue, `term`, `tasks` stay in the core.
- Size and speed: kernel is `opt-level = "z"` with LTO, so indirection is checked by comparing image size before and after.

**Risks.** Path moves hide from text search (split spellings); every step needs the aarch64 build and, at the end of 1b, a full
`make test-all` on landsbro. Real-hardware checks for anything that touches IRQ routing or DMA (`skylake`, `amd1`, laptop, `piano`).
Rough size: 1a about a week of focused work, 1b about two.

## Stage 1a status (2026-11-01, branch `kernel-core`)

Done, one commit each, all with the x86 and aarch64 kernels building and the matching `make test-NAME` green:
- `kernel/src/hal.rs`: the platform surface. Re-exports `cpu`, `paging`, `timer`, `msi`, `pci_config`; adds `interrupt_target`,
  `gsi_range` (x86), `is_pi4`, `register_irq`, `irq_ticks` (aarch64) and `print`. `tools/check_hal.py` (run by `make test`) fails
  when a driver names an arch-private module (`apic`, `ioapic`, `gic`, `serial`, `pl011`, ...); its allow-list is empty.
- Logging: `println!` -> `hal::print` -> the entry file's `CONSOLE_SINKS` (the core decides where text goes).
- `usb_class.rs`: one table of USB class drivers (release, background event, departed hooks); `usb_hid`/`usb_kbd` no longer name them.
  Attaching a new device still names each class in the enumerators (descriptor handling differs per class).
- `serialports.rs`: the UART and USB-serial numbering mux; `uart` no longer calls `usb_serial`.
- `keyboard` -> `usb_pointer`: a registered mouse sink (`keyboard::set_mouse_sink`).
- Size: x86 kernel 1,161,480 -> 1,167,120 bytes (+0.5%) against the pre-stage tree.

Left for 1b: `netfs` -> `fosfs`/`tape_fs`/`config`/`netservice` and `nicshell` -> `netfs` are core-side code and move with the shell;
the USB attach path; turning `hal` into a trait with the embedded core as its implementation; the library crate itself. Not covered
by any test: USB serial adapters, the Pi 4 console and GIC paths (hardware only).

## Stage 1b status (branch `kernel-core-1b`)

- `crates/fos-hal`: the `Platform` trait (block read/write, wall clock, console print), installed once by `kernel/src/platform.rs`
  at the start of `kmain`. Slow paths only; port I/O and `micros` stay inline in the platform.
- `crates/kernel-core`: modules that need nothing but `fos_hal`. First slice: `fosfs` (moved unchanged; the kernel re-exports it
  as `crate::fosfs`). Host tests run it on a RAM disk (`make test`).
- Size: x86 kernel 1,167,120 -> 1,168,008 bytes. Verified: `make test`, `test-fosfs4`, `test-v4-boot`, `test-mounts`, aarch64 build.
- Second slice: `gpt` and `lineedit` (the `Platform` gained `block_sectors`, `timer_counter`, `timer_ticks`; `gpt` has a host test).
  x86 kernel 1,168,328 bytes. Verified: `make test`, `test-format`, `test-grow`, `test-install`, `test-fosfs4`, `test-v4-boot`,
  `test-serial-shell` (line editor), aarch64 build.
- Third slice: `fatvol` (the `Platform` gained `boot_disk`, `timer_micros`; `fat::Time` now owns the date arithmetic that
  `clock.rs` used to have, so the clock and `fatvol` share it). Host test formats and mounts exFAT on a RAM disk; the host tests
  run one at a time because the kernel code keeps scratch tables in statics. x86 kernel 1,169,456 bytes. Verified: `make test`,
  `test-clock`, `test-config-fat`, `test-fat-shell`, `test-format-exfat`, `test-format`, `test-fosfs4`, `test-mounts`,
  `test-v4-boot`, aarch64 build.
- Fourth slice: `mounts` (the table, `lookup`, `fstab` parsing). The two kernel-only questions stay in a thin
  `kernel/src/mounts.rs`: `load()` passes the boot volume to `load_from`, `absent()` passes a "is it present" test to
  `absent_with`. Host test parses an `fstab` and checks longest-mount-point lookup. x86 kernel 1,169,584 bytes. Verified:
  `make test`, `test-mounts`, `test-v4-boot`, `test-config`, `test-config-fat`, `test-fosfs4`, aarch64 build.
- Fifth slice: the volume registry (`volumes`: boot volume, `volume_on`, `volume_labelled`, `volume_with_file`) and `tape_fs`
  (the tape file-system calls). `storage.rs` keeps the driver bring-up and re-exports the registry; `Platform` gained
  `block_info` (size and model) and `block_count`. Host test finds volumes by label. x86 kernel 1,169,944 bytes. Verified:
  `make test`, `test-mounts`, `test-fat-shell`, `test-fs-read-at`, `test-recorder-fat`, `test-config-fat`, `test-format-exfat`,
  `test-fosfs4`, `test-v4-boot`, `test-grow`, aarch64 build.
- Sixth slice: `pci` and the x86 NIC drivers (`e1000`, `rtl8168`, `rtl8139`, `threecom`, `nic`). The `Platform` gained physical
  memory, device mapping, PCI config, MSI/INTx, port I/O, interrupt masking and APIC routing calls; all are setup paths, the
  packet path stays MMIO. aarch64 keeps `genet` and gets stubs for the INTx calls. x86 kernel 1,172,088 bytes. Verified:
  `make test`, `test-nic`, `test-rtl8139`, `test-hda`, `test-xhci-abort`, `test-mounts`, `test-fosfs4`, aarch64 build.
  Real queue, DMA and interrupt behaviour on silicon only shows on hardware.
- Seventh slice: `ahci` and `nvme`. The `Platform` gained `free_frames` and `watchdog_pet` (the drivers' wait loops pet the
  watchdog). x86 kernel 1,172,144 bytes. Verified: `make test`, `test-disks`, `test-usb-boot`, `test-v4-boot`, `test-format`,
  `test-grow`, `test-mounts`, `test-debug-mode`, aarch64 build. Real controller timing on hardware only.
- Eighth slice: the USB host side: `xhci`, `ehci` (x86; aarch64 keeps the stub), `usb_hub`, `usb_storage` and `usb_class`. The
  controllers know no class driver by name: `usb_class` is now a registry (`register`, `set_host`) and the embedded kernel
  installs audio, MIDI, pointer, serial and the enumerator (`usb_hid`, or `usb_kbd` on the Pi) from `kernel/src/usb_register.rs`.
  The server kernel is a superset, so it registers the same drivers; the registry is the seam that lets the class drivers
  follow into `kernel-core` later, not a way to leave them out. The `Platform` gained `msi_message` (core programs
  the device's MSI/MSI-X capability itself) and `block_register_usb`/`block_usb_disk`. x86 kernel 1,177,776 bytes (the
  hook tables and calls across the crate boundary cost ~5 KiB). Verified: `make test`, `test-hub`, `test-midi`, `test-usb-audio`,
  `test-usb-boot`, `test-usb-kbd-replug`, `test-usb-pointer`, `test-usb-replug`, `test-xhci-abort`, `test-disks`,
  `test-serial-shell`, `test-aarch64-boot`. Real controllers (MSI, hub unplug, audio timing) only on hardware.
- Ninth slice: the audio side: `audio` (the facade), `hda` (aarch64 keeps `hda_none`), `usb_audio`, `loopback`. The `Platform`
  gained `task_context`/`task_context_live` (which service is calling; `fos_hal::tasks`), `allocate_contiguous_uncleared`,
  `largest_free_run` and `busy_wait_ms`. The Pi's I2S driver stays in the kernel (clock manager, GPIO, PCM block) and registers
  with `kernel_core::i2s::set`; no registration means no I2S device. Also fixed a slice-8 regression: the `Platform` forwarded
  `interrupts_save_and_disable`/`restore` on x86 only, so the moved `xhci` and `usb_audio` masked nothing on the Pi (aarch64
  has both). x86 kernel 1,177,400 bytes. Verified: `make test`, `test-hda`, `test-usb-audio`, `test-loopback`,
  `test-audio-services`, `test-record`, `test-recorder`, `test-audio-net`, `test-release-loops`, `test-fosplay`,
  `test-xhci-abort`, `test-aarch64-boot`. The Pi interrupt masking and I2S paths only show on a Pi 4.
- Slice 10a, the input side's leaves: `usb_midi`, `usb_serial`, `usb_serial_pl2303`, `usb_serial_ti` and `usb_pointer` are in core.
  `usb_class::Host` gained `poll` (one pass over the event rings), which replaces the calls into `midishell`/`usb_hid`.
  The pointer code reaches the screen through `screen::set` (logical size, rotation, and on the Pi the DSI touch chip, whose
  driver stays in the kernel with the I2C bus); `Rotate` and `line::Line` moved to core. The `touch monitor` loop needs the
  console and NETFS, so `kernel/src/usb_pointer.rs` is now a shim that re-exports core and adds `command`.
- Slice 10c, the enumerators: `usb_hid` (x86), `usb_kbd` (Pi) and `ohci` are in core. What they needed from the console side
  is `usb_env::set` (queue a key, the active keymap, whether the boot disk is known, which `DiskNumber` prints); the Pi's key
  queue and layout stay in `kernel/src/usb_kbd.rs`, which re-exports the core enumerator. `describe_xhci` moved to `usb_hub`.
- Slice 10d, the leaf drivers: `netdump`, `uart` (x86 COM2-COM4, port I/O through `cpu::inb/outb`), `serialports`, `gpu` and
  `virtio_blk` (Pi/QEMU aarch64) are in core. Left in the kernel on purpose: `nicpark` (needs the `term` NIC table), the Pi
  SoC drivers (`genet`, `sdhci`, `gpio`, `i2c`, `i2s_pi`, `mailbox`, `pipower`: they sit on the aarch64 HAL and only matter
  on a Pi), the PS/2 `keyboard` and the shells.
- Slice 10e, the NIC table: `nic::install/count/at/all_interrupt_driven` and `MAX_NICS` live in core (`term` keeps thin
  wrappers and the frame FIFOs), so `nicpark` moved too. `fos_hal::timer::TICK_HZ` (100 on both arches) is new.
- Next: the frame FIFOs and `nic_*` wrappers out of `term` (then `nicshell`), then the server kernel's own `Platform`.
  The server kernel needs all of them; only the real-time guarantees (interrupt-context audio refills) differ.
