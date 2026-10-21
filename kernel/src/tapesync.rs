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

use crate::fosfs::Volume;
use crate::netdump::Mac;
use crate::netfs::{self, NETFS_BUSY, NETFS_DISK_ERROR, NETFS_NO_SYNC, TAG_GET_ERROR, TAG_SYNC_REQUEST};
use crate::{println, timer};
use tape_sync::{Action, Entry, MANIFEST_MAX, MAX_TAPES, NAME_LEN, NONCE_LEN, Stamp};

use crate::netfs::KEY_FILE;
pub(crate) const KEY_MAX: usize = 128;

struct State {
    wire: [u8; MANIFEST_MAX],
    local: [Entry; MAX_TAPES],
    remote: [Entry; MAX_TAPES],
    names: [[u8; NAME_LEN]; MAX_TAPES],
    name_lengths: [u8; MAX_TAPES],
    candidates: [Candidate; MAX_TAPES],
}

/// The best offer for one tape so far, and who made it.
#[derive(Clone, Copy)]
struct Candidate {
    entry: Entry,
    action: Action,
    mac: [u8; 6],
    nic: usize,
    source: [u8; 20],
    source_length: usize,
    /// Another machine offered other bytes under the same build time.
    conflict: bool,
}

const NO_CANDIDATE: Candidate = Candidate { entry: Entry::EMPTY, action: Action::Same, mac: [0; 6], nic: 0, source: [0; 20], source_length: 0, conflict: false };

static mut STATE: State = State {
    wire: [0; MANIFEST_MAX],
    local: [Entry::EMPTY; MAX_TAPES],
    remote: [Entry::EMPTY; MAX_TAPES],
    names: [[0; NAME_LEN]; MAX_TAPES],
    name_lengths: [0; MAX_TAPES],
    candidates: [NO_CANDIDATE; MAX_TAPES],
};
/// Set while the client command runs: a request from a peer meanwhile would reuse the buffers.
static mut CLIENT_ACTIVE: bool = false;
static mut LAST_SERVED: u64 = 0;

fn state() -> &'static mut State {
    unsafe { &mut *core::ptr::addr_of_mut!(STATE) }
}

/// The shared secret: the first line of `synckey` on the boot volume, trimmed. `None` without the file (or if it is empty
/// or over 128 bytes).
pub(crate) fn read_key(volume: &Volume, out: &mut [u8; KEY_MAX]) -> Option<usize> {
    let length = volume.read(KEY_FILE.as_bytes(), out).ok()??;
    let text = core::str::from_utf8(&out[..length]).ok()?;
    let line = text.lines().next()?.trim();
    if line.is_empty() {
        return None;
    }
    let start = line.as_ptr() as usize - text.as_ptr() as usize;
    let key_length = line.len();
    out.copy_within(start..start + key_length, 0);
    Some(key_length)
}

/// Hashes every stamped tape on `volume` into `state.local` and remembers every file name; returns (tapes, files).
fn scan(volume: &Volume, state: &mut State) -> Result<(usize, usize), &'static str> {
    let mut files = 0;
    volume
        .for_each(|entry| {
            if files < MAX_TAPES {
                let name = entry.name();
                state.names[files] = [0; NAME_LEN];
                state.names[files][..name.len()].copy_from_slice(name);
                state.name_lengths[files] = name.len() as u8;
                files += 1;
            }
        })
        .map_err(|_| "cannot read the FOSFS directory")?;
    let staging = crate::netfs::staging().ok_or("no memory for the transfer buffer")?;
    let mut tapes = 0;
    for index in 0..files {
        let name = &state.names[index][..usize::from(state.name_lengths[index])];
        crate::netfs::pet();
        let Ok(Some(length)) = volume.read(name, staging) else { continue };
        let image = &staging[..length];
        let Some(stamp) = fosfor_abi::elf::section(image, tape_sync::SECTION).and_then(Stamp::decode) else { continue };
        let entry = &mut state.local[tapes];
        entry.name = state.names[index];
        entry.length = length as u32;
        entry.stamp = stamp;
        entry.sha256 = tape_sync::sha256(image);
        tapes += 1;
    }
    Ok((tapes, files))
}

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

pub(crate) fn nonce() -> [u8; NONCE_LEN] {
    let mut seed = [0u8; 16];
    seed[0..8].copy_from_slice(&cycles().to_le_bytes());
    seed[8..16].copy_from_slice(&timer::micros().to_le_bytes());
    let digest = tape_sync::sha256(&seed);
    digest[..NONCE_LEN].try_into().unwrap()
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
    if unsafe { CLIENT_ACTIVE } || recently {
        return refuse(NETFS_NO_SYNC);
    }
    if crate::netfs::netfs_busy(&source) {
        return refuse(NETFS_BUSY);
    }
    unsafe { LAST_SERVED = timer::ticks() };
    let state = state();
    let Ok((tapes, _)) = scan(&volume, state) else { return refuse(NETFS_DISK_ERROR) };
    let length = tape_sync::encode(&state.local[..tapes], &key[..key_length], &nonce, &mut state.wire);
    crate::netfs::netfs_stream(index, &source, &state.wire[..length], "the tape manifest");
    unsafe { LAST_SERVED = timer::ticks() };
}

fn age(seconds: u64) -> (u64, &'static str) {
    match seconds {
        0..=119 => (seconds, "s"),
        120..=7199 => (seconds / 60, "min"),
        7200..=172_799 => (seconds / 3600, "h"),
        _ => (seconds / 86_400, "days"),
    }
}

/// `tapesync [HOST] [apply]`.
pub fn command(words: core::str::SplitWhitespace) {
    let usage = || println!("Usage: tapesync [HOST] [apply]   (no HOST: every machine in `nic peers`; HOST: a name, its number or a MAC address)");
    let mut host = None;
    let mut apply = false;
    for word in words {
        match word {
            "apply" if !apply => apply = true,
            _ if host.is_none() && !apply => host = Some(word),
            _ => return usage(),
        }
    }
    unsafe { CLIENT_ACTIVE = true };
    run(host, apply);
    unsafe { CLIENT_ACTIVE = false };
}

struct Target {
    mac: [u8; 6],
    nic: usize,
    name: [u8; 20],
    name_length: usize,
}

impl Target {
    fn name(&self) -> &str {
        core::str::from_utf8(&self.name[..self.name_length]).unwrap_or("?")
    }
}

fn run(host: Option<&str>, apply: bool) {
    let Some(volume) = crate::storage::boot_volume() else { return println!("tapesync: no boot volume.") };
    let mut key = [0u8; KEY_MAX];
    let Some(key_length) = read_key(&volume, &mut key) else {
        return println!("tapesync: no `{KEY_FILE}` file on the boot volume (one line of text, the same on every machine).");
    };
    let key = &key[..key_length];
    let state = state();
    let (local_count, file_count) = match scan(&volume, state) {
        Ok(counts) => counts,
        Err(reason) => return println!("tapesync: {reason}."),
    };
    let mut candidates = 0;
    state.candidates.fill(NO_CANDIDATE);
    if let Some(host) = host {
        let Some((mac, nic, _)) = crate::netfs::netfs_peer(host) else { return println!("tapesync: no machine called {host} answered (see `nic peers`).") };
        let mut target = Target { mac, nic, name: [0; 20], name_length: host.len().min(20) };
        target.name[..target.name_length].copy_from_slice(&host.as_bytes()[..target.name_length]);
        ask(&target, key, state, (local_count, file_count), &mut candidates, true);
    } else {
        if crate::netfs::peer_count() == 0 {
            crate::netfs::nic_discover();
        }
        if crate::netfs::peer_count() == 0 {
            return println!("tapesync: no other machine is known (see `nic peers`).");
        }
        for number in 1..=crate::netfs::peer_count() {
            let Some((mac, name, name_length, nic)) = crate::netfs::peer_at(number) else { continue };
            let target = Target { mac, nic, name, name_length };
            ask(&target, key, state, (local_count, file_count), &mut candidates, false);
        }
    }
    if candidates == 0 {
        return println!("Nothing to pull ({local_count} tapes here).");
    }
    if host.is_none() {
        for candidate in &state.candidates[..candidates] {
            let name = core::str::from_utf8(candidate.entry.name_bytes()).unwrap_or("?");
            let source = core::str::from_utf8(&candidate.source[..candidate.source_length]).unwrap_or("?");
            let note = if candidate.conflict { " (another machine has other bytes of the same build)" } else { "" };
            match candidate.action {
                Action::New => println!("{name:<16} new here, from {source}{note}"),
                _ => println!("{name:<16} newer there, from {source}{note}"),
            }
        }
    }
    let new = state.candidates[..candidates].iter().filter(|candidate| candidate.action == Action::New).count();
    let free_entries = MAX_TAPES - file_count;
    if new > free_entries {
        return println!("tapesync: {new} new tapes need {new} free directory entries, only {free_entries} of {MAX_TAPES} are free; nothing done.");
    }
    if !apply {
        return match host {
            Some(host) => println!("{candidates} tape(s) would be pulled. Run `tapesync {host} apply` to do it."),
            None => println!("{candidates} tape(s) would be pulled. Run `tapesync apply` to do it."),
        };
    }
    let (mut done, mut failed) = (0, 0);
    for index in 0..candidates {
        let candidate = state.candidates[index];
        let remote = candidate.entry;
        let name = core::str::from_utf8(remote.name_bytes()).unwrap_or("?");
        let length = match crate::netfs::netfs_fetch(candidate.mac, candidate.nic, 1, name) {
            Ok(length) => length,
            Err(reason) => {
                println!("{name}: {reason}.");
                failed += 1;
                continue;
            }
        };
        let image = &crate::netfs::staging().unwrap()[..length];
        let genuine = length == remote.length as usize
            && tape_sync::equal(&tape_sync::sha256(image), &remote.sha256)
            && fosfor_abi::elf::section(image, tape_sync::SECTION).and_then(Stamp::decode) == Some(remote.stamp);
        if !genuine {
            println!("{name}: what arrived is not what the manifest signed; not installed.");
            failed += 1;
            continue;
        }
        match volume.write(remote.name_bytes(), image) {
            Ok(()) => {
                println!("{name}: installed ({length} bytes).");
                done += 1;
            }
            Err(error) => {
                println!("{name}: cannot write it ({error:?}).");
                failed += 1;
            }
        }
    }
    println!("tapesync: {done} installed, {failed} failed.");
}

/// Asks one machine for its manifest and merges what it offers into `state.candidates`. `verbose` lists every tape it has.
fn ask(target: &Target, key: &[u8], state: &mut State, (local_count, file_count): (usize, usize), candidates: &mut usize, verbose: bool) {
    let name = target.name();
    let nonce = nonce();
    println!("Asking {name} ({}) for its tape manifest...", Mac(&target.mac));
    let received = match crate::netfs::netfs_receive(target.mac, target.nic, TAG_SYNC_REQUEST, &nonce, &mut state.wire) {
        Ok(length) => length,
        Err(reason) => return println!("  {name}: {reason}."),
    };
    let remote_count = match tape_sync::decode(&state.wire[..received], key, &nonce, &mut state.remote) {
        Ok(count) => count,
        Err(tape_sync::Error::BadSignature) => return println!("  {name}: the manifest is not signed with our key (another synckey there, or a forgery); ignored."),
        Err(tape_sync::Error::Malformed) => return println!("  {name}: the answer is not a tape manifest; ignored."),
    };
    let abi = fosfor_abi::tape::TAPE_API_VERSION;
    let mut offered = 0;
    for index in 0..remote_count {
        let remote = state.remote[index];
        let tape = remote.name_bytes();
        let local = state.local[..local_count].iter().find(|entry| entry.name_bytes() == tape).copied();
        let exists = state.names[..file_count].iter().zip(&state.name_lengths).any(|(known, &length)| &known[..usize::from(length)] == tape);
        let action = tape_sync::decide(&remote, local.as_ref(), exists, abi, tape_sync::HOST_ARCH);
        let label = core::str::from_utf8(tape).unwrap_or("?");
        if verbose {
            match (action, local) {
                (Action::New, _) => println!("{label:<16} new here"),
                (Action::Newer, Some(mine)) => {
                    let (amount, unit) = age(remote.stamp.build - mine.stamp.build);
                    println!("{label:<16} newer there by {amount} {unit}")
                }
                (Action::Same, _) => println!("{label:<16} the same"),
                (Action::Keep, _) => println!("{label:<16} ours is newer; kept"),
                (Action::Differs, _) => println!("{label:<16} same build time, other bytes; kept"),
                (Action::TooNew, _) => println!("{label:<16} needs tape ABI {} (this kernel has {abi}); skipped", remote.stamp.abi),
                (Action::OtherCpu, _) => println!("{label:<16} built for another CPU; skipped"),
                (Action::NotATape, _) => println!("{label:<16} a file of that name here is not a tape; skipped"),
                (Action::BadName, _) | (Action::Newer, None) => println!("{label:<16} not usable; skipped"),
            }
        }
        if !matches!(action, Action::New | Action::Newer) {
            continue;
        }
        offered += 1;
        let mut offer = NO_CANDIDATE;
        offer.entry = remote;
        offer.action = action;
        offer.mac = target.mac;
        offer.nic = target.nic;
        offer.source = target.name;
        offer.source_length = target.name_length;
        match state.candidates[..*candidates].iter().position(|held| held.entry.name_bytes() == tape) {
            Some(at) => {
                let held = &mut state.candidates[at];
                if remote.stamp.build > held.entry.stamp.build {
                    *held = offer;
                } else if remote.stamp.build == held.entry.stamp.build && remote.sha256 != held.entry.sha256 {
                    held.conflict = true;
                }
            }
            None if *candidates < MAX_TAPES => {
                state.candidates[*candidates] = offer;
                *candidates += 1;
            }
            None => {}
        }
    }
    if !verbose {
        println!("  {name}: {remote_count} tapes, {offered} newer or missing here.");
    }
}
