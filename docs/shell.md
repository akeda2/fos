# The shared shell

One shell core for every port: `crates/shell` (`fosfor_shell`, `no_std`, host-tested with `cargo test`, part of `make test`).

- **`Host`** is what a port provides: `print`, file calls on references (`NAME`, `diskN/NAME`, `@LABEL/NAME`, `esp/NAME`),
  `list`, a scratch buffer and `tape_help`. `kernel/src/shellhost.rs` (`KernelHost`) implements it for both kernels on
  top of `tape_fs`, so the shell sees the volumes a tape sees.
- **`Command`** is a table row: name, aliases, usage, summary, handler. `fosfor_shell::execute(host, &[PORT, files::COMMANDS], line)`
  looks a line up (a port's table first, so it may override) and builds `help` / `help NAME` from the tables; for a name
  that is no command `help NAME` shows a tape's `.fos_help`.
- **Shared commands** (`files.rs`): `ls cat write rm cp (copy) mv cmp mkdir stat chmod chown` (the last three show or set owner, group and mode on ext volumes: `chown -R 1000:100 ext1/media`, `chmod 755 ext1/media`, `stat ext1/media`); `ls`, `cp`, `mv` and `rm` take `* ? [a-z]` patterns (`glob.rs`, docs/fosfs-v4.md). The older `cat diskN NAME` spelling still means `diskN/NAME`.
  `cp SRC DST` accepts a bare volume as DST (keeps the name); files up to 32 MiB on the embedded kernels. The server kernel (mf) has no limit when the
  destination is an ext volume: `Host::copy_stream`/`compare_stream` move 1 MiB at a time (`make test-mf-cp`).
- **Working folder (mf only)**: `cd [FOLDER]` and `pwd`. Relative names of the file commands (`ls cp cat ...`) are looked up under it;
  names with a volume (`@LABEL/`, `diskN/`, `extN/`, `esp/`, `//`) are not, `/NAME` is always under the boot volume's root (mounts are
  folders in it: `mv x /data/`), `..` works, `cd /` goes to the boot volume's root and a bare `cd` to the root of the volume you are on
  (`disk2`, `@LABEL`, a mount point). The prompt shows it (`fos @rackdata/www-root> `); a new remote
  terminal session starts at the root. Tapes and scripts are still found as typed. A mount point may be an ext volume
  (`mount rackdata @rackdata`, or `fstab`).
- **Switching kernels**: `kernel [list]` shows the kernels kept on the boot volume (`kernel.embedded`, `kernel.mf`) and the running one;
  `kernel use embedded|mf [force]` checks the file (ELF, `.fos_flavour` section, machine type), writes it to the inactive A/B slot as the
  pending one, as KUP does, and restarts. The loader gives it one try and falls back to the old kernel if it never reaches its milestone.
  `make image` puts both kernels on the volume; on a machine `fosadm put HOST build/cargo-target/x86_64-fos-kernel/release/fosfor-kernel
  kernel.embedded` (and `fosfor-kernel-mf` as `kernel.mf`) keeps them current. `make test-mf-kernel-switch`.
- **A port** keeps only what is hardware- or port-specific, as its own table (Pi: `kernel/src/shell.rs`), plus what to do with
  a line that is no command (tape names, `PATH/NAME`).

Status: both shells use the core for the file commands (`ls cat write rm cp cmp`). x86 keeps its own `copy` (peers,
NETFS) and `fsck`; its bare names are pinned to the boot disk (`shellhost.rs` `pin`). `help` is shared too (x86's own commands are a table too, `kernel/src/shellx86.rs` (`tape install bootcode videomodes hda gpu midi touch power`; the chip-specific `nic park|phy|ts|csum|vlan|mcast|regs` and `usb try` are hooks the shared `nic` and `usb` rows call first), so `help` and `help NAME` need no extras; `term.rs` `execute` just runs the three tables). Tape names, `PATH/NAME` and scripts dispatch
through `fosfor_shell::external` too (`Host::exists/run_script/run_tape`; the Pi has no scripts). The common commands (`sleep uptime date keymap ps kill memory disks clear echo reset shutdown`) are one table, `kernel/src/shellcmds.rs`, with `cfg` for the few differences (RTC or software clock, PIC count in `uptime`). `reset` and `shutdown` too (a `cfg` power shim: x86 firmware/ACPI, Pi PM watchdog or PSCI). `start`, `dmesg`, `run`, `grow`, `sector`, `about` (alias `version`), `hexdump`, `sha256 [diskN/]NAME...` (SHA-256 and length of stored files, for `fosadm` over serial; over Ethernet `fosadm` prefers the signed tapesync manifest, `tools/tape_manifest.py`, when a gitignored `synckey` copy is in the repo root), `label`, `format`, `debug` (`video` on x86 BIOS boots only) and `fsck [diskN|@LABEL]` too. Commands that touch hardware follow one pattern: shared wording and loops in a kernel module, a small per-kernel `imp` block for the drivers: `nic [list|sniff|promisc|peers|discover|use|map|send|recv]` (`nicshell.rs`; the chip-specific `nic park|phy|ts|csum|vlan|mcast|regs` stay x86-only in `term.rs`), `usb [rescan]` (`usbshell.rs`; one xHCI listing), `kup serial [BAUD]` (`kupshell.rs`); `pci [read|write]`, `cpuinfo`, `bootinfo`, `memmap`, `irqstat` (Pi alias `irqs`) and `mmio` are shared too (`diag.rs`; `copy` (peer forms `HOST[:diskN]/NAME DST` and `SRC NAME HOST[:diskN]` in `netfs.rs`; locally x86 keeps its FAT-aware copy, the Pi uses `cp`), `io` stays x86, the Pi has no port space; `peek`/`poke` stay Pi bring-up aliases of `mmio`'s job). Discovery, name announcements, the peer table, the NETFS server and client and `tapesync` are shared too (`netfs.rs`, `tapesync.rs`; per-kernel `imp`: NICs, staging buffer, break key): the Pi announces itself, shows peers and serves and pulls files like x86 (hardware-untested; `copy` and `tape` stay x86). The Pi `slots` command is gone: `debug slots` is the one way, same wording on both. The rest moves into tables as its code is shared; the Pi-only ones stay in `shell.rs`.

**Line editing and Tab completion.** Both kernels' shells use one editor, `crates/kernel-core/src/lineedit.rs`: cursor keys, Home/End/Delete, Ctrl-A/E/K/U/W/C/L, history of 32 lines on Up/Down (kept for the boot, not saved), lines up to 240 bytes, the same from the local keyboard, serial and `term net`. Tab completes the word at the cursor through a hook (`lineedit::set_completer`): one match is filled in, several as far as they agree, a second Tab lists them. The mf kernel installs `fosfor_shell::complete::complete` (`crates/shell/src/complete.rs`): the first word is a command (the tables, the port's `BUILTINS`), a tape in `bin/` or a script; later words are paths (`Host::list` on any folder, `Host::volumes` for `diskN/` and `extN/`); after `help` a command name. The embedded kernel does not install a completer yet. `make test-mf-shell`, `make test-lineedit`.

## Scripting (mf)

The server kernel's shell has a small scripting language (`crates/script`, host-tested; `kernel-mf/src/script.rs` is the glue).
A prompt line that uses it goes to one interpreter that keeps its variables between lines; `NAME.sh` runs in a fresh one with
`$1..$9`, `$#`, `$*`, `"$@"`. Lines without any of `;` `&&` `||` `$` `>`, a leading `if/for/while/until`, an assignment or `#`
stay on the plain command path, so `run A | B`, `run X &2` and a `write` of quoted text behave as before.

```
X=5; Y=$((X * 2 + 1))              # variables, $((arithmetic)), no spaces around =
if [ -f data.txt ]; then ... elif ...; else ...; fi
for f in *.txt; do cp "$f" backup/; done      # globs expand in the leaf name only
while [ $N -lt 3 ]; do N=$((N + 1)); done     # also until, break [N], continue [N]
cmd && ok || failed        a; b
NAME=$(cmd; cmd)           # output of built-in commands, trailing newlines removed
echo text > file           echo more >> file  # one redirection per command, output only
set -x / set +x            # trace each command as `+ cmd`; scripts are silent by default
```

Builtins of the language: `echo [-n] set [NAME=V | -x | +x | -- ARGS] exit [N] shift [N] test/[ true false : break continue`.
`test`: `-e -f -d -z -n`, `= != -eq -ne -lt -le -gt -ge`, leading `!`. Quote with `'..'`, `".."` (expands `$`) or `\`.

Limits, on purpose: no functions, `case`, here-documents, `<` or `|` (it means split screen elsewhere; a script says so). Commands
have no exit codes: a command's status is 1 when it prints a line `NAME: message` for its own name, 127 when unknown, else 0.
`$(...)` and `>` only catch what the calling thread prints, so a tape's output (its own thread) is not caught, and a foreground
tape does not make the script wait. Loops stop after 1,000,000 rounds, or on Ctrl-C/Escape from the serial line.
