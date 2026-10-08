# v2 on the Skylake: a visit checklist

First real-hardware run of the v2 kernel (milestone M7). Everything below passed in QEMU
(`make test-v2`, `-keyboard`, `-usb-boot`, `-config`, `-tapes`, `-nettest`, `-debug-mode`,
`-msi`, `-router`, `-nettapes`). **Status (2026-10-03):** most of it has since run on this
machine: boot from the stick, five NICs on MSI, KUP pushes, `memtest`, `nettest`, `shutdown`,
the watchdog (`debug hang`), a USB keyboard and `router` (including a DHCP lease that arrives
after startup). Still untried here: the crash-loop guard, `bridge`, hubs, NVMe, audio. Expect
surprises, and write down what the screen says: the kernel narrates every step, so a stop is almost always
located by the last line.

**Fallback at every stage:** the v1 stick still boots this machine in BIOS/CSM mode. Nothing
here touches the internal disk (the AHCI driver only reads it, and only `disks` shows it).

## 0. Before leaving the desk

```sh
make v2-image                          # build/v2/v2.img (96 MiB: ESP + payload + FOSFS)
ls -l build/v2/v2.img
sudo python3 tools/write_stick.py /dev/sdX   # or: sudo dd if=build/v2/v2.img of=/dev/sdX bs=4M conv=fsync status=progress
```

`dd` is fine for one stick; the script is `dd` with the checks (whole removable disk, nothing mounted,
size sane, you type the name back). It also gives each stick fresh random GUIDs: the kernel finds its
boot disk by the payload partition's GUID, so two sticks written from the same image (or by plain `dd`)
cannot be told apart when both are plugged in. `--keep-guids` opts out. Triple-check `lsblk` first:
this erases the stick.

Optional files on the stick's FOSFS (build them in with `--file NAME=PATH`, or write them
later with `write`): `debug` (`reboot-on-crash`, `reboot-delay 3`, `kernel-update`,
`watchdog 60`), `hostname`, `settings` (`no-msi` if MSI misbehaves, `deny-term-net`), `start`.
`tools/default-debug.txt` is a good `debug` for a remote machine. Without `kernel-update`
there is no network update path.

Bring: the stick, the v1 stick, a keyboard, a way to see the screen (a photo of each stop is
worth a paragraph), the NIC cabling notes below, a second stick if you want one with a `debug` file.

## 1. Firmware settings

* Boot mode **UEFI** (not Legacy/CSM). **Secure Boot off**: the loader is not signed.
* SATA mode **AHCI** (v2 does not drive IDE-compat or RAID). The v1 notes mention a SATA disk
  that has to be present to avoid an F1 prompt at POST; leave it as it was.
* Boot the stick from the one-time boot menu, entry "UEFI: <stick>".
* USB ports: a root port is the safe choice; hubs on xHCI are supported (`usb_hub.rs`) and confirmed here with a hub plugged in after boot (this machine would not POST with that hub attached; run `usb rescan`): back-panel ports
  directly on the board are root ports. The stick and a keyboard both need ports; the
  keyboard also must not be behind a hub. Skylake has xHCI only, which v2 drives.

## 2. What the screen should say, in order (and what a stop means)

| Last line you see | Meaning |
| --- | --- |
| nothing / firmware menu | not UEFI-booting the stick: boot mode, Secure Boot, wrong entry |
| `FOS v2 loader (milestone M1)` ... `Graphics: WxH`, `ACPI 2.0+ RSDP` | loader running; firmware found |
| `Payload partition: LBA ...`, `Slot metadata: booting slot 0 ...` | stick read through firmware Block I/O |
| `Kernel at 0x..., entry ...` then `Exiting boot services` | loader done; next line is the kernel |
| (nothing after Exiting) | kernel died before serial/framebuffer: send me the screen photo of the last loader lines; try `reboot-on-crash` is not loaded yet at this point |
| `FOS v2 kernel (milestone M4): running in 64-bit long mode.` | kernel alive |
| `GDT + TSS loaded`, `Paging: own page tables`, `Framebuffer console ready` | memory/paging fine; from here text is also on screen |
| `ACPI 2.0+: PM timer at port ...`, `Local APIC ... ticking at 100 Hz` | interrupts and the clock |
| `Timer self-test: 100 ticks took ~1000 ms` | **verify**: 900-1100 ms |
| `I/O APIC`, `PS/2 keyboard: ...` | PS/2 may say "no 8042": fine on a USB-only board |
| `PCI: N functions found` | |
| `e1000 (E1000e) at ...: MAC ...` and `interrupts: ...` | **verify**: which delivery (MSI / INTx / polled) and the MAC |
| `e1000 (I350) at ...` (x4) | same, per port |
| `USB: N xHCI ...` and `USB keyboard ... ready` / `USB storage ...: diskN` | xHCI up; the stick as a disk |
| `Boot disk: disk1` and `FOSFS: mounted on disk1` | boot-disk discovery by GUID worked |
| `FOS v2 kernel: M3 complete. Waiting for a term net client.` and `fos> ` | milestone reached |

A crash prints `*** CPU EXCEPTION ...` with the faulting RIP and a plain-words cause; with
`reboot-on-crash` it resets and the next boot prints "Previous boot crashed".

## 3. First commands (keyboard or `tools/term_net.py`)

Run these in order and keep the output (`dmesg` replays the whole boot log):

```
dmesg            # everything above, again, complete
pci              # every function; look for 8086:15b7 (I219-LM), 8086:1521 (I350), 8086:xxxx xHCI
nic              # one block per NIC: MAC, link, "interrupts: MSI / INTx pin N / polled", frame counters
usb              # xHCI ports, devices, keyboards
disks            # the stick as disk1; the internal SATA disk(s) if AHCI mode
memory           # managed vs free; should be ~16 GiB less firmware reservations
uptime
```

Expected on this machine (from v1 notes): NIC 1 = the I219-LM on the LAN (`18:66:da:25:1f:73` is
the dev machine's session MAC, not a NIC here); NICs 2-5 = four I350 ports (`a0:36:9f:be:42:78`,
`...:79`, `a0:36:9f:6b:fc:5a`, `...:5b`), cabled 2<->3 and 4<->5 for the nettest lab.

**Things to confirm or capture if wrong:**
* the I219: link up? frames received/sent counters rising? (`TCTL` fix is carried over from v1);
* the I350s: MAC valid (not 00:.. / ff:..)? link? (QEMU has none; this is their first v2 run);
* `interrupts:` for each NIC. If a NIC is polled or storms, put `no-msi` in `settings`
  and note which path failed. `nic` shows interrupts taken: it must rise with traffic;
* `Timer self-test` within 900-1100 ms;
* `Crash record: RAM page reserved at 0x5000`: a later "Previous boot crashed" proves RAM survives warm reset.

## 4. Network driving (from the dev machine, with `python3-netraw`)

```sh
~/.local/bin/python3-netraw tools/term_net.py --iface enp0s31f6 --mac MAC "uptime"
~/.local/bin/python3-netraw tools/peers.py --discover    # the hostname shows as fos2 / fos2-N per NIC
```

The wire protocol is v1's, so `term_net.py`, `send_tape.py`, `send_kernel_update.py`, `peers.py`
should work unchanged. `tools/devloop.py` works against v2 (`wait`, `run`, `push`; `tape-into` reads plain `nic`
when there is no `nic list`). If a push misbehaves, use the plain tools and check `debug slots`.
The Skylake boots slowly: `wait` may time out once before the machine answers.

Kernel push (needs `kernel-update` in `debug`): `python3-netraw tools/send_kernel_update.py
--iface IF --mac MAC build/cargo-target/x86_64-fos-kernel/release/fos-v2-kernel`, then
`debug slots` (booted slot 1, active 1, nothing pending = promoted).

## 5. Load tests (in this order; stop at the first problem)

1. `run hello`, `run calc 3 4 +`: tapes work (ELF load, relocation, console).
2. `run memtest size=1024 passes=1`: 1 GiB, a few minutes; then `run memtest` (all RAM, ~16 GiB:
   long) and `run memtest soak for=10m status=60` for the soak path. Watch for "memtest: FAIL".
3. NIC lab: `nic` first; then `run nettest 3 4` (impair between I350 ports 3 and 4),
   or with generator and sink: `run nettest 3 4 gen=2 sink=5 ...` as in `docs/nettest.md`.
   Compare against v1's numbers (982 Mbit/s of 1500-byte frames, 0 lost).
4. `run router ...` / `run bridge 1 2` if the cabling allows.

## 6. If it will not boot, or misbehaves

* Take the screen photo, run `dmesg` if a prompt exists, and note which NIC/USB port was used.
* `reboot-on-crash` + `watchdog` in a `debug` file turn a hang into a reset plus a record; the
  loop guard stops after five pre-milestone crashes in a row (the loader prints the counts).
* Known not-done (do not report as bugs): audio, NVMe on real hardware (QEMU only), hubs on real hardware (QEMU only),
  the legacy-BIOS path (use the v1 stick), `chat`, OHCI/UHCI.
* To return to v1, boot the v1 stick; nothing on the machine was modified.
