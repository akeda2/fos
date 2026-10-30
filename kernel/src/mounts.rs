//! The mount table (`docs/fosfs-v4.md`): a few folder paths of the boot volume that live on another FOSFS volume,
//! named by label so a stick keeps working when the disk numbers change. A path under a mount point goes to that
//! volume with the mount path cut off (`config/phono.cfg` is `phono.cfg` on `@cfgstick`); the longest mount point
//! wins. The label is looked up at every use, so a stick that appears later starts working at the next lookup and one
//! that goes away is simply not there: a read then falls back to the boot volume's own copy of the folder, and a write
//! goes to the boot volume (which makes the folders on the way). That is the whole fallback; a machine with one drive
//! needs no mount at all, because `config/` is then an ordinary folder of the root.
//!
//! The table is filled from the boot volume's `fstab` at boot (`load`), one `PATH @LABEL` per line, `#` starts a
//! comment; the `mount` and `umount` shell commands change it until the next boot (edit `fstab` to keep a change).
//! Only FOSFS volumes can be mounted so far: exFAT needs subfolders first (step 3).

use crate::fosfs;

const MAX_MOUNTS: usize = 8;
const LABEL_MAX: usize = 16;
const FSTAB_MAX: usize = 1024;

#[derive(Clone, Copy)]
struct Mount {
    path: [u8; fosfs::MAX_NAME],
    path_length: usize,
    label: [u8; LABEL_MAX],
    label_length: usize,
}

static mut TABLE: [Option<Mount>; MAX_MOUNTS] = [None; MAX_MOUNTS];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The mount point is not a valid folder path (or is longer than a name).
    BadPath,
    BadLabel,
    Full,
}

/// A volume label, copied out of the table.
#[derive(Clone, Copy)]
pub struct Label {
    bytes: [u8; LABEL_MAX],
    length: usize,
}

impl Label {
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.length]
    }
}

fn table() -> &'static mut [Option<Mount>; MAX_MOUNTS] {
    unsafe { &mut *core::ptr::addr_of_mut!(TABLE) }
}

/// Mounts the volume labelled `label` at the folder `path`; an existing mount of that path is replaced.
pub fn add(path: &[u8], label: &[u8]) -> Result<(), Error> {
    if path.len() > fosfs::MAX_NAME || !fosfs::valid_path(path) {
        return Err(Error::BadPath);
    }
    if !fosfs::valid_label(label) {
        return Err(Error::BadLabel);
    }
    let mut entry = Mount { path: [0; fosfs::MAX_NAME], path_length: path.len(), label: [0; LABEL_MAX], label_length: label.len() };
    entry.path[..path.len()].copy_from_slice(path);
    entry.label[..label.len()].copy_from_slice(label);
    let slots = table();
    let at = slots.iter().position(|slot| slot.is_some_and(|m| &m.path[..m.path_length] == path)).or_else(|| slots.iter().position(Option::is_none));
    slots[at.ok_or(Error::Full)?] = Some(entry);
    Ok(())
}

/// Removes the mount at `path`; false if there was none.
pub fn remove(path: &[u8]) -> bool {
    let slots = table();
    match slots.iter().position(|slot| slot.is_some_and(|m| &m.path[..m.path_length] == path)) {
        Some(at) => {
            slots[at] = None;
            true
        }
        None => false,
    }
}

pub fn clear() {
    *table() = [None; MAX_MOUNTS];
}

/// Calls `visit(path, label)` for every mount.
pub fn each(mut visit: impl FnMut(&[u8], &[u8])) {
    for mount in table().iter().flatten() {
        visit(&mount.path[..mount.path_length], &mount.label[..mount.label_length]);
    }
}

/// The longest mount point that `reference` lies at or under: its label and what is left of the path (empty for the
/// mount point itself).
pub fn lookup_dir(reference: &[u8]) -> Option<(Label, &[u8])> {
    let mut best: Option<(&Mount, usize)> = None;
    for mount in table().iter().flatten() {
        let path = &mount.path[..mount.path_length];
        let under = reference == path || (reference.len() > path.len() && reference.starts_with(path) && reference[path.len()] == b'/');
        if under && best.is_none_or(|(_, length)| path.len() > length) {
            best = Some((mount, path.len()));
        }
    }
    let (mount, length) = best?;
    let rest = reference.get(length + 1..).unwrap_or(&[]);
    Some((Label { bytes: mount.label, length: mount.label_length }, rest))
}

/// Like `lookup_dir` for a file: the path must go below the mount point (a file named like the mount point itself is
/// the root's).
pub fn lookup(reference: &[u8]) -> Option<(Label, &[u8])> {
    lookup_dir(reference).filter(|(_, rest)| !rest.is_empty())
}

/// Reads `fstab` from the boot volume into the table (replacing it); returns how many mounts it set and how many
/// lines it could not use. A volume without `fstab` leaves the table empty.
pub fn load() -> (usize, usize) {
    clear();
    let Some(volume) = crate::storage::boot_volume() else { return (0, 0) };
    let mut buffer = [0u8; FSTAB_MAX];
    let Ok(Some(length)) = volume.read(b"fstab", &mut buffer) else { return (0, 0) };
    let Ok(text) = core::str::from_utf8(&buffer[..length]) else { return (0, 1) };
    let (mut set, mut bad) = (0, 0);
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut words = line.split_whitespace();
        let (path, label) = (words.next().unwrap_or(""), words.next().and_then(|word| word.strip_prefix('@')));
        match label.filter(|_| words.next().is_none()).map(|label| add(path.as_bytes(), label.as_bytes())) {
            Some(Ok(())) => set += 1,
            _ => {
                bad += 1;
                crate::println!("fstab: cannot use the line `{line}` (PATH @LABEL).");
            }
        }
    }
    (set, bad)
}
