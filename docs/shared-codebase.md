# Fosfor OS: one 64-bit codebase, two boot paths

Status (2026-10-08): **P0, the driver ports and P2 are done on branch `unify-64`; the BIOS path
(B1-B5), the shared-crate move (P3) and the rename (R) are not started.** Built so far: tag
`legacy-v1` and branch `legacy` at `055c093`; RTL8139 and Intel HD Audio ported into the 64-bit
kernel (`make test-rtl8139`, `make test-hda`); tape API 3 (`audio_devices`, `audio_use_device`);
the tree promoted to the top level (`abi/`, `kernel/`, `boot/uefi/`, `fat/`, `tapes/`, `tools/`)
with the v2-prefixed make targets renamed (`make image`, `make test-NAME`, `make test-all`).
32 of 33 tests pass; `test-nettapes` failed before the move too. **B1 (BIOS spike) is done**: see
section 4.4. This note supersedes the
"v1 and v2 live side by side, drivers copied and widened" decision in v2-design.md
section 3 and the "v1 is maintained" paragraph in section 12.

## 1. Decisions

1. **One kernel, 64-bit, for both firmware types.** Everything after the boot handoff
   is shared: kernel, drivers, shell, FOSFS/FAT, tape ABI, tapes, host tools, tests.
   Only POST/boot differs.
2. **Two boot paths** that end in the same call: `kernel_entry(&BootInfo)` in long mode.
   - **UEFI**: the existing loader (`v2/loader`).
   - **BIOS**: a new 64-bit-aware boot chain (MBR -> stage two -> long mode). It
     replaces v1's 32-bit chain; it does not extend it.
3. **v1 is frozen as the legacy edition** (32-bit, BIOS, Pentium 4, the protected-mode
   teaching tree). It is kept on a `legacy-v1` tag and branch; fixes may be
   cherry-picked into it later, but it is no longer built from `main`.
4. **v2 is production, v1 is lab/education.** The shared tree is developed v2-first;
   the BIOS path exists so lab and older 64-bit machines (for example the AMD/nForce
   Athlon 64 X2, which has BIOS only) run the same system as the UEFI machines.
5. **The project is renamed Fosfor OS** (Swedish *fosfor*, phosphorus, as in phosphor
   screens). Binary and image prefix `fosfor`. `fos` stays wherever it is hard to
   change (section 7).

## 2. What differs between the two platforms (and nothing else)

| Concern | UEFI path | BIOS path |
| --- | --- | --- |
| Reading the kernel | Block I/O, in the loader | INT 13h EDD, in stage two (real mode) |
| Slot A/B state machine | `select_slot` in `v2/abi`, called by the loader | same logic; see 4.3 for where it runs |
| Memory map | `GetMemoryMap`, folded to E820 numbering | E820 (INT 15h) |
| Console | GOP framebuffer | VGA text 80x25 at boot; VBE linear framebuffer if stage two can set one |
| ACPI RSDP | configuration table | scan EBDA and `0xE0000-0xFFFFF` |
| Reset / power | runtime services `ResetSystem` | ACPI reset register, then 8042, then triple fault |
| Boot disk identity | GPT partition GUID from the loaded image's handle | stage two knows the LBA of its own payload partition and passes the GUID from its GPT entry |
| Runtime services pointer | present in `BootInfo` | zero |
| Kernel physical placement | position-independent ELF64, loader picks pages | same ELF64; stage two picks a fixed high address and says so in `BootInfo` |

Everything else, including interrupts (APIC/IOAPIC/MSI, ACPI tables, no AML), paging,
all drivers, the shell and tapes, must not know which path booted it. If code needs to
ask, that is a signal the difference belongs in `BootInfo` (a field or a flag), not in
an `if`.

**Consequence worth stating:** the kernel never calls BIOS. v1's real-mode disk
trampoline, PIC/IVT swapping and A20 handling stay in the legacy tree. The kernel finds
its disks with native drivers (AHCI, NVMe, xHCI, EHCI, USB storage), exactly as on UEFI.
A BIOS machine whose only disk interface is legacy IDE or OHCI USB is therefore **not
supported by the new tree**; it stays on legacy v1.

## 3. Repository layout (target)

```
boot/uefi/        loader (was v2/loader)
boot/bios/        stage1.S, stage2.S, long-mode entry (new; v1's asm is the reference)
kernel/           the one 64-bit kernel (was v2/kernel)
abi/              BootInfo, slot metadata + select_slot, TapeApi, ELF, keymap (was v2/abi)
crates/           shared no_std crates with no tape of their own: fat, tape-net, opus-codec
                  (with its vendored libopus), audio-wire, tape-rt, tape-panel
tapes/            one crate per tape, library and binary together (was v2/tapes). The tape
                  logic (`lib.rs` of router, nettest, ed, ip-chat, telnet) is already
                  byte-identical in v1 and v2, so the v2 crate simply wins; v1's copy goes.
tools/            image builder, host tools (devloop, foscopy, send_*...), test harness
docs/
```

`main` stops carrying the v1 kernel, boot stages and v1 tapes; they live on `legacy-v1`.
The moves are done with `git mv` in a few mechanical commits so history follows the
files. The `v2-` prefix on make targets and `v2/` in paths go away at that point.

## 3a. Driver parity: what v1 has that v2 does not

Measured 2026-10-08 by comparing `kernel/src` with `v2/kernel/src`. Deleting v1 from
`main` loses these until they are ported, so **P2 must not delete the v1 kernel from
`main` before the drivers someone still needs are in the shared tree**:

| v1 driver | Hardware | v2 equivalent | Notes |
| --- | --- | --- | --- |
| `hda` | Intel HD Audio | none | lab audio machines; v2 has only `usb_audio` |
| `ac97`, `es1370` | AC'97, Ensoniq | none | old; low priority |
| `uhci` | USB 1.1 controller (+ USB audio, mass storage) | none (xHCI/EHCI only) | the Pentium 4 case; stays legacy |
| `rtl8139`, `ne2000` | old NICs, QEMU defaults in v1 tests | none | `rtl8139` is v1's default test NIC |
| `parallel` | LPT printer | none | `term printer`, `print` |
| `e1000e`, `i350` | separate files in v1 | folded into v2's `e1000.rs` | covered: 0x10d3, 0x1503, 0x15b7, 0x156f, 0x1521 |
| `lapic`, `interrupts`, `pmem`, `entry.S` | 32-bit core | `apic`, `idt`, `memory`, ... | rewritten, not ported |

v2-only: `acpi`, `ioapic`, `msi`, `nvme`, `usb_hub`, `usb_hid`, `fatvol`, `clock`, `install`,
`lineedit`, `framebuffer`, `power`, `diag`.

Decision needed (open): which of `hda`, `uhci`, `rtl8139`, `parallel` are ported into the
shared tree before the v1 kernel leaves `main`, and which stay legacy-only. Everything not
ported is still available at tag `legacy-v1`.

Also not carried over blindly: the 69 v1 e2e scripts (`tools/test_*.py`, `tapes/test_*_e2e.py`).
Three of them (`test_router_e2e`, `test_bridge_e2e`, `test_ip_chat_e2e`) already run
against v2 through `v2/tools/qemu_v1_shim.py`; the rest need triage into "covered by a
v2 test", "port", or "drop with the driver".

## 4. The BIOS boot path

### 4.1 Chain

```
BIOS -> stage1 (MBR, 512 B) -> stage2 (real mode) -> long mode -> kernel_entry(&BootInfo)
```

Stage two keeps v1's narration philosophy: say what each step does and why.

### 4.2 Stage two's jobs

1. E820 memory map (INT 15h, `0xE820`), with the same sanity checks v1 has.
2. A20 (BIOS, then 8042, then port `0x92`, re-verified after each; v1's order is proven
   on real hardware, keep it).
3. Optional VBE: pick a linear-framebuffer mode and describe it in `BootInfo`; if none,
   leave VGA text mode and say so. The kernel console needs a text backend for this
   (small; v1's VGA code is the reference).
4. Find its own payload partition (GPT, by type GUID `f05b2a01-0000-4f05-8000-0000000000a1`),
   run the A/B slot state machine, read the chosen kernel ELF64 by EDD in bounded
   transfers.
5. Place the ELF segments above 1 MiB, build identity page tables, enable PAE and
   long mode, load a GDT, jump to the kernel entry with `BootInfo` in a register.

### 4.3 Open design question: where the slot logic and ELF placement run

The kernel is already about 550 KB and will grow, so it cannot be loaded inside the
640 KB low-memory window as v1 did. Two ways to load it high with BIOS reads:

- **A. Unreal mode / protected-mode copy in stage two.** Read in chunks into a low
  buffer with EDD, copy each chunk above 1 MiB, parse the ELF afterwards. Keeps stage
  two entirely in assembly (as v1) and keeps the proven A/B state machine in asm.
- **B. A small Rust "bios-loader" in long mode.** Stage two only reads the loader blob
  and enters long mode; the loader calls back into stage-two real-mode code for disk
  reads. Lets the slot logic and ELF relocator be the *same Rust* as the UEFI loader
  (`abi::select_slot`, `elf.rs`), but needs a long-mode-to-real-mode disk trampoline
  (v1 already has the 32-bit equivalent).

Decide with a short spike (milestone B1). The default expectation is **A first** (least
new machinery, v1's asm is proven) with the state machine's byte layout and its tests
shared with `abi::select_slot` so the two implementations cannot drift.

### 4.4 B1 spike result (2026-10-08)

`make test-bios-spike` boots `build/bios-spike/spike.img` under SeaBIOS on QEMU's `pc` and `q35`
machines: stage one (`boot/bios/stage1.S`, the MBR) reads stage two from LBA 64 with EDD; stage two
(`stage2.S`, 1.3 KB) initialises COM1, enables A20 (BIOS, then port 0x92, verified), collects the
E820 map into a boot-information block at `0x5000`, reads the kernel in 16 KiB pieces and copies each
above 1 MiB through unreal mode, builds identity page tables for 4 GiB (2 MiB pages), enables PAE and
`EFER.LME` and jumps to a 64-bit test kernel (`spike_kernel.S`) that checks the boot information and a
marker at the end of its own image. What this settles:

- **Option A of 4.3 works**: stage two stays in assembly and needs no real-mode trampoline from long
  mode. Unreal mode is re-entered before every copy because a BIOS call may reset the ES limit.
- **Stage two lives in the GPT "BIOS boot gap"** (LBA 34-2047, 1 MiB free), so no partition is
  needed and one disk image can later carry both the UEFI ESP and the BIOS chain (a hybrid).
- **Not yet done, deliberately**: loading a real ELF64 PIE (the spike kernel is a flat binary linked
  at its address). The plan is to stage the whole slot image high, then let a small 64-bit routine
  in stage two apply the `PT_LOAD` segments and `R_X86_64_RELATIVE` relocations, so the same kernel
  ELF the UEFI loader reads boots on both paths. Also not done: the 8042 A20 fallback, the A/B slot
  state machine (to share `abi::select_slot`'s byte layout), GPT discovery of the payload partition,
  VBE, the ACPI RSDP scan, and everything on real hardware (unreal mode and A20 are the usual
  real-machine surprises; the AMD/nForce board is the first target).
- A builder bug worth remembering: patching a `bytearray` with a slice of a different length shrinks
  it silently, which shifted every data address by four bytes and showed up as garbled strings and a
  BIOS disk error; `tools/build_bios_spike.py` now asserts the length is unchanged.

## 5. Migration, in order

| # | Step | Done when |
| --- | --- | --- |
| P0 | Snapshot: tag `legacy-v1` and branch `legacy` at the last commit that builds v1 | `git checkout legacy-v1 && make test` still passes |
| P1 | Document, then agree on the layout (this file) | you have read it |
| P2 | Promote v2 on `main`: `git mv` the trees per section 3, drop `v2-` prefixes, delete the v1 kernel/boot/tapes from `main`, fix every path in Makefile, tools and tests | `make` builds the UEFI image, `make test` passes under OVMF |
| P3 | Move shared crates to `crates/` (`fat`, tape libs, `tape-net`, audio crates) with their host tests | `make test-unit` passes; no crate is copied twice |
| B1 | BIOS spike: stage1/stage2 reach long mode and print from a tiny test kernel under SeaBIOS (QEMU `-machine q35` without OVMF) | serial shows long-mode entry |
| B2 | Real kernel boots via BIOS: `BootInfo` filled, VGA text console backend, ACPI scan, slot A/B | `make test-bios` boots to the prompt |
| B3 | Parametrise the test harness by platform: every existing e2e script runs with `PLATFORM=uefi` and `PLATFORM=bios` | `make test-all` covers both |
| B4 | KUP + soft fallback on BIOS (the slot logic and its tests are shared) | `test-debug-mode` passes on both |
| B5 | Real hardware: AMD/nForce (BIOS only), then the Skylake in CSM/legacy mode if its firmware has one | prompt, `term net`, tape load |
| R | Rename to Fosfor OS (section 7), as one mechanical change after P2 and P3 | banner, docs, image names |

P0 is the only step that must happen before anything is moved or deleted. It is local
and reversible; pushing it is the user's call.

## 6. Tests

- The existing v2 tests (`make test-v2-*`) keep their meaning and become the UEFI half
  of the matrix.
- v1's e2e scripts are not carried over blindly: for each, either the behaviour is
  already covered by a v2 test (drop it), or it is ported to the shared harness with
  the platform as a parameter.
- Host-side `cargo test` for the shared crates is the main guard that a "shared" crate
  has not grown a platform assumption.
- Hardware verification stays with the machines in the memory notes; BIOS-path claims
  need the AMD/nForce machine, not just QEMU.

## 7. The rename (Fosfor OS)

Changes (user-visible and code names): boot banner, `about`, README and docs,
`fosfor.img`, crate names `fos-v2-*` -> `fosfor-*`, make variables, host tool help text.

Stays `fos` on purpose, because changing it would break existing media or the shared
host tools:

- the FOSFS on-disk format and name (`fosfs`), including the superblock magic;
- the GPT payload type GUID `f05b2a01-...`;
- wire magics and tags (`FOSMAP`, chat/NETFS/KUP/term tags) and the `CHAT_ETHERTYPE`s;
- `foscopy.py`, `FOS_PEERS_FILE`, `~/.cache/fos_peers.json`.

"fos" then reads as the short form of Fosfor; the docs say so once.

Name check (2026-10-08, web search only, not a trademark search): no operating system
called Fosfor was found. A commercial analytics platform, Fosfor Decision Cloud
(LTIMindtree), uses the word. Use "Fosfor OS" in titles; run USPTO/EUIPO/WIPO searches
before any commercial use.

## 8. Risks

1. **BIOS machines without native-driver coverage** (IDE-only, OHCI-only USB) cannot run
   the new tree. They stay on legacy v1. Say this in the README.
2. **The kernel becoming a video-mode dependent system.** A BIOS machine with no VBE
   linear mode must still get a usable text console; the console backend interface has to
   admit both.
3. **IRQ routing on old BIOS chipsets** (nForce): the IOAPIC/MADT path in the kernel was
   built for UEFI machines; on BIOS the MADT may be absent or odd. The 8259 PIC fallback
   (kept as a disabled-by-default path) is the safety net; test on the real machine early.
4. **Test matrix doubles.** Mitigated by running the platform-neutral tests against both
   QEMU configurations (`SeaBIOS` and `OVMF`), not by writing platform-specific tests.
5. **Deleting v1 from `main` loses easy access to its teaching material.** The legacy
   branch is the answer; `README.md` should link it.
