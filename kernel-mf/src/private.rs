//! The hook for the optional private drivers (docs/private.md): drivers and tapes kept in a separate, private repository
//! checked out at `private/` in this tree. The Makefile builds with the `private` feature when it is there; without it
//! (any public build) this module is empty and the kernel has no trace of them.
//!
//! What the private tree's `kernel-mf/mod.rs` provides, and all the public kernel knows of it:
//! - `init()`, called at boot after the built-in audio devices, to probe its hardware and register what it finds
//!   (an audio card through `kernel_core::ext_audio::set`);
//! - `command(name, words) -> bool`, offered every shell command the built-in table does not know, true if it took it;
//! - `TOPICS`, its `help` entries, also used for Tab completion.
//!
//! Its modules are part of this crate, so they use the kernel's own modules (`crate::pci`, `crate::pmm`, ...) like any
//! driver here; a crate they need is a module of theirs (no change to `Cargo.toml` or `Cargo.lock`).

#[cfg(feature = "private")]
#[path = "../../private/kernel-mf/mod.rs"]
mod drivers;

#[cfg(feature = "private")]
pub use drivers::{TOPICS, command, init};

#[cfg(not(feature = "private"))]
pub const TOPICS: &[crate::helptext::Topic] = &[];

#[cfg(not(feature = "private"))]
pub fn init() {}

#[cfg(not(feature = "private"))]
pub fn command(_name: &str, _words: core::str::SplitWhitespace) -> bool {
    false
}
