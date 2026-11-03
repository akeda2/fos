//! Linux (ext2/3/4) volumes on the disks the drivers found, read-only: the glue between the `ext4` crate and `block`,
//! and the shell's `ext4 diskN [ls|cat|info] [PATH]` command, shared by both kernels (docs/ext4.md).
//!
//! A volume is found the way Linux lays it out: a GPT partition of the "Linux filesystem" type, an MBR partition of
//! type 0x83, or the whole disk. The first one that mounts is the disk's ext volume.

use crate::{block, gpt};
use core::fmt::Write;
use ext4::{Device, Error};

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
}

pub type Fs = ext4::Fs<Partition>;

/// Mounts the ext volume of disk index `disk` (0-based). Err carries the last reason when something looked like ext
/// but was refused (a dirty journal, an unsupported feature); NotExt when there is nothing.
pub fn mount(disk: usize) -> Result<Fs, Error> {
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
        match ext4::Fs::mount(partition) {
            Ok(fs) => return Ok(fs),
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
        Error::NeedsRecovery => "its journal needs recovery (mount and unmount it on Linux first)",
        Error::Corrupt => "the volume's structures contradict themselves",
        Error::NotADirectory => "that is not a folder",
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

/// `ext4 diskN [ls|cat|info] [PATH]`; `rest` is what follows the word `ext4`.
pub fn command(rest: &str, out: &mut dyn Write) {
    let mut words = rest.split_whitespace();
    let Some(disk) = words.next().and_then(|word| word.strip_prefix("disk")).and_then(|number| number.parse::<usize>().ok()).filter(|&number| number >= 1) else {
        let _ = writeln!(out, "usage: ext4 diskN [ls|cat|info] [PATH]   (read-only Linux ext2/3/4 volume)");
        return;
    };
    let action = words.next().unwrap_or("ls");
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
            let _ = writeln!(out, "usage: ext4 diskN [ls|cat|info] [PATH]");
        }
    }
}
