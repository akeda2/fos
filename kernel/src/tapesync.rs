//! `tapesync`: pull newer tapes from another machine, explicitly and authenticated. The protocol, the signed manifest and
//! the decision rules are in crates/tape-sync (host-tested); this file is the part that touches the disk and the network.
//!
//! Server (always on while the shell or `term net` is in charge, like NETFS): a `TAG_SYNC_REQUEST` carrying a nonce is
//! answered with the manifest of the stamped tapes on the boot volume, signed with the `synckey` file (any text, the same
//! on every machine that should trust each other; NETFS refuses to read or write that file). No `synckey`, no answer.
//! Building the manifest hashes every tape, so a second request within 2 s is refused.
//!
//! Client (`tapesync [HOST] [apply]`): asks HOST, or without one every machine in the peer table (`nic peers`) in turn,
//! checks each signature against its own nonce and key, compares with the local tapes and prints what differs; with
//! several machines the newest offer of each tape wins (equal build times with other bytes are reported, not guessed). `apply` fetches each newer or missing tape with plain NETFS and installs it only if its
//! SHA-256 and its stamp equal the signed manifest's. Files that are not stamped tapes are never touched. FOSFS rewrites
//! a file in place when it still fits its old extent, so a power cut during the write of a replaced tape can leave that
//! one tape damaged. Run `tapesync` again: the damaged file's hash differs, and (if it still reads as a tape) it is
//! replaced; otherwise copy it over with `copy`/`foscopy`.
//! The client is shared with the server kernel (`kernel_core::tapesync_client`); this file is the embedded kernel's side of it.

use crate::fosfs::Volume;
use crate::netfs::{NETFS_BUSY, NETFS_DISK_ERROR, NETFS_NO_SYNC, TAG_GET_ERROR};
use crate::timer;
use kernel_core::tapesync_client::{self as client, Env};
use tape_sync::{NONCE_LEN};

pub(crate) use client::KEY_MAX;

static mut LAST_SERVED: u64 = 0;

struct Embedded;

#[cfg(target_arch = "x86_64")]
fn cycles() -> u64 {
    unsafe { core::arch::x86_64::_rdtsc() }
}

#[cfg(target_arch = "aarch64")]
fn cycles() -> u64 {
    let count: u64;
    unsafe { core::arch::asm!("mrs {}, cntvct_el0", out(reg) count) };
    count
}

impl Env for Embedded {
    fn print(&self, line: core::fmt::Arguments) {
        println!("{}", line);
    }
    fn volume(&self) -> Option<Volume> {
        crate::storage::boot_volume()
    }
    fn staging(&self) -> Option<&'static mut [u8]> {
        crate::netfs::staging()
    }
    fn pet(&self) {
        crate::netfs::pet();
    }
    fn seed(&self) -> [u8; 16] {
        let mut seed = [0u8; 16];
        seed[0..8].copy_from_slice(&cycles().to_le_bytes());
        seed[8..16].copy_from_slice(&timer::micros().to_le_bytes());
        seed
    }
    fn peer(&self, host: &str) -> Option<([u8; 6], usize)> {
        crate::netfs::netfs_peer(host).map(|(mac, nic, _)| (mac, nic))
    }
    fn peer_count(&self) -> usize {
        crate::netfs::peer_count()
    }
    fn peer_at(&self, number: usize) -> Option<([u8; 6], [u8; 20], usize, usize)> {
        crate::netfs::peer_at(number)
    }
    fn discover(&self) {
        crate::netfs::nic_discover();
    }
    fn receive(&self, mac: [u8; 6], nic: usize, tag: u8, request: &[u8], out: &mut [u8]) -> Result<usize, &'static str> {
        crate::netfs::netfs_receive(mac, nic, tag, request, out)
    }
    fn fetch(&self, mac: [u8; 6], nic: usize, name: &str) -> Result<usize, &'static str> {
        crate::netfs::netfs_fetch(mac, nic, 1, name)
    }
}

pub(crate) fn read_key(volume: &Volume, out: &mut [u8; KEY_MAX]) -> Option<usize> {
    client::read_key(&Embedded, volume, out)
}

pub(crate) fn nonce() -> [u8; NONCE_LEN] {
    client::nonce(&Embedded)
}

/// `tapesync [HOST] [apply]`.
pub fn command(words: core::str::SplitWhitespace) {
    client::command(&Embedded, words);
}

/// A request for our manifest.
pub fn serve(index: usize, source: [u8; 6], payload: &[u8]) {
    let refuse = |code: u8| crate::netfs::netfs_reply(index, &source, TAG_GET_ERROR, &[code]);
    if payload.len() < NONCE_LEN {
        return;
    }
    let nonce: [u8; NONCE_LEN] = payload[..NONCE_LEN].try_into().unwrap();
    let Some(volume) = crate::storage::boot_volume() else { return refuse(NETFS_NO_SYNC) };
    let mut key = [0u8; KEY_MAX];
    let Some(key_length) = read_key(&volume, &mut key) else { return refuse(NETFS_NO_SYNC) };
    let recently = unsafe { LAST_SERVED } != 0 && timer::ticks().wrapping_sub(unsafe { LAST_SERVED }) < 2 * timer::TICK_HZ;
    if unsafe { client::CLIENT_ACTIVE } || recently {
        return refuse(NETFS_NO_SYNC);
    }
    if crate::netfs::netfs_busy(&source) {
        return refuse(NETFS_BUSY);
    }
    unsafe { LAST_SERVED = timer::ticks() };
    let state = client::state();
    let Ok((tapes, _)) = client::scan(&Embedded, &volume, state) else { return refuse(NETFS_DISK_ERROR) };
    let length = tape_sync::encode(&state.local[..tapes], &key[..key_length], &nonce, &mut state.wire);
    crate::netfs::netfs_stream(index, &source, &state.wire[..length], "the tape manifest");
    unsafe { LAST_SERVED = timer::ticks() };
}
