//! Linux (ext2/3/4) volumes on the disks the drivers found: the glue between the `ext4` crate and `block`, and the
//! shell's `ext4 diskN ...` command, shared by both kernels (docs/ext4.md). Reading works on any volume the driver
//! understands; changing works on volumes made by `ext4 diskN mkfs` (no checksums, a journal that is replayed first).
//!
//! A volume is found the way Linux lays it out: a GPT partition of the "Linux filesystem" type, an MBR partition of
//! type 0x83, or the whole disk. The first one that mounts is the disk's ext volume.

use crate::{block, gpt};
use core::fmt::Write;
use core::mem::MaybeUninit;
use ext4::{Device, Error, FormatOptions, Journaled, Writer};

/// GPT type GUID "Linux filesystem data" (0FC63DAF-8483-4772-8E79-3D69D8477DE4), in on-disk mixed-endian order.
const LINUX_GUID: [u8; 16] = [0xaf, 0x3d, 0xc6, 0x0f, 0x83, 0x84, 0x72, 0x47, 0x8e, 0x79, 0x3d, 0x69, 0xd8, 0x47, 0x7d, 0xe4];

#[derive(Clone, Copy)]
pub struct Partition {
    pub disk: usize,
    pub base: u64,
    pub sectors: u64,
}

impl Device for Partition {
    fn read(&mut self, lba: u64, buffer: &mut [u8]) -> Result<(), Error> {
        if lba + (buffer.len() / 512) as u64 > self.sectors {
            return Err(Error::Device);
        }
        block::read(self.disk, self.base + lba, buffer).map_err(|_| Error::Device)
    }

    fn write(&mut self, lba: u64, buffer: &[u8]) -> Result<(), Error> {
        if lba + (buffer.len() / 512) as u64 > self.sectors {
            return Err(Error::Device);
        }
        crate::debug::pet(); // a format writes gigabytes of inode tables: longer than the watchdog period
        block::write(self.disk, self.base + lba, buffer).map_err(|_| Error::Device)
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

/// Mounts the ext volume of disk index `disk` (0-based). Err carries the last reason when something looked like ext
/// but was refused (a dirty journal, an unsupported feature); NotExt when there is nothing.
pub fn mount(disk: usize) -> Result<Fs, Error> {
    ext4::Fs::mount(locate(disk)?)
}

/// The partition (or whole disk) holding the ext volume of disk index `disk`, even if its journal needs replaying.
pub fn locate(disk: usize) -> Result<Partition, Error> {
    let info = block::sectors(disk).ok_or(Error::Device)?;
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
        match ext4::Fs::mount_with(partition, ext4::Options { ignore_journal: true }) {
            Ok(_) => return Ok(partition),
            Err(Error::NotExt) => {}
            Err(other) => reason = other,
        }
    }
    Err(reason)
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

const USAGE: &str = "usage: ext4 diskN [ls|cat|info|recover|mkfs|remkfs|mkdir|rm|mv|write] [PATH ...]";

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
    let device = Partition { disk: index, base: part.start, sectors: part.sectors };
    let mut uuid = [0u8; 16];
    uuid.copy_from_slice(&part.unique_guid);
    let options = FormatOptions { label: &stored[..length], bytes_per_inode: 0, uuid, time: seconds(), journal: true };
    let mut device = device;
    let result = ext4::format_raw(&mut device, part.sectors, &options).and_then(|()| {
        let fs = ext4::open(device, stage())?;
        let writer = Writer::new_in(unsafe { &mut *core::ptr::addr_of_mut!(WRITER) }, fs)?;
        writer.set_time(options.time);
        let root = writer.root()?.number;
        writer.mkdir(root, b"lost+found", 0o700)?;
        writer.flush()
    });
    match result {
        Ok(()) => {
            let _ = writeln!(out, "ext4: formatted. See `ext4 disk{disk} info`.");
        }
        Err(error) => {
            let _ = writeln!(out, "ext4: formatting failed: {}.", why(error));
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
