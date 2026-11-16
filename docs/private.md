# The private overlay

Some drivers and tapes are kept out of this public repository. They live in a separate repository checked out at
`private/` in the source tree (the directory is in `.gitignore`). When it is there the build includes it; without it
everything builds and runs as before, minus those parts. Nothing in the public tree names what the private tree
holds.

## Switching it on and off

The Makefile sets `PRIVATE=1` when `private/private.mk` exists, otherwise `PRIVATE=0`. Override it on the command line:
`make PRIVATE=0 kernel-mf` builds the public kernel with a private tree present. Then:

- the server kernel (`kernel-mf`, x86_64 and aarch64) is built with the cargo feature `private`;
- `private/private.mk` is included;
- tapes listed in its `PRIVATE_TAPES`, `PRIVATE_FP_TAPES` (hard-float) and `PRIVATE_A64_TAPES` (aarch64) are built
  like the public ones, from `private/tapes/NAME` when `tapes/NAME` does not exist;
- `make test` also runs `make test-private`, which `private.mk` defines.

`make release` always builds with `PRIVATE=0` and starts from empty stamped-tape folders, so a release never contains
private code.

## The kernel hook

`kernel-mf/src/private.rs` is the only place the kernel touches the private tree. With the feature it includes
`private/kernel-mf/mod.rs` as a module of the kernel (`#[path]`), so private code uses the kernel's own modules
(`crate::pci`, `crate::pmm`, ...) and needs no entry in `Cargo.toml` or `Cargo.lock`. That file provides:

| Item | Used by |
| --- | --- |
| `pub fn init()` | `main.rs`, after the public audio drivers (x86) or after the config is read (aarch64) |
| `pub fn command(name: &str, words: SplitWhitespace) -> bool` | `shell.rs`, for a command the public table does not know |
| `pub const TOPICS: &[helptext::Topic]` | `help`, `help NAME` and tab completion |

Without the feature `private.rs` has empty versions of all three. A PCI audio card driver registers itself through
the public `kernel_core::ext_audio` interface, so the `audio` shell command and the audio tapes see it like any
other device (`AUDIO_KIND_CARD`).

## Keeping names out of the public tree

`private/denylist.txt` holds regular expressions (one per line, case-insensitive, `#` comments) for the names that
must not appear in public files. `tools/check_public.py` checks every tracked file and the messages of commits not yet
pushed, and fails on a match; `make test` runs it. Without a private tree it has nothing to compare and passes. Run it
before every push from a tree that has the overlay.
