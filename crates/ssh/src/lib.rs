//! A small SSH-2 server and SFTP v3 for tapes, over any byte stream: `session::Session` is the connection state machine,
//! `sftp::Sftp` the file protocol over `vfs_export::Files`. The `sshd` tape is the TCP front end; `examples/serve.rs` runs
//! the same code on a host directory so the real `ssh`, `sftp` and `sshfs` can be tried against it.
#![cfg_attr(not(test), no_std)]

pub mod authorized;
pub mod client;
pub mod session;
pub mod sftp;
pub mod wire;

pub use session::{ARENA, Account, Host, Session, ShellRead};
pub use sftp::{Exports, Step};
