//! The tape-sync client, shared by the embedded kernel and the server kernel (`kernel-mf`): `tapesync [HOST] [apply]` pulls newer
//! tapes from another machine, explicitly and authenticated. The protocol, the signed manifest and the decision rules are in
//! `crates/tape-sync` (host-tested); this file is the command itself, and `Env` is everything that differs between the kernels
//! (console, boot volume, transfer buffer, peer table, the NETFS receive loop).
//!
//! The client asks HOST, or without one every machine in the peer table (`nic peers`) in turn, checks each signature against its
//! own nonce and key, compares with the local tapes and prints what differs; with several machines the newest offer of each tape
//! wins (equal build times with other bytes are reported, not guessed). `apply` fetches each newer or missing tape with plain
//! NETFS and installs it only if its SHA-256 and its stamp equal the signed manifest's. Files that are not stamped tapes are never
//! touched. FOSFS rewrites a file in place when it still fits its old extent, so a power cut during the write of a replaced tape
//! can leave that one tape damaged. Run `tapesync` again: the damaged file's hash differs, and (if it still reads as a tape) it is
//! replaced; otherwise copy it over with `copy`/`foscopy`.

use crate::fosfs::Volume;
use crate::netdump::Mac;
use tape_sync::{Action, Entry, MANIFEST_MAX, MAX_TAPES, NAME_LEN, NONCE_LEN, Stamp};

/// The tape-sync request: a nonce in, a signed manifest out as a GET stream.
pub const TAG_SYNC_REQUEST: u8 = 0x40;
/// The shared secret file; NETFS never reads or writes it.
pub const KEY_FILE: &str = "synckey";
pub const KEY_MAX: usize = 128;

/// What the client needs from its kernel.
pub trait Env {
    fn print(&self, arguments: core::fmt::Arguments);
    /// The boot volume.
    fn volume(&self) -> Option<Volume>;
    /// A transfer buffer big enough for the largest tape; a fetched file is left in it.
    fn staging(&self) -> Option<&'static mut [u8]>;
    /// Keeps a watchdog quiet during long work.
    fn pet(&self);
    /// Runs `work` with whatever lock orders disk access with the rest of the kernel.
    fn disk<R>(&self, work: impl FnOnce() -> R) -> R {
        work()
    }
    /// 16 bytes that differ from boot to boot and call to call: the nonce's seed.
    fn seed(&self) -> [u8; 16];
    /// HOST (a name, a peer number or a MAC) to the machine's MAC and the interface to reach it on.
    fn peer(&self, host: &str) -> Option<([u8; 6], usize)>;
    fn peer_count(&self) -> usize;
    /// Peer `number` (1-based): MAC, name, name length, interface.
    fn peer_at(&self, number: usize) -> Option<([u8; 6], [u8; 20], usize, usize)>;
    /// Asks the network who is there (fills the peer table).
    fn discover(&self);
    /// Sends a GET-style request and collects the START/DATA/STOP answer into `out`.
    fn receive(&self, mac: [u8; 6], nic: usize, tag: u8, request: &[u8], out: &mut [u8]) -> Result<usize, &'static str>;
    /// Fetches file `name` of disk 1 into the transfer buffer; returns its length.
    fn fetch(&self, mac: [u8; 6], nic: usize, name: &str) -> Result<usize, &'static str>;
}

macro_rules! say {
    ($env:expr, $($arguments:tt)*) => {
        $env.print(format_args!($($arguments)*))
    };
}

pub struct State {
    pub wire: [u8; MANIFEST_MAX],
    pub local: [Entry; MAX_TAPES],
    pub remote: [Entry; MAX_TAPES],
    pub names: [[u8; NAME_LEN]; MAX_TAPES],
    pub name_lengths: [u8; MAX_TAPES],
    pub candidates: [Candidate; MAX_TAPES],
}

/// The best offer for one tape so far, and who made it.
#[derive(Clone, Copy)]
pub struct Candidate {
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
/// Set while the client command runs: a request from a peer meanwhile would reuse the buffers (the embedded server checks it).
pub static mut CLIENT_ACTIVE: bool = false;

pub fn state() -> &'static mut State {
    unsafe { &mut *core::ptr::addr_of_mut!(STATE) }
}

/// The shared secret: the first line of `synckey` on the boot volume, trimmed. `None` without the file (or if it is empty
/// or over 128 bytes).
pub fn read_key<E: Env>(env: &E, volume: &Volume, out: &mut [u8; KEY_MAX]) -> Option<usize> {
    let length = env.disk(|| volume.read(KEY_FILE.as_bytes(), out)).ok()??;
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
pub fn scan<E: Env>(env: &E, volume: &Volume, state: &mut State) -> Result<(usize, usize), &'static str> {
    let mut files = 0;
    env.disk(|| {
        volume.for_each(|entry| {
            if files < MAX_TAPES {
                let name = entry.name();
                state.names[files] = [0; NAME_LEN];
                state.names[files][..name.len()].copy_from_slice(name);
                state.name_lengths[files] = name.len() as u8;
                files += 1;
            }
        })
    })
    .map_err(|_| "cannot read the FOSFS directory")?;
    // A v4 volume keeps its tapes in bin/; a name already seen in the root is that file.
    let _ = env.disk(|| {
        volume.for_each_in(b"bin", |entry| {
            let name = entry.name();
            let known = state.names[..files].iter().zip(&state.name_lengths).any(|(known, &length)| &known[..usize::from(length)] == name);
            if files < MAX_TAPES && !known && !entry.is_dir && name.len() <= NAME_LEN {
                state.names[files] = [0; NAME_LEN];
                state.names[files][..name.len()].copy_from_slice(name);
                state.name_lengths[files] = name.len() as u8;
                files += 1;
            }
        })
    });
    let staging = env.staging().ok_or("no memory for the transfer buffer")?;
    let mut tapes = 0;
    for index in 0..files {
        let name = &state.names[index][..usize::from(state.name_lengths[index])];
        env.pet();
        let length = env.disk(|| match volume.read(name, staging) {
            Ok(Some(length)) => Some(length),
            _ => {
                let mut path = [0u8; 4 + NAME_LEN];
                path[..4].copy_from_slice(b"bin/");
                path[4..4 + name.len()].copy_from_slice(name);
                match volume.read(&path[..4 + name.len()], staging) {
                    Ok(Some(length)) => Some(length),
                    _ => None,
                }
            }
        });
        let Some(length) = length else { continue };
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

pub fn nonce<E: Env>(env: &E) -> [u8; NONCE_LEN] {
    let digest = tape_sync::sha256(&env.seed());
    digest[..NONCE_LEN].try_into().unwrap()
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
pub fn command<E: Env>(env: &E, words: core::str::SplitWhitespace) {
    let usage = || say!(env, "Usage: tapesync [HOST] [apply]   (no HOST: every machine in `nic peers`; HOST: a name, its number or a MAC address)");
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
    run(env, host, apply);
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

fn run<E: Env>(env: &E, host: Option<&str>, apply: bool) {
    let Some(volume) = env.volume() else { return say!(env, "tapesync: no boot volume.") };
    let mut key = [0u8; KEY_MAX];
    let Some(key_length) = read_key(env, &volume, &mut key) else {
        return say!(env, "tapesync: no `{KEY_FILE}` file on the boot volume (one line of text, the same on every machine).");
    };
    let key = &key[..key_length];
    let state = state();
    let (local_count, file_count) = match scan(env, &volume, state) {
        Ok(counts) => counts,
        Err(reason) => return say!(env, "tapesync: {reason}."),
    };
    let mut candidates = 0;
    state.candidates.fill(NO_CANDIDATE);
    if let Some(host) = host {
        let Some((mac, nic)) = env.peer(host) else { return say!(env, "tapesync: no machine called {host} answered (see `nic peers`).") };
        let mut target = Target { mac, nic, name: [0; 20], name_length: host.len().min(20) };
        target.name[..target.name_length].copy_from_slice(&host.as_bytes()[..target.name_length]);
        ask(env, &target, key, state, (local_count, file_count), &mut candidates, true);
    } else {
        if env.peer_count() == 0 {
            env.discover();
        }
        if env.peer_count() == 0 {
            return say!(env, "tapesync: no other machine is known (see `nic peers`).");
        }
        for number in 1..=env.peer_count() {
            let Some((mac, name, name_length, nic)) = env.peer_at(number) else { continue };
            let target = Target { mac, nic, name, name_length };
            ask(env, &target, key, state, (local_count, file_count), &mut candidates, false);
        }
    }
    if candidates == 0 {
        return say!(env, "Nothing to pull ({local_count} tapes here).");
    }
    if host.is_none() {
        for candidate in &state.candidates[..candidates] {
            let name = core::str::from_utf8(candidate.entry.name_bytes()).unwrap_or("?");
            let source = core::str::from_utf8(&candidate.source[..candidate.source_length]).unwrap_or("?");
            let note = if candidate.conflict { " (another machine has other bytes of the same build)" } else { "" };
            match candidate.action {
                Action::New => say!(env, "{name:<16} new here, from {source}{note}"),
                _ => say!(env, "{name:<16} newer there, from {source}{note}"),
            }
        }
    }
    let new = state.candidates[..candidates].iter().filter(|candidate| candidate.action == Action::New).count();
    let free_entries = MAX_TAPES - file_count;
    if new > free_entries {
        return say!(env, "tapesync: {new} new tapes need {new} free directory entries, only {free_entries} of {MAX_TAPES} are free; nothing done.");
    }
    if !apply {
        return match host {
            Some(host) => say!(env, "{candidates} tape(s) would be pulled. Run `tapesync {host} apply` to do it."),
            None => say!(env, "{candidates} tape(s) would be pulled. Run `tapesync apply` to do it."),
        };
    }
    let (mut done, mut failed) = (0, 0);
    for index in 0..candidates {
        let candidate = state.candidates[index];
        let remote = candidate.entry;
        let name = core::str::from_utf8(remote.name_bytes()).unwrap_or("?");
        let length = match env.fetch(candidate.mac, candidate.nic, name) {
            Ok(length) => length,
            Err(reason) => {
                say!(env, "{name}: {reason}.");
                failed += 1;
                continue;
            }
        };
        let image = &env.staging().unwrap()[..length];
        let genuine = length == remote.length as usize
            && tape_sync::equal(&tape_sync::sha256(image), &remote.sha256)
            && fosfor_abi::elf::section(image, tape_sync::SECTION).and_then(Stamp::decode) == Some(remote.stamp);
        if !genuine {
            say!(env, "{name}: what arrived is not what the manifest signed; not installed.");
            failed += 1;
            continue;
        }
        // Replace the file where it lives: the root's if it has one, else bin/ on a volume that has that folder.
        let mut path = [0u8; 4 + NAME_LEN];
        let target = remote.name_bytes();
        let in_root = env.disk(|| matches!(volume.file_length(target), Ok(Some(_))));
        let target = if !in_root && env.disk(|| volume.for_each_in(b"bin", |_| {}).is_ok()) && target.len() <= NAME_LEN {
            path[..4].copy_from_slice(b"bin/");
            path[4..4 + target.len()].copy_from_slice(target);
            &path[..4 + target.len()]
        } else {
            target
        };
        match env.disk(|| volume.write(target, image)) {
            Ok(()) => {
                say!(env, "{name}: installed ({length} bytes).");
                done += 1;
            }
            Err(error) => {
                say!(env, "{name}: cannot write it ({error:?}).");
                failed += 1;
            }
        }
    }
    say!(env, "tapesync: {done} installed, {failed} failed.");
}

/// Asks one machine for its manifest and merges what it offers into `state.candidates`. `verbose` lists every tape it has.
fn ask<E: Env>(env: &E, target: &Target, key: &[u8], state: &mut State, (local_count, file_count): (usize, usize), candidates: &mut usize, verbose: bool) {
    let name = target.name();
    let nonce = nonce(env);
    say!(env, "Asking {name} ({}) for its tape manifest...", Mac(&target.mac));
    let received = match env.receive(target.mac, target.nic, TAG_SYNC_REQUEST, &nonce, &mut state.wire) {
        Ok(length) => length,
        Err(reason) => return say!(env, "  {name}: {reason}."),
    };
    let remote_count = match tape_sync::decode(&state.wire[..received], key, &nonce, &mut state.remote) {
        Ok(count) => count,
        Err(tape_sync::Error::BadSignature) => return say!(env, "  {name}: the manifest is not signed with our key (another synckey there, or a forgery); ignored."),
        Err(tape_sync::Error::Malformed) => return say!(env, "  {name}: the answer is not a tape manifest; ignored."),
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
                (Action::New, _) => say!(env, "{label:<16} new here"),
                (Action::Newer, Some(mine)) => {
                    let (amount, unit) = age(remote.stamp.build - mine.stamp.build);
                    say!(env, "{label:<16} newer there by {amount} {unit}")
                }
                (Action::Same, _) => say!(env, "{label:<16} the same"),
                (Action::Keep, _) => say!(env, "{label:<16} ours is newer; kept"),
                (Action::Differs, _) => say!(env, "{label:<16} same build time, other bytes; kept"),
                (Action::TooNew, _) => say!(env, "{label:<16} needs tape ABI {} (this kernel has {abi}); skipped", remote.stamp.abi),
                (Action::OtherCpu, _) => say!(env, "{label:<16} built for another CPU; skipped"),
                (Action::NotATape, _) => say!(env, "{label:<16} a file of that name here is not a tape; skipped"),
                (Action::BadName, _) | (Action::Newer, None) => say!(env, "{label:<16} not usable; skipped"),
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
        say!(env, "  {name}: {remote_count} tapes, {offered} newer or missing here.");
    }
}
