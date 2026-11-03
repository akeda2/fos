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
- Slice 10f, the tape receive FIFOs and NIC control calls: new core `nicctl` holds the per-interface frame FIFOs
  (`queue_frame`, `pop_frame`, drop counts, `last_rx_stamp`) and the `nic_*` calls the tape ABI forwards (timestamps,
  multicast, VLAN, offload, counters); `term` re-exports them. `Platform::task_service_owns_nic` (new, default false)
  tells core whether a service owns a NIC.
- Slice 10g, the receive path: `nicctl::drain_nic_to_fifo`, `service_poll` and `service_receive` are in core; the terminal
  registers its `handle_frame` with `nicctl::set_frame_handler` (a server kernel registers its own), and
  `Platform::task_current_owns_nic` is new. `handle_frame`, KUP and the shell stay in `term`: they need the console.
- Stage 1b is at its natural end: what remains in `kernel/` is bound to the console and shells (`term`, `netfs`, `nicshell`,
  `shellcmds`, the PS/2 `keyboard`) or to Pi SoC hardware. Moving those needs a "shell host" trait for little gain; the server
  kernel will have its own console and shell and share only the core crates.
- Next: the server kernel's own `Platform` (design below).
  The server kernel needs all of them; only the real-time guarantees (interrupt-context audio refills) differ.

## Stage 2 design: the server kernel `kernel-mf` (2026-11-02, decided with the maintainer)

Decisions: (1) real VM from the start, no identity-mapped interim; (2) bring up SMP, per-core APIC timers and MSI/MSI-X, all of
them; (3) consoles: serial, framebuffer and network; (4) the same UEFI/BIOS loaders and `BootInfo` (version 5), the same A/B slots.

**Shape.** A new crate `kernel-mf/` (target `x86_64-fos-kernel`, same linker setup) that depends on `fos-hal`, `kernel-core`,
`fosfor-abi`, `fat` and the other shared crates, and implements `Platform` itself (`kernel-mf/src/platform.rs`). It does not
link `kernel/`: the embedded kernel's `term`, `tape`, shells and `static mut` state stay there. A flavour note section
(`.fos_flavour`: `embedded` | `mf`) is added first so KUP, `fosadm` and the loader never cross the two.

**Memory (stage 2a).**
- Physical: a frame allocator over the `BootInfo` memory map with per-node free lists (SRAT is read from ACPI for NUMA; one node
  when absent), "below 4 GiB" runs for 32-bit DMA devices, and `allocate_contiguous` for drivers.
- Virtual: the kernel gets its own higher-half address space (direct map of RAM at a fixed offset, kernel image, MMIO window,
  per-CPU areas, stacks with guard pages); NX and W^X everywhere. The `kernel-core` drivers, however, use physical
  addresses (BARs, DMA buffers) directly as pointers. Until they call a HAL `phys_to_virt()` (a no-op on the embedded kernel),
  the server kernel keeps a temporary **identity alias** of RAM and mapped MMIO in the lower half (NX, W^X kept for the image).
  The conversion and the alias's removal come before user space takes the lower half (done in slice 8a: see the status entry).
- A kernel heap (slab per size class over the frame allocator) so `alloc` exists in the server kernel. The embedded kernel stays
  `alloc`-free.
- Per-process address spaces arrive in stage 5/6 of the staged path; the kernel address space layout leaves the lower half
  free for them from day one.

**CPUs and interrupts (stage 2b).**
- ACPI MADT gives the CPU list; an INIT-SIPI-SIPI trampoline below 1 MiB (not over `0x5000`) starts the APs; each gets its own
  GDT, TSS, IDT, stacks, local APIC and APIC timer. Per-CPU data is reached through `GS`.
- Interrupts: I/O APIC for legacy INTx, MSI/MSI-X for devices (the existing `msi` seam: `msi_allocate` etc.), vectors allocated
  per CPU with a simple spread policy; the 8259s are masked first as in the embedded kernel.
- Locks: ticket spinlocks with IRQ-save, a `Mutex` that sleeps once the scheduler exists; `Platform::interrupts_save_and_disable`
  already exists as the hook. Shared `kernel-core` statics (`static mut` NIC table, FIFOs, USB registry) get a lock audit
  (stage 2 of the staged path) before a second CPU touches them: until then drivers run on the boot CPU only.

**Consoles (stage 2c).**
- Serial (COM1, 115200) first: panics and bring-up logs; the same line discipline as the embedded shell.
- Framebuffer from `BootInfo` (the loader's GOP/VBE mode), with the font code shared from `kernel-core` where it exists.
- Network: the `term net` wire protocol and KUP through `nicctl::set_frame_handler`, so `tools/term_net.py`, `devloop.py` and
  `fosadm` work unchanged against the server kernel. Console output fans out to all three.
- `Platform::print` writes to all active consoles under one lock.

**Slices, each shippable and tested under QEMU `-smp N`.**
1. `kernel-mf` skeleton: boots through UEFI and BIOS, validates `BootInfo`, prints to serial, flavour note, `make image-mf`, and a
   test that boots it (`test-mf-boot`).
2. Physical allocator, higher-half page tables, kernel heap; `make test-mf-boot` also checks the memory report.
3. ACPI (reuse `kernel-core` readers), local APIC and I/O APIC, APIC timer on the boot CPU, `Platform` timer methods.
4. AP bring-up, per-CPU areas, locks; APs idle. `-smp 1,2,8` all boot.
5. `Platform` block, PCI and MSI methods; `kernel-core` drivers (AHCI, NVMe, xHCI, NICs) run on the boot CPU; FOSFS mounts.
6. Framebuffer and network consoles, `term net`, KUP with the flavour marker.
7. Scheduler and kernel threads; then stages 5-6 of the staged path (per-process VM, ring 3).

**Open.** Where the server shell lives (a `kernel-mf` module, or a shared crate once the console seam is clear), how the
flavour marker reaches `fosadm status`, and whether the batch queue needs the scheduler before ring 3.

**Status.**
- Slice 1 (skeleton) is built: `kernel-mf/` (own crate and `Cargo.lock`, shares `kernel/linker.ld` and the kernel target),
  `make kernel-mf image-mf` (`build/fosfor-mf/fosfor.img`, no tapes), `make test-mf-boot` (UEFI and BIOS, `-smp 1,2,8`: valid
  `BootInfo`, serial narration, memory map summary). The `.fos_flavour` ELF section says `mf` (the embedded kernel now says
  `embedded`); nothing reads it yet (KUP, `fosadm`, the loader come with slice 6). It has no IDT yet, so a CPU exception resets
  the guest; slice 3 adds one.
- Slice 2 (memory) is built: `pmm.rs` (bitmap frame allocator over the `BootInfo` map, reached through `vm::direct` so it
  survives the page-table switch; no NUMA lists or lock yet), `vm.rs` (own page tables: every RAM-like region at
  `0xffff_8000_0000_0000 + physical`, NX, 2 MiB pages; the kernel image keeps its identity mapping with W^X per section and
  unmapped guard pages and page 0; framebuffer or VGA text uncached; the lower half is otherwise empty), `heap.rs` (first-fit
  free list, `#[global_allocator]`, grows by 2 MiB chunks, no lock yet), `boot.rs` (`Machine`: what the kernel needs from loader
  memory, copied out before the switch). `test-mf-boot` checks the frame count, the direct map, W^X, a frame round trip and a
  heap self-test with growth. UEFI runtime services and device registers are not mapped yet. Only hardware shows how the
  framebuffer's uncached mapping behaves (write-combining via the PAT comes with the framebuffer console, slice 6).
- Slice 3 (interrupts, ACPI, APIC) is built: `gdt.rs`, `idt.rs` and `isr_stubs.S` are the embedded kernel's files, included
  with `#[path]` (slice 4 replaces the GDT/TSS with per-CPU ones); `trap.rs` (`interrupt_dispatch`, a legible exception report
  with the faulting address described by `vm::describe`), `acpi.rs` (RSDP/XSDT, FADT, MADT with the CPU list, SRAT with nodes
  and memory ranges; tables the map does not describe are mapped on demand through `vm::map_device`), `apic.rs` (8259s
  retired, local APIC enabled, timer calibrated on the PM timer, 100 Hz tick; the I/O APIC found and all pins masked). The boot
  CPU takes an `int3` and the timer tick. `test-mf-boot` checks the CPU count against `-smp`, the tick rate, and a two-node
  `-numa` boot (SRAT). Not yet: the `Platform` trait (slice 5, when `kernel-core` is linked), per-NUMA frame lists (slice 4),
  APs (slice 4). Real hardware shows the x2APIC path, firmware that hands over a different APIC mode, and real SRATs.
- Slice 4 (SMP) is built: `sync.rs` (ticket `SpinLock`, interrupts off while held, `force_unlock` for crash paths) now guards
  the frame allocator, the heap and the serial line (`println!` holds the lock for a whole line; `fatal` and the panic handler
  break it first). `gdt.rs`/`idt.rs` are the kernel's own now: one GDT, TSS and double-fault stack per CPU (`percpu.rs`, `Cpu`
  at `gs:0`, GS base set after the segment reload), one shared IDT. `smp.rs`: a real -> protected -> long mode trampoline
  assembled for the fixed page `0x8000` (checked against the loader's map; clear of the crash-record page `0x5000`), copied
  there and identity-mapped in the kernel root only while APs start; the root is allocated below 4 GiB because the AP loads a
  32-bit CR3. The boot CPU starts APs one at a time (INIT, 10 ms, SIPI twice, wait up to 200 ms for the AP's online flag); an
  AP loads its GDT/IDT, enables its APIC like the boot CPU's (xAPIC or x2APIC) and starts its own 100 Hz timer from the boot
  CPU's calibration. AP stacks (32 KiB, plus 8 KiB for the double fault) come from the AP's own NUMA node
  (`pmm::allocate_near`, SRAT CPU entries); `test-mf-boot` checks every CPU online and ticking at `-smp` 1, 2, 8 and the
  `-numa` placement. Not yet: guard pages under AP stacks (the direct map uses 2 MiB pages; guarded stacks come with the
  scheduler, slice 7), locks on the page tables (only the boot CPU edits them so far), APs taking part in work. Only real
  hardware shows AP startup timing on slow firmware, x2APIC AP mode, and real NUMA machines.
- Slice 5 (devices) is built: `platform.rs` implements `fos_hal::Platform` for `kernel-core`; `vm.rs` gained the identity alias
  (2 MiB pages where clear of the image, 4 KiB in the first 2 MiB, page 0 and the image left to `map_image`) and
  `map_device_identity` (uncached, NX). `pci_config.rs` locks the 0xCF8/0xCFC pair. `msi.rs` is the embedded kernel's file
  included with `#[path]` (so MSI and shared INTx cannot drift), with `trap.rs` dispatching both ranges; `apic::ioapic` has
  `route_gsi`/`mask_gsi`/`gsi_range` (locked). All device interrupts go to the boot CPU. `apic::micros` is one monotonic clock for
  every CPU: the invariant TSC if CPUID says so, else the ACPI PM timer. `block.rs`/`storage.rs` register SATA and NVMe disks,
  find the boot disk by the payload partition GUID and mount its FOSFS. `test-mf-boot` checks the PCI scan, the AHCI disk, the
  boot disk and the FOSFS mount at every `-smp`. Known gaps: no RTC (`unix_seconds` is 0), the `kernel-core` statics are unlocked
  so drivers run on the boot CPU only (APs idle), no USB or NIC bring-up yet, no NVMe disk in the test. Only real hardware shows
  real AHCI/NVMe, MSI/INTx routing and an invariant TSC.
- Slice 6 (consoles, partly) is built: `net.rs` starts the `kernel-core` NIC drivers (e1000 family, RTL8168/8139, 3c905) and
  speaks the embedded kernel's wire formats, so `term_net.py`, `peers.py`, `devloop.py` and `fosadm` work unchanged: discovery
  and the periodic name announce (`hostname` file + NIC number, else `fos-` + last 3 MAC bytes), the `term net` session
  (HELLO/DATA/BYE; console output is queued by `println!` into a relay ring and sent by the service loop, never from inside the
  console lock) and KUP (an 8 MiB staging area from the frame allocator, checksum checked before anything is written, the
  inactive slot written and made pending, reset a second after the ACK). `slots.rs` is the slot-metadata side (`confirm_boot`
  promotes a pending kernel, clean-stop marker, unclean-boot streak cleared after 5 good minutes), `power.rs` the reset (ACPI
  reset register, 0xCF9, 8042, triple fault), `config.rs` reads `hostname`, `settings` (`deny-term-net`) and `debug`
  (`kernel-update`), `shell.rs` is a small shell (`help version cpus mem pci disks nic uptime slots echo reboot`) fed from
  COM1 and the network. `kmain` ends in a service loop on the boot CPU. `make test-mf-net` covers discovery, a session, a bad
  and a good KUP with the loader promoting slot 1, and the `kernel-update`/`deny-term-net` gates. Slice 6d: `fb.rs` draws the console on the loader's
  linear framebuffer (8x16 shared font, light grey on black, `\n \r` backspace and scrolling; ANSI colour and cursor escapes, see the framebuffer paragraph at the end), fed from the same
  `serial::write_str` hook as the network relay, started right after the page tables so the whole kernel narration is on
  screen; the mapping is write-combining (see the end) and a BIOS text-mode
  or headless machine stays serial/network only; `test-mf-net` checks a QEMU screendump. Not yet: NICs and their interrupts
  work on the boot CPU only. Only real hardware shows NIC DMA and interrupts, a real reset and the A/B fallback on silicon.
- Slice 7 (scheduler, kernel threads) is built: `sched.rs`. A thread is a 32 KiB stack in `vm::STACK_REGION` (a guard page below
  each) and a saved `rsp`; `switch_context` pushes the callee-saved registers and swaps stacks (kernel code has no SSE, so no FP
  state; tapes get theirs with ring 3, slice 8). `spawn` (pinned to CPU 0, where the unlocked `kernel-core` drivers and the
  shell/net statics live) and `spawn_any`, `yield_now`, `exit` (the next thread on that CPU frees the dead stack), `sleep_ticks`,
  `wait_while`/`wake_all` (condition checked under the scheduler lock, so no lost wakeup) and `SleepMutex`. The 100 Hz tick
  preempts (10 ms slice); interrupt gates and `SpinLock` guards keep IF off, so a thread is only preempted while it holds no
  lock. Every CPU schedules from one run queue; an application processor's boot context is its idle thread (never queued),
  and other CPUs skip a thread while `on_cpu` (until the CPU that left it has saved its `rsp`). The service loop (net poll,
  serial, KUP, slots) is a CPU-0 thread; the boot thread is idle0. Shell: `threads`. `test-mf-boot` checks interleaving and stack
  return, preemption of threads that never yield, sleep, the mutex and, at `-smp` 2 and 8, unpinned threads running on every
  CPU. Not yet: a wake IPI (an idle AP looks at each tick, so up to 10 ms latency), priorities, per-CPU queues, thread
  migration cost awareness. Only real hardware shows real APIC timing and cross-CPU behaviour under load.
- Slice 8a (the identity alias is gone) is built: `fos_hal::memory::phys_to_virt`/`virt_to_phys` (an offset set once by the platform: 0 on the
  embedded kernel and the Pi 4 core, `DIRECT_BASE` on `kernel-mf`) and every `kernel-core` driver dereferences BARs, rings and DMA
  frames through it (about 370 sites in xhci, ehci, ohci, hda, e1000, rtl8168/8139, 3c905, ahci, nvme, virtio-blk, usb_*); addresses
  written to devices stay physical. `Platform::map_device` now maps registers uncached at `DIRECT_BASE + physical`, `vm::init` no
  longer builds the lower-half alias (only the kernel image is mapped there, W^X) and `kmain` reads `BootInfo` through the direct map.
  `ohci.rs` returns virtual pointers for its ED/TD helpers and converts back with `virt_to_phys` where the controller is told;
  `virtio_blk` (Pi/aarch64 only) turns a caller buffer into a descriptor address with `virt_to_phys`, which only holds for direct-map
  buffers (not thread stacks): bounce it before the server kernel gets virtio. `test-mf-boot` checks the lower half is empty and the
  AHCI/e1000 paths still work; the embedded kernel was re-run through `make test`, `test-nic`, `test-rtl8139`, `test-hda`,
  `test-usb-audio`, `test-xhci-abort`, `test-usb-kbd-replug` (offset 0, nothing should change). Only real hardware shows xHCI/EHCI/OHCI, HDA
  and RTL8168 behind the direct map (64-bit BARs above the RAM top are mapped on demand).
- Slice 8b (ring 3) is built: `process.rs`, `syscall` path, per-process page tables. A process is a kernel thread plus a root page
  table (`vm::create_user_space`): PML4 slot 0 (kernel image) and 256..511 (direct map, stacks; pre-populated so copies stay valid)
  are shared, user pages (U bit, slots 1..255, from 512 GiB) are private and freed with the process (`vm::destroy_user_space`). The
  scheduler loads CR3 and the TSS rsp0/`Cpu::kernel_rsp` at every switch. The GDT has user data/code (0x1b/0x23); interrupt stubs
  `swapgs` when they come from ring 3 (`gen_isr_stubs.py --swapgs`); `syscall` (STAR/LSTAR/SFMASK, SMEP on when present) builds an
  interrupt-style frame and shares the `iretq` return. Calls: write, exit, yield, sleep, ticks; pointers are checked page by page
  (`vm::user_range_ok`). A ring 3 exception kills only the process; `sched::kill` ends a spinner at its next tick. `user_test.rs`
  runs five hand-assembled processes and checks frames come back; `test-mf-boot` at `-smp` 1/2/8. Found on the way: stack slots
  of dead threads are no longer reused (another CPU's stale TLB entry made a new thread run on zeroed memory; needs a shootdown IPI).
  Not yet: ELF loading, FP/SSE/XSAVE state (user SSE gives #UD), SMAP, a user stack guard page, the tape API. Only real hardware
  shows ring transitions, SYSCALL MSRs, SMEP and swapgs on silicon.
- **Slice 8c: real tapes in ring 3** (`kernel-mf/src/{tapeapi,fpu}.rs`, `abi::elf::load_for/segments`). `process::spawn_elf` loads a
  PIE tape at `CODE_BASE` (written at its physical address, relocated for its user address), maps per-page permissions, and enters
  through a per-process shim page. Per-thread FP state: `fpu.rs` enables SSE/AVX (XCR0) per CPU and `sched` saves and restores an
  XSAVE (or FXSAVE) area on every switch of a user thread; the kernel itself never touches XMM. TapeApi (version 30, 104 slots)
  is a read-only table of stubs `mov eax,0x100+slot; syscall`; `tapeapi::call` serves console, time, memory (`alloc`/`free`, a
  per-process heap), `gfx_*` (screen 0), `fp_level`; `fs_*` return "no volume"; everything else returns 0. Shell: `tape NAME [ARGS]`,
  `kill PID`. `make test-mf-tape` runs hello, fpcheck and gfxcheck at `-smp` 1/2/8 (SSE2 and AVX2).
  Input: the started tape is the foreground one; typed bytes (serial or `term net`) reach it through `read_key` (64-byte queue), Escape kills it and the shell prompt returns when it ends. Files: `tapefs.rs` serves `fs_read`/`fs_read_at`/`fs_write`/`fs_remove`/`fs_list` and the stream calls
  on the shared `kernel_core::tape_fs`, under one lock (also the shell's), through kernel bounce buffers (1 MiB read pieces, 8 MiB
  whole-file write); the first process to open a stream owns the stream table until it ends. `test-mf-tape` runs `fscheck` (ranges, two streams)
  and an `ed` save/reload through the keyboard.
  NICs (`tapenet.rs`): the first send/receive/setting call claims a NIC for the process (its frame queue is cleared; released with promiscuous and multicast off when it ends); `nic_receive` drains `kernel_core::nicctl` FIFOs; `net.rs` serialises all NIC access with a per-CPU re-entrant `lock_nics` (the service thread polls from any CPU) and keeps answering discovery frames (0x88b6) through a frame hook. `test-mf-tape` runs `netcheck` (DHCP + 4 pings) on QEMU user networking, e1000. Timestamps, TX offload, VLAN/multicast filters only hardware shows. Gaps: `alloc` gives scattered virtual pages, not identity-mapped physical RAM (no tape DMAs itself; the kernel owns the drivers; ABI 31 `phys_addr` gives a RAM tester the real address),
  Only hardware shows XSAVE/AVX on silicon, SYSCALL/swapgs/SMEP/SMAP and the pixel path.
  Tapes see a one-entry memory map (kind 7, the allocator's free pages at process start; memtest sizes its run from it) and a 1 TiB heap (`test-mf-tape` runs memtest, and with `inject=` checks the reported address lies inside RAM). Hardening: the tape file calls' disk lock is a sleeping mutex held with interrupts on (`tapefs::lock`, `DiskGuard`), so a transfer no longer stalls the tick, NICs or wake IPIs; wake IPI (vector 0x42: a thread made ready while another CPU idles kicks one idle CPU that may run it, instead of waiting for that CPU's 10 ms tick; `test-mf-boot` measures about 150 us in QEMU); TLB shootdown (`tlb.rs`, vector 0x41): an exited thread's stack is unmapped, flushed on every online CPU by IPI, and only then are its frames freed and its slot reused (`test-mf-boot` counts the shootdowns); SMAP is on when the CPU has it (`process::user_access` opens a `stac`/`clac` window in the three places the kernel copies to or from a process; `test-mf-tape` checks it is on for `-cpu max`); the user stack's guard page is the unmapped page below it, and `test-mf-boot` runs a process that overflows it.
  USB (`kernel-mf/src/usb.rs`): xHCI/EHCI/OHCI bring-up through `kernel-core`, keyboards (typed bytes queue to the shell; `keymap sv` in `settings`), and tape calls `midi_*` and `touch_*`. The controller code is unlocked, so the controllers stay polled (no MSI) and every entry holds one re-entrant, interrupts-on lock (a `SpinLock` would stop the tick the waits count); the service thread polls on CPU 0. Unattended operation (`debug.rs`): `debug` file options `reboot-on-crash`, `reboot-delay N`, `watchdog [N]` and `crash-on-boot` as on the embedded kernel; the fatal handler writes a crash record to RAM page 0x5000 (printed and cleared by the next boot), resets, and the loader's crash count stops a loop after five; the watchdog is the boot CPU's timer tick checking that the service thread petted within N s (it does not catch a hang with interrupts off on that CPU: that needs a hardware watchdog). Shell `debug crash|hang` are test hooks, `slots` shows the watchdog; `make test-mf-debug` (QEMU, smp 1 and 4, UEFI) checks crash, hang, record and loop guard. The wall clock is the embedded kernel's `clock.rs` included by path (CMOS RTC read at boot, advanced by the microsecond count; `date` shows it, files get mtimes, `test-mf-boot` checks the line; no `date set` yet). USB mass storage (BOT, 512-byte blocks) is a disk like any other: `block::Backend::Usb`, transfers take the USB lock on CPU 0 (`usb::storage_read/write`), `make test-mf-usb` reads a file off a `usb-storage` stick by `fscheck disk2/note.txt`. Hot-unplug/replug is untested on mf. Audio is separate (`tapeaudio.rs`). `make test-mf-usb` types on a QEMU USB keyboard at `-smp` 1/2/8; QEMU has no USB-MIDI or digitizer, so MIDI and touch only hardware shows.
  Audio (`tapeaudio.rs`): the `audio_*` calls go to `kernel_core::audio` (HD Audio at boot; USB audio through its registered class and xHCI MSI, `no-msi` in `settings` keeps them polled). The drivers are CPU-0 oriented, so a tape pins itself to CPU 0 on its first audio call (`sched::pin_current`) and holds the USB lock per call; one process owns the stream (`AUDIO_BUSY` for others), released at `audio_stop` or process end. Kernel thread stacks grew to 128 KiB (the audio path overflowed 32 KiB: #DF). The MIDI/touch calls pin to CPU 0 too. The USB lock waits with interrupts on (a syscall arrives with them off; a preempted holder would never run again at -smp 1) and the host `poll` hook called from interrupt handlers only try-locks. `make test-mf-audio` plays a 440 Hz WAV twice through QEMU HDA and QEMU `usb-audio` at -smp 1/2/8 and checks the recording (the USB run shows underruns: QEMU's full-speed model, tick-paced refills); QEMU only, real audio timing is hardware-only.
  GPIO and I2C (`tapeapi.rs`): the BCM2711 pins exist on the Pi 4 only, so the server kernel answers as the embedded x86 kernel does without pins (`gpio_pins`/`gpio_read` 0, writes refused, mode/pull -3, function/pull_get -2, I2C -4); `gpiopanel` says "no GPIO pins" (`test-mf-tape`). `gpio-sim` is not carried over (the policy lives in `kernel/src/gpio.rs`, outside kernel-core).
  NETFS and tape sync (`netfs.rs`, `tapesync.rs`): `tools/foscopy.py` reads and writes files of any FOSFS volume (`disk1/NAME`, tapes also from `bin/`), and a peer's `tools/tape_manifest.py` or `tapesync` gets the signed manifest of the stamped tapes (tag 0x40, `synckey` on the boot volume; the key is never served). The frame handler runs under the NIC lock, so it only queues requests; the service thread's `serve` does the disk work under the tape disk lock (`tapefs::lock`), a GET streaming 1 KiB chunks a millisecond apart (the shell is busy meanwhile, as on the embedded kernel). Files up to 8 MiB, buffered on the heap, so a running tape does not stop NETFS. Not done: the `tapesync` client command, GPIO over the network (needs pins). `make test-mf-netfs` (foscopy round trips up to 300,000 bytes, refusals, the signed manifest and a wrong key); QEMU only: real NIC pacing and a transfer racing a tape on silicon are for hardware.
  Interrupt routing: `percpu::interrupt_target()` is the one place that names the CPU device interrupts go to (MSI messages through `Platform::interrupt_target`, I/O APIC pins in `apic::ioapic::route_gsi`). It is the boot CPU unless `irq-cpu N` is in `settings` and CPU N runs; it is read once when a device is set up, so there is no migration. `test-mf-net` boots with `irq-cpu 1` on 4 CPUs and checks the e1000's interrupt self-test and the terminal. Only the NIC is tested that way: the USB, HD Audio and storage handlers are not yet run on another CPU (the USB driver is still CPU 0 oriented and polled), so leave the default for them; a spread of different devices over different CPUs is not done.
  Framebuffer (`fb.rs`, `cpu::pat_init`, `vm::map_write_combining`): PAT entry 4 is set to write-combining on every CPU (boot CPU before the framebuffer is mapped, each AP in `ap_main`) and the framebuffer's 4 KiB pages use the PAT bit; a 2 MiB mapping over it is split first (before other CPUs run, so no shootdown). Without a PAT it stays uncached; boot says which (`Framebuffer: write-combining (PAT)`). The console keeps its text grid in `CELLS` and never reads the screen (reads of WC memory are as slow as uncached); scrolling redraws from the grid once per `write_str`, and an `sfence` ends each call. ANSI: SGR 0/1/22/30-37/39/40-47/49/90-97/100-107, cursor `H f A B C D`, erase `J` (0, 2, 3) and `K`; other sequences are swallowed. `debug colors` prints a palette; `test-mf-net` checks the PAT line and red pixels in a screendump. Only real hardware shows the speed-up (QEMU's framebuffer is RAM) and whether a given chipset honours WC. Still missing: a BIOS text-mode fallback.
  First hardware run (`piano`, Sandy Bridge i5-2540M, 4 logical CPUs, 82579 NIC, EHCI only, boots from a USB stick, no COM1, PS/2 keyboard), by KUP from the normal kernel: write-combining framebuffer, SMP, the NIC and terminal over the network, KUP and the A/B confirm work. It found three bugs QEMU cannot: (1) no UART: COM1's status reads 0xff, so `serial::read_byte` returned data forever and the service thread never finished a pass; `serial::init` now detects the UART (scratch register) and reads/writes skip it; (2) a USB boot disk exists only after `usb::init`: `main` now runs USB first when SATA/NVMe holds no payload partition and retries `storage::find_boot_disk` (then `settings` `no-msi` cannot apply to USB on that machine); before, slots and every config file were skipped; (3) no PS/2: `ps2.rs` polls the 8042 from the service thread (no IRQ), `test-mf-net` types `version` through QEMU's PS/2 keyboard. Still unverified on hardware: ring 3 tapes, USB keyboards, audio.
  Second hardware run (`piano`): `tape hello`, `fpcheck` (SSE) and `gfxcheck` (colour bars, ramp, clipping, console restored) run in ring 3 and draw correctly; `reboot`, `debug crash` (reset after 3 s, back on the same slot) and `debug hang` typed on the local keyboard (the watchdog resets after 60 s) work. Not caught: a `debug hang` typed over the network, because the terminal session's shell runs with interrupts off inside the NIC poll, so no timer tick reaches the watchdog (the missing hardware watchdog, DEFERRED.md). `tape NAME` now also looks in `bin/` (a v4 volume keeps its tapes there, and NETFS puts replace them there).
  USB audio on piano (EHCI, a class-2.0 card, 44.1-96 kHz): the shell command `audio` (`kernel-mf/src/audioshell.rs`: `audio`, `audio device N|auto`, `audio tone [HZ] [SECONDS] [RATE]`) lists the card and plays a tone that was heard, clean, on the machine's speaker; 4 s gave 32768 iso packets and 176 underruns, all in the first 48 packets (the ring fills at start). The first attempts froze in `ua::start`: the controller code waits on timer ticks and the network shell runs with interrupts off, so the command now pins to CPU 0, takes the USB lock and turns interrupts on for its duration (as the tape calls do). Any new shell command that touches USB needs the same.
  The "N frames not returned" flake of `test-mf-boot` at `-smp 8` (always 32 frames short per late thread, minus the 2 allowed) was the test, not a leak: its settle loop called `sched::sleep_ticks(1)` from the boot thread, which returns at once when nothing else is eligible on CPU 0 (the boot thread is the idle thread and cannot sleep), so 50 iterations took microseconds and the frames of threads still between their outcome and `sched::exit` were counted too early. The loop now yields and halts a tick per iteration (like the waits above it), up to 10 s. Anything else that must wait on CPU 0 uses `yield_now` + `sti; hlt`, not `sleep_ticks`.
  `date set YYYY-MM-DD HH:MM[:SS]` (local time) works in the mf shell as in the embedded kernel: it calls the shared `clock::set`, which writes the RTC; dates before 2024 are refused. Checked in QEMU by `make test-mf-net`; real-RTC write only verifiable on hardware.
  The disk lock (`tapefs::lock`) now also covers the slot-metadata writes of `slots::housekeeping`, `mark_clean_stop` and the shell `slots` command, so they cannot interleave with a tape or NETFS transfer. KUP runs in the NIC receive path, where sleeping is not allowed, so it uses `tapefs::try_lock` (`sched::SleepMutex::try_lock`): a busy disk fails that attempt with the disk-error code and the host retries. Verified in QEMU: `test-mf-net`, `test-mf-netfs`, `test-mf-boot`, `test-mf-tape` (one garbled-keystroke failure in `fscheck remove`, the known flake, then 2 clean runs).
  Image size: the user stack moved from 16 MiB to just under the tape shim (`STACK_TOP` = `USER_BASE + 0x1ff_f000`), so an image can span about 32 MiB instead of colliding with the stack at 16 MiB. `spawn_elf` refuses a larger one up front (`SpawnError::TooLarge`, `process::MAX_IMAGE`); `test-mf-boot` checks that ("big image ok"). Verified in QEMU: `test-mf-boot`, `test-mf-tape`; no real tape over 16 MiB was run.
