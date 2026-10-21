# The shared shell

One shell core for every port: `crates/shell` (`fosfor_shell`, `no_std`, host-tested with `cargo test`, part of `make test`).

- **`Host`** is what a port provides: `print`, file calls on references (`NAME`, `diskN/NAME`, `@LABEL/NAME`, `esp/NAME`),
  `list`, a scratch buffer and `tape_help`. `kernel/src/shellhost.rs` (`KernelHost`) implements it for both kernels on
  top of `tape_fs`, so the shell sees the volumes a tape sees.
- **`Command`** is a table row: name, aliases, usage, summary, handler. `fosfor_shell::execute(host, &[PORT, files::COMMANDS], line)`
  looks a line up (a port's table first, so it may override) and builds `help` / `help NAME` from the tables; for a name
  that is no command `help NAME` shows a tape's `.fos_help`.
- **Shared commands** (`files.rs`): `ls cat write rm cp (copy) cmp`. The older `cat diskN NAME` spelling still means `diskN/NAME`.
  `cp SRC DST` accepts a bare volume as DST (keeps the name); files up to 32 MiB.
- **A port** keeps only what is hardware- or port-specific, as its own table (Pi: `kernel/src/shell.rs`), plus what to do with
  a line that is no command (tape names, `PATH/NAME`).

Status: both shells use the core for the file commands (`ls cat write rm cp cmp`). x86 keeps its own `copy` (peers,
NETFS) and `fsck`; its bare names are pinned to the boot disk (`shellhost.rs` `pin`). `help` is shared too (x86 lists its not-yet-shared commands through `Host::help_extra`). Tape names, `PATH/NAME` and scripts dispatch
through `fosfor_shell::external` too (`Host::exists/run_script/run_tape`; the Pi has no scripts). The common commands (`sleep uptime date keymap ps kill memory disks clear echo reset shutdown`) are one table, `kernel/src/shellcmds.rs`, with `cfg` for the few differences (RTC or software clock, PIC count in `uptime`). `reset` and `shutdown` too (a `cfg` power shim: x86 firmware/ACPI, Pi PM watchdog or PSCI). `start`, `dmesg`, `run`, `grow`, `sector`, `about` (alias `version`) and `hexdump` too. into tables as their code is shared; the Pi-only ones stay in `shell.rs`.
