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

Status: the Pi shell uses the core. The x86 shell (`term.rs`) does not yet: next steps are its file commands (`ls cat write
rm`, `fsck`, `copy` with peers), then `help`, `run`/tape names and scripts, then the common hardware-free commands
(`sleep`, `uptime`, `keymap`, `date`, `ps`, `kill`). The extra Pi commands move to the shared table when x86 has them.
