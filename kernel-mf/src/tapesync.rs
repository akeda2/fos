//! Tape sync, server side: a machine that shares the `synckey` file answers a nonce (tag 0x40) with the signed manifest of the
//! stamped tapes on its boot volume, in the format of the embedded kernel's `tapesync.rs` (the format, signature and
//! decision rules are in `crates/tape-sync`, host-tested). A peer's `tapesync` command then fetches what is newer with plain
//! NETFS. No `synckey`, no answer; the key itself is never served (`netfs.rs`). The `tapesync` client command is not part of
//! the server kernel.
//!
//! Building the manifest hashes every tape, so a second request within 2 s is refused.

use alloc::vec::Vec;
use kernel_core::fosfs::Volume;
use kernel_core::volumes;
use tape_sync::{Entry, MANIFEST_MAX, MAX_TAPES, NAME_LEN, NONCE_LEN, Stamp};

use crate::apic::{self, TICK_HZ};
use crate::netfs::{self, KEY_FILE, TAG_GET_ERROR};
use crate::tapefs;

const NO_SYNC: u8 = 10;
const BUSY: u8 = 9;
const DISK_ERROR: u8 = 7;
const KEY_MAX: usize = 128;
/// Largest tape hashed; a bigger file is not a tape.
const TAPE_LIMIT: u64 = 8 << 20;

static mut LAST_SERVED: u64 = 0;

/// The shared secret: the first line of `synckey`, trimmed; `None` without the file (or if it is empty or over 128 bytes).
fn read_key(volume: &Volume, out: &mut [u8; KEY_MAX]) -> Option<usize> {
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

/// The names of the root's files, then those of `bin/` not already seen (a v4 volume keeps its tapes there).
fn names(volume: &Volume) -> Result<Vec<([u8; NAME_LEN], usize)>, ()> {
    let mut names: Vec<([u8; NAME_LEN], usize)> = Vec::new();
    let mut add = |name: &[u8], is_dir: bool| {
        if names.len() < MAX_TAPES && !is_dir && name.len() <= NAME_LEN && !names.iter().any(|(known, length)| &known[..*length] == name) {
            let mut stored = [0u8; NAME_LEN];
            stored[..name.len()].copy_from_slice(name);
            names.push((stored, name.len()));
        }
    };
    volume.for_each(|entry| add(entry.name(), entry.is_dir)).map_err(|_| ())?;
    let _ = volume.for_each_in(b"bin", |entry| add(entry.name(), entry.is_dir));
    Ok(names)
}

/// Hashes every stamped tape of `volume`. The disk lock is held per file, not for the whole scan.
fn scan(volume: &Volume) -> Result<Vec<Entry>, ()> {
    let mut entries = Vec::new();
    for (stored, length) in names(volume)? {
        crate::debug::pet();
        let name = &stored[..length];
        let image = {
            let _disk = tapefs::lock();
            let mut path = [0u8; 4 + NAME_LEN];
            path[..4].copy_from_slice(b"bin/");
            path[4..4 + length].copy_from_slice(name);
            let path = match volume.file_length(name) {
                Ok(Some(_)) => name,
                _ => &path[..4 + length],
            };
            let size = match volume.file_length(path) {
                Ok(Some(size)) if size > 0 && size <= TAPE_LIMIT => size as usize,
                _ => continue,
            };
            let mut image = alloc::vec![0u8; size];
            match volume.read(path, &mut image) {
                Ok(Some(read)) => image.truncate(read),
                _ => continue,
            }
            image
        };
        let Some(stamp) = fosfor_abi::elf::section(&image, tape_sync::SECTION).and_then(Stamp::decode) else { continue };
        entries.push(Entry { name: stored, length: image.len() as u32, stamp, sha256: tape_sync::sha256(&image) });
    }
    Ok(entries)
}

/// A request for our manifest.
pub fn serve(index: usize, source: [u8; 6], payload: &[u8]) {
    let refuse = |code: u8| netfs::reply(index, &source, TAG_GET_ERROR, &[code]);
    if payload.len() < NONCE_LEN {
        return;
    }
    let nonce: [u8; NONCE_LEN] = payload[..NONCE_LEN].try_into().unwrap();
    let mut key = [0u8; KEY_MAX];
    let Some(key_length) = ({
        let _disk = tapefs::lock();
        volumes::boot_volume().and_then(|volume| read_key(&volume, &mut key))
    }) else {
        return refuse(NO_SYNC);
    };
    let recently = unsafe { LAST_SERVED } != 0 && apic::ticks().wrapping_sub(unsafe { LAST_SERVED }) < 2 * TICK_HZ;
    if recently {
        return refuse(NO_SYNC);
    }
    if netfs::busy(&source) {
        return refuse(BUSY);
    }
    unsafe { LAST_SERVED = apic::ticks() };
    let Some(volume) = volumes::boot_volume() else { return refuse(NO_SYNC) };
    let Ok(entries) = scan(&volume) else { return refuse(DISK_ERROR) };
    let mut wire = alloc::vec![0u8; MANIFEST_MAX];
    let wire: &mut [u8; MANIFEST_MAX] = wire.as_mut_slice().try_into().unwrap();
    let length = tape_sync::encode(&entries, &key[..key_length], &nonce, wire);
    netfs::stream(index, &source, &wire[..length], "the tape manifest");
    unsafe { LAST_SERVED = apic::ticks() };
}
