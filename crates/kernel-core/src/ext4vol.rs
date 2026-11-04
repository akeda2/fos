//! Linux (ext2/3/4) volumes on the disks the drivers found: the glue between the `ext4` crate and `block`, and the
//! shell's `ext4 diskN ...` command, shared by both kernels (docs/ext4.md). Reading works on any volume the driver
//! understands; changing works on volumes made by `ext4 diskN mkfs` (no checksums, a journal that is replayed first).
//!
//! A volume is found the way Linux lays it out: a GPT partition of the "Linux filesystem" type, an MBR partition of
//! type 0x83, or the whole disk. The first one that mounts is the disk's ext volume.

use crate::{block, gpt};
use core::fmt::Write;
use core::mem::MaybeUninit;
use ext4::{Cache, Device, Error, FormatOptions, Journaled, Writer};

/// GPT type GUID "Linux filesystem data" (0FC63DAF-8483-4772-8E79-3D69D8477DE4), in on-disk mixed-endian order.
pub const LINUX_GUID: [u8; 16] = [0xaf, 0x3d, 0xc6, 0x0f, 0x83, 0x84, 0x72, 0x47, 0x8e, 0x79, 0x3d, 0x69, 0xd8, 0x47, 0x7d, 0xe4];

#[derive(Clone, Copy)]
pub struct Partition {
    pub disk: usize,
    pub base: u64,
    pub sectors: u64,
}

/// Read cache for the volume in use (docs/ext4.md "Performance"): 512 slots of 4 KiB. A folder of thousands of names or the
/// allocation bitmaps are read from the disk once, not on every use. It is emptied whenever a volume is located or made, so
/// nothing written behind its back (`sector`, `foscopy`, another tool) can be served stale.
const CACHE_SLOTS: usize = 512;
static mut CACHE_TAGS: [u64; CACHE_SLOTS] = [0; CACHE_SLOTS];
static mut CACHE_DATA: [u8; CACHE_SLOTS * 4096] = [0; CACHE_SLOTS * 4096];
static mut CACHE: Option<Cache> = None;

fn cache() -> &'static mut Cache {
    // SAFETY: one shell command at a time uses the volume code; the statics are only reached from here.
    unsafe {
        let slot = &mut *core::ptr::addr_of_mut!(CACHE);
        slot.get_or_insert_with(|| Cache::new(&mut *core::ptr::addr_of_mut!(CACHE_TAGS), &mut *core::ptr::addr_of_mut!(CACHE_DATA)))
    }
}

impl Device for Partition {
    fn read(&mut self, lba: u64, buffer: &mut [u8]) -> Result<(), Error> {
        if lba + (buffer.len() / 512) as u64 > self.sectors {
            return Err(Error::Device);
        }
        let (disk, base) = (self.disk, self.base);
        cache().read(lba, buffer, |lba, buffer| block::read(disk, base + lba, buffer).map_err(|_| Error::Device))
    }

    fn write(&mut self, lba: u64, buffer: &[u8]) -> Result<(), Error> {
        if lba + (buffer.len() / 512) as u64 > self.sectors {
            return Err(Error::Device);
        }
        crate::debug::pet(); // a format writes gigabytes of inode tables: longer than the watchdog period
        block::write(self.disk, self.base + lba, buffer).map_err(|_| Error::Device)?;
        cache().wrote(lba, buffer);
        Ok(())
    }

    /// File data goes to the drive without a cache flush each time (a flush costs milliseconds on a SATA SSD); `sync` flushes
    /// once, before the journal commits the metadata that refers to it.
    fn write_data(&mut self, lba: u64, buffer: &[u8]) -> Result<(), Error> {
        if lba + (buffer.len() / 512) as u64 > self.sectors {
            return Err(Error::Device);
        }
        crate::debug::pet();
        block::write_deferred(self.disk, self.base + lba, buffer).map_err(|_| Error::Device)?;
        cache().wrote(lba, buffer);
        Ok(())
    }

    fn sync(&mut self) -> Result<(), Error> {
        block::flush(self.disk).map_err(|_| Error::Device)
    }
}

/// The one writer the shell uses at a time (about 40 KiB: too big for the stack).
static mut WRITER: MaybeUninit<Writer<Journaled<Partition>>> = MaybeUninit::uninit();

/// The journal's staging area (256 KiB: a static, not the stack).
static mut STAGE: ext4::Stage = ext4::Stage::new();

fn stage() -> &'static mut ext4::Stage {
    // SAFETY: one shell command at a time uses it; the previous `Fs`/`Writer` is dead by then.
    unsafe { &mut *core::ptr::addr_of_mut!(STAGE) }
}

/// `ext4 diskN mkfs` waiting for `ext4 diskN mkfs confirm`: the disk number and the label.
static mut PENDING_MKFS: Option<(usize, [u8; 16], usize, bool)> = None;

/// Forgets an unconfirmed `mkfs`: a confirmation must be the very next command.
pub fn cancel() {
    unsafe { PENDING_MKFS = None };
}

pub type Fs = ext4::Fs<Partition>;

/// Where a folder walk by index (`tape_vfs::read_dir`) stopped: the folder's inode, how many entries (without `.` and `..`)
/// were passed and the folder position after the last one, so asking for the next index continues there.
#[derive(Clone, Copy)]
pub struct Cursor {
    pub folder: u32,
    pub index: usize,
    pub position: u64,
}

/// A volume kept mounted between tape calls (an SMB server asks thousands of times a minute), with its read cache intact.
pub struct Shared {
    pub disk: usize,
    pub partition: Partition,
    pub fs: Fs,
    pub cursor: Option<Cursor>,
}

static mut SHARED: Option<Shared> = None;

/// The ext volume of disk index `disk` for tape calls, mounted once and kept until something else locates a volume (any
/// shell command that reads or changes one does, and that also empties the read cache, so nothing stale survives).
pub fn shared(disk: usize) -> Result<&'static mut Shared, Error> {
    commit_pending()?;
    // SAFETY: tape file calls are serialised by the caller (the kernel's file lock); the statics are only reached from here.
    unsafe {
        let slot = &mut *core::ptr::addr_of_mut!(SHARED);
        if slot.as_ref().is_none_or(|shared| shared.disk != disk) {
            *slot = None;
            let partition = locate(disk)?;
            let fs = ext4::Fs::mount(partition)?;
            *slot = Some(Shared { disk, partition, fs, cursor: None });
        }
        Ok(slot.as_mut().unwrap())
    }
}

/// The disk index of the ext volume labelled `label` (the kept one first, else every disk is looked at). Labels of FOSFS and
/// FAT volumes are the callers' to try before this.
pub fn disk_labelled(label: &[u8]) -> Option<usize> {
    // SAFETY: reached only from the serialised file calls, like `shared`.
    if let Some(shared) = unsafe { (*core::ptr::addr_of!(SHARED)).as_ref() } {
        if shared.fs.label() == label {
            return Some(shared.disk);
        }
    }
    (0..block::count()).find(|&disk| mount(disk).is_ok_and(|fs| fs.label() == label))
}

/// The volume `WRITER` holds changes for that are not committed yet (`with_writer_deferred`): the disk index and partition.
static mut OPEN: Option<(usize, Partition)> = None;

/// The writer for `partition`: the open one when it is for the same volume (its uncommitted changes carry on), else a new
/// one (the old one is committed first; the journal is replayed if needed).
fn writer_for(disk: usize, partition: Partition) -> Result<&'static mut Writer<Journaled<Partition>>, Error> {
    // SAFETY: the one writer slot is only used by the (serialised) shell and tape calls; a writer that is replaced is committed first.
    unsafe {
        if let Some((open_disk, open)) = *core::ptr::addr_of!(OPEN) {
            if open_disk == disk && open.base == partition.base {
                let writer = (*core::ptr::addr_of_mut!(WRITER)).assume_init_mut();
                writer.set_time(seconds());
                return Ok(writer);
            }
            commit_pending()?;
        }
        forget_shared();
        let fs = ext4::open(partition, stage())?;
        let writer = Writer::new_in(&mut *core::ptr::addr_of_mut!(WRITER), fs)?;
        writer.set_time(seconds());
        *core::ptr::addr_of_mut!(OPEN) = Some((disk, partition));
        Ok(writer)
    }
}

/// Changes the kept volume of disk index `disk` (on `partition`, as `Shared::partition` says) for a tape call: opens it for
/// writing (the journal is replayed if needed), runs `change`, commits, and keeps the volume mounted again afterwards. The
/// read cache stays (it follows every write), so a stream of small writes costs a mount each, not a rescan of the disk.
/// The caller serialises calls, as for `shared`. Changes an earlier `with_writer_deferred` left open are committed with it.
pub fn with_writer<T>(disk: usize, partition: Partition, change: impl FnOnce(&mut Writer<Journaled<Partition>>) -> Result<T, Error>) -> Result<T, Error> {
    let writer = writer_for(disk, partition)?;
    let done = change(writer);
    let flushed = commit_pending();
    let value = done?;
    flushed?;
    Ok(value)
}

/// Like `with_writer`, but the commit waits: the writer stays open and the next change to the same volume carries on in it,
/// which saves the open, the journal commit and the remount of every call of a stream of file writes. The changes are
/// committed by `commit_pending`, which every other way into the volume (`shared`, `locate`, `with_writer`, a shell command)
/// runs first, and by the tape call `vfs_sync`. A failed change is committed at once.
pub fn with_writer_deferred<T>(disk: usize, partition: Partition, change: impl FnOnce(&mut Writer<Journaled<Partition>>) -> Result<T, Error>) -> Result<T, Error> {
    let writer = writer_for(disk, partition)?;
    let done = change(writer);
    if done.is_err() {
        let _ = commit_pending();
    }
    done
}

/// Commits what `with_writer_deferred` left open (marks the volume clean, commits the journal) and keeps the volume mounted
/// for reading again. Nothing is open: nothing happens.
pub fn commit_pending() -> Result<(), Error> {
    // SAFETY: as in `writer_for`.
    unsafe {
        let Some((disk, partition)) = (*core::ptr::addr_of_mut!(OPEN)).take() else { return Ok(()) };
        let flushed = (*core::ptr::addr_of_mut!(WRITER)).assume_init_mut().flush();
        if let Ok(fs) = ext4::Fs::mount(partition) {
            *core::ptr::addr_of_mut!(SHARED) = Some(Shared { disk, partition, fs, cursor: None });
        }
        flushed
    }
}

/// The partition of the ext volume of disk index `disk`: the one with changes open, else the kept volume's.
pub fn partition_of(disk: usize) -> Result<Partition, Error> {
    // SAFETY: as in `writer_for`.
    if let Some((open_disk, partition)) = unsafe { *core::ptr::addr_of!(OPEN) } {
        if open_disk == disk {
            return Ok(partition);
        }
    }
    Ok(shared(disk)?.partition)
}

/// The disk index of the kept volume, if any.
pub fn shared_disk() -> Option<usize> {
    unsafe { (*core::ptr::addr_of!(OPEN)).map(|(disk, _)| disk).or_else(|| (*core::ptr::addr_of!(SHARED)).as_ref().map(|shared| shared.disk)) }
}

/// Drops the kept volume: after the tape side wrote to it, or the disk went away.
pub fn forget_shared() {
    let _ = commit_pending();
    unsafe { *core::ptr::addr_of_mut!(SHARED) = None };
}

/// Mounts the ext volume of disk index `disk` (0-based). Err carries the last reason when something looked like ext
/// but was refused (a dirty journal, an unsupported feature); NotExt when there is nothing.
pub fn mount(disk: usize) -> Result<Fs, Error> {
    ext4::Fs::mount(locate(disk)?)
}

/// The partition (or whole disk) holding the ext volume of disk index `disk`, even if its journal needs replaying.
pub fn locate(disk: usize) -> Result<Partition, Error> {
    forget_shared();
    let info = block::sectors(disk).ok_or(Error::Device)?;
    cache().clear();
    let mut candidates = [Partition { disk, base: 0, sectors: 0 }; 6];
    let mut count = 0;
    let mut index = 0;
    while count < 4 {
        let mut seen = 0;
        let found = gpt::find(disk, &LINUX_GUID, |partition| {
            seen += 1;
            if seen > index {
                candidates[count] = Partition { disk, base: partition.start, sectors: partition.sectors };
                true
            } else {
                false
            }
        });
        if found.is_none() {
            break;
        }
        count += 1;
        index += 1;
    }
    let mut sector = [0u8; 512];
    if block::read(disk, 0, &mut sector).is_ok() && sector[510] == 0x55 && sector[511] == 0xaa {
        for entry in 0..4 {
            let at = 446 + entry * 16;
            let start = u64::from(u32::from_le_bytes(sector[at + 8..at + 12].try_into().unwrap()));
            let length = u64::from(u32::from_le_bytes(sector[at + 12..at + 16].try_into().unwrap()));
            if sector[at + 4] == 0x83 && start > 0 && count < candidates.len() - 1 {
                candidates[count] = Partition { disk, base: start, sectors: length };
                count += 1;
            }
        }
    }
    candidates[count] = Partition { disk, base: 0, sectors: info };
    count += 1;
    let mut reason = Error::NotExt;
    for &partition in &candidates[..count] {
        cache().clear(); // slots are keyed by sector within the partition, so each candidate starts empty
        match ext4::Fs::mount_with(partition, ext4::Options { ignore_journal: true }) {
            Ok(_) => return Ok(partition),
            Err(Error::NotExt) => {}
            Err(other) => reason = other,
        }
    }
    Err(reason)
}

/// One line for `disks` when disk index `disk` holds an ext volume (`extN` names it); nothing otherwise. A volume whose
/// journal is unreplayed is still listed.
pub fn describe(disk: usize, out: &mut dyn Write) {
    if commit_pending().is_err() {
        return;
    }
    let Ok(partition) = locate(disk) else { return };
    let kind = if partition.base == 0 { "whole disk" } else { "partition" };
    match ext4::Fs::mount(partition) {
        Ok(fs) => {
            let (total, free) = fs.space();
            let _ = writeln!(out, "  ext{}: '{}' {} MiB, {} MiB free ({kind})", disk + 1, core::str::from_utf8(fs.label()).unwrap_or("?"), total >> 20, free >> 20);
        }
        Err(Error::NeedsRecovery) => {
            let _ = writeln!(out, "  ext{}: {} MiB {kind}, journal needs recovery (`ext4 disk{} recover`)", disk + 1, partition.sectors >> 11, disk + 1);
        }
        Err(_) => {}
    }
}

fn why(error: Error) -> &'static str {
    match error {
        Error::Device => "the disk could not be read",
        Error::NotExt => "no ext2/3/4 volume on that disk",
        Error::Unsupported => "this driver does not handle a feature of that volume (or file)",
        Error::NeedsRecovery => "its journal needs recovery (`ext4 diskN recover` replays it)",
        Error::Corrupt => "the volume's structures contradict themselves",
        Error::NotADirectory => "that is not a folder",
        Error::ReadOnly => "this volume cannot be changed (it uses a feature this writer does not keep up with; `ext4 diskN mkfs` makes one it can)",
        Error::Exists => "that name exists already",
        Error::NotFound => "not found",
        Error::NoSpace => "the volume is full",
        Error::NotEmpty => "that folder is not empty",
        Error::IsADirectory => "that is a folder",
        Error::InvalidName => "that is not a valid name (1-255 bytes, no / or NUL)",
        Error::TooFragmented => "that file is too fragmented for this driver",
        Error::TooManyLinks => "that folder has too many sub-folders",
        Error::Invalid => "that cannot be done (a folder into itself, ...)",
    }
}

/// Writes UTF-8 text with controls and invalid bytes as dots; `carry` holds a sequence cut at a chunk's end.
fn print_text(out: &mut dyn Write, carry: &mut ([u8; 4], usize), chunk: &[u8]) {
    let mut bytes = [0u8; 4100];
    let (held, count) = *carry;
    bytes[..count].copy_from_slice(&held[..count]);
    bytes[count..count + chunk.len()].copy_from_slice(chunk);
    let mut rest = &bytes[..count + chunk.len()];
    *carry = ([0; 4], 0);
    while !rest.is_empty() {
        match core::str::from_utf8(rest) {
            Ok(text) => {
                text_out(out, text);
                break;
            }
            Err(error) => {
                let (good, bad) = rest.split_at(error.valid_up_to());
                text_out(out, unsafe { core::str::from_utf8_unchecked(good) });
                match error.error_len() {
                    Some(length) => {
                        let _ = out.write_char('.');
                        rest = &bad[length..];
                    }
                    None => {
                        carry.0[..bad.len()].copy_from_slice(bad);
                        carry.1 = bad.len();
                        break;
                    }
                }
            }
        }
    }
}

fn text_out(out: &mut dyn Write, text: &str) {
    for character in text.chars() {
        let _ = out.write_char(if character == '\n' || character == '\t' || !character.is_control() { character } else { '.' });
    }
}

const USAGE: &str = "usage: ext4 diskN [ls|cat|info|recover|trim|mkfs|remkfs|mkdir|rm|mv|write] [PATH ...]";

/// Splits "a/b/c" into the folder "a/b" (an inode) and the name "c".
fn parent_of(writer: &mut Writer<Journaled<Partition>>, path: &str) -> Result<(u32, usize), Error> {
    let trimmed = path.trim_end_matches('/');
    let cut = trimmed.rfind('/').map_or(0, |at| at + 1);
    let folder = trimmed[..cut].trim_end_matches('/');
    let parent = if folder.is_empty() { writer.root()? } else { writer.resolve(folder.as_bytes())?.ok_or(Error::NotFound)? };
    if !parent.is_dir() {
        return Err(Error::NotADirectory);
    }
    Ok((parent.number, cut))
}

fn leaf(path: &str, cut: usize) -> &[u8] {
    path.trim_end_matches('/')[cut..].as_bytes()
}

fn seconds() -> u32 {
    crate::clock::unix_seconds() as u32
}

/// `ext4 diskN mkfs [LABEL]`: adds a Linux partition over the free space after the last one, if there is no ext volume,
/// and formats it. Asks first.
fn mkfs(disk: usize, label: &str, confirm: bool, replace: bool, out: &mut dyn Write) {
    let index = disk - 1;
    if disk == 1 && !confirm {
        let _ = writeln!(out, "ext4: disk1 is the boot disk; its free space after the FOSFS partition is used when you confirm.");
    }
    if !replace && mount(index).is_ok() {
        let _ = writeln!(out, "ext4: disk{disk} already has an ext volume; nothing was changed (`ext4 disk{disk} remkfs` erases and remakes it).");
        return;
    }
    if !confirm {
        let mut stored = [0u8; 16];
        let length = label.len().min(16);
        stored[..length].copy_from_slice(&label.as_bytes()[..length]);
        unsafe { PENDING_MKFS = Some((disk, stored, length, replace)) };
        if replace {
            let _ = writeln!(out, "This ERASES the first Linux partition of disk{disk} (all its files) and formats it again as ext4 with a journal.");
            let _ = writeln!(out, "To go ahead, type exactly: ext4 disk{disk} remkfs confirm");
            return;
        }
        let _ = writeln!(out, "This adds a partition over ALL the free space after the last partition of disk{disk} and formats it as ext4 (with a journal, no checksums).");
        let _ = writeln!(out, "Existing partitions are not touched. To go ahead, type exactly: ext4 disk{disk} mkfs confirm");
        return;
    }
    let Some((pending, stored, length, was_replace)) = (unsafe { (*core::ptr::addr_of_mut!(PENDING_MKFS)).take() }) else {
        let _ = writeln!(out, "ext4: nothing is waiting for confirmation; run `ext4 disk{disk} mkfs` first.");
        return;
    };
    if pending != disk || was_replace != replace {
        let _ = writeln!(out, "ext4: the pending mkfs was for disk{pending}.");
        return;
    }
    let part = match if replace { gpt::find(index, &LINUX_GUID, |_| true).ok_or(gpt::AddError::NoSpace) } else { gpt::add_partition(index, &LINUX_GUID, "Linux data") } {
        Ok(part) => part,
        Err(error) => {
            let _ = writeln!(out, "ext4: the partition could not be added ({error:?}); nothing was formatted.");
            return;
        }
    };
    let _ = writeln!(out, "ext4: partition ready ({} MiB); formatting, this takes a moment...", part.sectors >> 11);
    match format_partition(index, &part, &stored[..length]) {
        Ok(()) => {
            let _ = writeln!(out, "ext4: formatted. See `ext4 disk{disk} info`.");
        }
        Err(error) => {
            let _ = writeln!(out, "ext4: formatting failed: {}.", why(error));
        }
    }
}

/// Formats `part` of disk index `disk` as ext4 (journal, metadata_csum) with a `lost+found`. Erases the partition.
pub fn format_partition(disk: usize, part: &gpt::Partition, label: &[u8]) -> Result<(), Error> {
    cache().clear();
    // An SSD is told the whole partition is free first (best effort: it may not support it); the format writes everything it needs.
    let _ = discard_ranges(disk, part.start, &[(0, part.sectors)]);
    let mut device = Partition { disk, base: part.start, sectors: part.sectors };
    let mut uuid = [0u8; 16];
    uuid.copy_from_slice(&part.unique_guid);
    let options = FormatOptions { label, bytes_per_inode: 0, uuid, time: seconds(), journal: true, checksums: true };
    ext4::format_raw(&mut device, part.sectors, &options).and_then(|()| {
        let fs = ext4::open(device, stage())?;
        let writer = Writer::new_in(unsafe { &mut *core::ptr::addr_of_mut!(WRITER) }, fs)?;
        writer.set_time(options.time);
        let root = writer.root()?.number;
        writer.mkdir(root, b"lost+found", 0o700)?;
        writer.flush()
    })
}

/// Sends the free-space ranges of a volume to the disk as TRIM (`base` turns volume sectors into disk sectors).
fn discard_ranges(disk: usize, base: u64, ranges: &[(u64, u64)]) -> Result<(), Error> {
    let mut shifted = [(0u64, 0u64); 64];
    for (slot, &(lba, sectors)) in shifted.iter_mut().zip(ranges) {
        *slot = (base + lba, sectors);
    }
    crate::debug::pet();
    match block::discard(disk, &shifted[..ranges.len()]) {
        Ok(()) => Ok(()),
        Err(block::Error::Unsupported) => Err(Error::Unsupported),
        Err(_) => Err(Error::Device),
    }
}

/// `ext4 diskN trim`: tells an SSD which blocks of the volume are free (TRIM), so it can erase them in advance. Done on
/// request, not at every delete: the free blocks are read from the bitmaps after a commit, so nothing in use is touched.
fn trim(disk: usize, out: &mut dyn Write) {
    let result = locate(disk - 1).and_then(|partition| {
        let fs = ext4::open(partition, stage())?;
        let writer = Writer::new_in(unsafe { &mut *core::ptr::addr_of_mut!(WRITER) }, fs)?;
        writer.set_time(seconds());
        let blocks = writer.trim(|ranges| discard_ranges(disk - 1, partition.base, ranges))?;
        Ok((blocks, writer.block_size()))
    });
    match result {
        Ok((blocks, size)) => {
            let _ = writeln!(out, "ext4: disk{disk}: {} MiB of free space trimmed.", blocks * size as u64 >> 20);
        }
        Err(Error::Unsupported) => {
            let _ = writeln!(out, "ext4: disk{disk}: this disk does not support TRIM (SATA and NVMe SSDs only).");
        }
        Err(error) => {
            let _ = writeln!(out, "ext4: disk{disk}: trim failed: {}.", why(error));
        }
    }
}

/// `ext4 diskN mkdir|rm|mv|write ...`
fn change(disk: usize, action: &str, rest: &str, out: &mut dyn Write) {
    let fs = match locate(disk - 1).and_then(|partition| ext4::open(partition, stage())) {
        Ok(fs) => fs,
        Err(error) => {
            let _ = writeln!(out, "ext4: disk{disk}: {}.", why(error));
            return;
        }
    };
    let writer = match Writer::new_in(unsafe { &mut *core::ptr::addr_of_mut!(WRITER) }, fs) {
        Ok(writer) => writer,
        Err(error) => {
            let _ = writeln!(out, "ext4: disk{disk}: {}.", why(error));
            return;
        }
    };
    writer.set_time(seconds());
    let mut words = rest.split_whitespace();
    let (Some(first), second) = (words.next(), words.next()) else {
        let _ = writeln!(out, "{USAGE}");
        return;
    };
    let done: Result<(), Error> = (|| match action {
        "mkdir" => {
            let (parent, cut) = parent_of(writer, first)?;
            writer.mkdir(parent, leaf(first, cut), 0o755).map(|_| ())
        }
        "rm" => {
            let (parent, cut) = parent_of(writer, first)?;
            writer.remove(parent, leaf(first, cut))
        }
        "mv" => {
            let to = second.ok_or(Error::Invalid)?;
            let (from_parent, from_cut) = parent_of(writer, first)?;
            let (to_parent, to_cut) = parent_of(writer, to)?;
            writer.rename(from_parent, leaf(first, from_cut), to_parent, leaf(to, to_cut))
        }
        _ => {
            // write PATH TEXT...: the rest of the line after the path is the file's content.
            let text = rest.trim_start()[first.len()..].trim_start();
            let (parent, cut) = parent_of(writer, first)?;
            let name = leaf(first, cut);
            let existing = {
                let folder = writer.inode(parent)?;
                writer.lookup(&folder, name)?
            };
            let number = match existing {
                Some(inode) => {
                    writer.truncate(inode.number, 0)?;
                    inode.number
                }
                None => writer.create(parent, name, 0o644)?.number,
            };
            writer.write(number, 0, text.as_bytes()).map(|_| ())
        }
    })();
    let flushed = writer.flush();
    match done.and(flushed) {
        Ok(()) => {}
        Err(error) => {
            let _ = writeln!(out, "ext4: {}.", why(error));
        }
    }
}

/// `ext4 diskN [ls|cat|info|recover|mkfs|remkfs|mkdir|rm|mv|write] [ARGS]`; `rest` is what follows the word `ext4`.
pub fn command(rest: &str, out: &mut dyn Write) {
    let confirmed = rest.split_whitespace().nth(2) == Some("confirm") && matches!(rest.split_whitespace().nth(1), Some("mkfs" | "remkfs"));
    if !confirmed {
        cancel();
    }
    let mut words = rest.split_whitespace();
    let Some(disk) = words.next().and_then(|word| word.strip_prefix("disk")).and_then(|number| number.parse::<usize>().ok()).filter(|&number| number >= 1) else {
        let _ = writeln!(out, "{USAGE}");
        return;
    };
    let action = words.next().unwrap_or("ls");
    if action == "mkfs" || action == "remkfs" {
        let argument = words.next().unwrap_or("");
        let replace = action == "remkfs";
        return if argument == "confirm" { mkfs(disk, "", true, replace, out) } else { mkfs(disk, argument, false, replace, out) };
    }
    if action == "recover" {
        let done = locate(disk - 1).and_then(|partition| ext4::open(partition, stage())).and_then(|fs| fs.into_device().close());
        match done {
            Ok(()) => {
                let _ = writeln!(out, "ext4: disk{disk}: journal replayed, the volume is clean.");
            }
            Err(error) => {
                let _ = writeln!(out, "ext4: disk{disk}: {}.", why(error));
            }
        }
        return;
    }
    if action == "trim" {
        return trim(disk, out);
    }
    if matches!(action, "mkdir" | "rm" | "mv" | "write") {
        let mut after = rest.trim_start();
        for _ in 0..2 {
            after = after[after.find(char::is_whitespace).unwrap_or(after.len())..].trim_start();
        }
        return change(disk, action, after, out);
    }
    let path = words.next().unwrap_or("/");
    let mut fs = match mount(disk - 1) {
        Ok(fs) => fs,
        Err(error) => {
            let _ = writeln!(out, "ext4: disk{disk}: {}.", why(error));
            return;
        }
    };
    let failed = |out: &mut dyn Write, error: Error| {
        let _ = writeln!(out, "ext4: {}.", why(error));
    };
    match action {
        "info" => {
            let (total, free) = fs.space();
            let (inodes, free_inodes) = fs.inode_counts();
            let label = core::str::from_utf8(fs.label()).unwrap_or("?");
            let _ = writeln!(out, "label '{label}', {} byte blocks, {} MiB, {} MiB free, {inodes} inodes ({free_inodes} free).", fs.block_size(), total >> 20, free >> 20);
        }
        "ls" | "cat" => {
            let inode = match fs.resolve(path.as_bytes()) {
                Ok(Some(inode)) => inode,
                Ok(None) => {
                    let _ = writeln!(out, "ext4: {path}: not found.");
                    return;
                }
                Err(error) => return failed(out, error),
            };
            if action == "ls" {
                if !inode.is_dir() {
                    let _ = writeln!(out, "{:>10} {path}", inode.size);
                    return;
                }
                let mut names = 0;
                let result = fs.list(&inode, &mut |entry| {
                    if entry.name != b"." && entry.name != b".." {
                        names += 1;
                        let mut carry = ([0u8; 4], 0);
                        let _ = write!(out, "{:>3} ", if entry.kind == 2 { "dir" } else { "" });
                        print_text(out, &mut carry, entry.name);
                        let _ = writeln!(out);
                    }
                    names >= 4096
                });
                if let Err(error) = result {
                    failed(out, error);
                }
            } else {
                if !inode.is_file() {
                    let _ = writeln!(out, "ext4: {path}: not a regular file.");
                    return;
                }
                let mut buffer = [0u8; 4096];
                let mut offset = 0u64;
                let mut carry = ([0u8; 4], 0);
                while offset < inode.size {
                    match fs.read(&inode, offset, &mut buffer) {
                        Ok(0) => break,
                        Ok(got) => {
                            print_text(out, &mut carry, &buffer[..got]);
                            offset += got as u64;
                        }
                        Err(error) => return failed(out, error),
                    }
                }
                let _ = writeln!(out);
            }
        }
        _ => {
            let _ = writeln!(out, "{USAGE}");
        }
    }
}
