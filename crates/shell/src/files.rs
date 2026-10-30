//! The file commands: `ls cat write rm cp mv cmp mkdir`, on references the host resolves (see `Host`).

use core::fmt;

use crate::glob::{has_wildcards, matches, unescape};
use crate::{Command, Entry, FsError, Host, say, sayln, split_word};

pub static COMMANDS: &[Command] = &[
    Command::new("ls", "ls [diskN|@LABEL|esp][/FOLDER|PATTERN]", "lists the files of a volume or folder, or the matches of a pattern (* ? [a-z]).", ls),
    Command::new("cat", "cat NAME", "shows a file as text (NAME, diskN/NAME, @LABEL/NAME or esp/NAME); other bytes show as dots.", cat),
    Command::new("write", "write NAME TEXT", "makes or replaces a small text file (NAME, diskN/NAME, @LABEL/NAME or esp/NAME).", write),
    Command::new("rm", "rm NAME|PATTERN [confirm]", "removes a file, or every match of a pattern (more than one needs `confirm` at the end).", rm),
    Command::new("cp", "cp SRC|PATTERN DST", "copies a file between volumes; DST may be a volume or folder to keep the name (required for a pattern).", cp).alias(&["copy"]),
    Command::new("mv", "mv SRC|PATTERN DST", "moves a file (copy, read back, remove); DST may be a volume or folder (required for a pattern).", mv),
    Command::new("cmp", "cmp A B", "says whether two files are byte-identical (a read-back check of `cp`).", cmp),
    Command::new("mkdir", "mkdir NAME", "makes a folder on a FOSFS v4 or exFAT volume (NAME, diskN/NAME or @LABEL/NAME); `ls diskN/NAME` lists it.", mkdir),
];

/// The largest file `cat` shows.
const CAT_LIMIT: u64 = 64 * 1024;
/// The largest file `cp` copies or `cmp` compares (each of the two).
const COPY_LIMIT: u64 = 32 << 20;

/// A volume word: `diskN`, `@LABEL` or `esp`.
pub fn is_volume(word: &str) -> bool {
    word == "esp" || word.strip_prefix('@').is_some_and(|label| !label.is_empty()) || word.strip_prefix("disk").is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
}

/// `diskN NAME ...` (the older spelling) as `diskN/NAME ...`, written into `out`; anything else comes back unchanged.
fn join_volume<'a>(rest: &'a str, out: &'a mut [u8; 192]) -> &'a str {
    let (first, after) = split_word(rest);
    if !is_volume(first) || after.is_empty() || first.len() + 1 + after.len() > out.len() {
        return rest;
    }
    out[..first.len()].copy_from_slice(first.as_bytes());
    out[first.len()] = b'/';
    out[first.len() + 1..first.len() + 1 + after.len()].copy_from_slice(after.as_bytes());
    core::str::from_utf8(&out[..first.len() + 1 + after.len()]).unwrap_or(rest)
}

fn ls(host: &mut dyn Host, rest: &str) {
    let volume = rest.trim().trim_end_matches('/');
    if has_wildcards(volume) {
        let mut any = false;
        let outcome = expand(host, volume, &mut |host, reference, directory, length| {
            any = true;
            if directory {
                sayln!(host, "{reference:<24} {:>10}", "<dir>");
            } else {
                sayln!(host, "{reference:<24} {length:>10} bytes");
            }
        });
        return match outcome {
            Ok(()) if !any => sayln!(host, "ls: no match for {volume}."),
            Ok(()) => {}
            Err(error) => sayln!(host, "ls: {volume}: {error}."),
        };
    }
    let mut any = false;
    let outcome = host.list(volume, &mut |host, entry: Entry| {
        any = true;
        let name = core::str::from_utf8(entry.name).unwrap_or("?");
        if entry.directory {
            say!(host, "{name:<16} {:>10}", "<dir>");
        } else {
            say!(host, "{name:<16} {:>10} bytes", entry.length);
        }
        if let Some(time) = entry.modified {
            say!(host, "  {:04}-{:02}-{:02} {:02}:{:02}:{:02}", time.year, time.month, time.day, time.hour, time.minute, time.second);
        }
        sayln!(host);
    });
    match outcome {
        Ok(kind) if !any && (volume.is_empty() || kind == "FOSFS") => sayln!(host, "{kind} is empty."),
        Ok(kind) if !any => sayln!(host, "{volume} ({kind}) is empty."),
        Ok(_) => {}
        Err(error) if volume.is_empty() => sayln!(host, "ls: {error}."),
        Err(error) => sayln!(host, "ls: {volume}: {error}."),
    }
}

/// Prints `bytes` as text: anything but printable ASCII and newlines shows as a dot.
fn print_text(host: &mut dyn Host, bytes: &[u8]) {
    let mut shown = [0u8; 128];
    for chunk in bytes.chunks(shown.len()) {
        for (slot, &byte) in shown.iter_mut().zip(chunk) {
            *slot = if byte == b'\n' || (0x20..0x7f).contains(&byte) { byte } else { b'.' };
        }
        say!(host, "{}", core::str::from_utf8(&shown[..chunk.len()]).unwrap_or("?"));
    }
}

fn cat(host: &mut dyn Host, rest: &str) {
    let mut joined = [0u8; 192];
    let (reference, _) = split_word(join_volume(rest, &mut joined));
    if reference.is_empty() {
        return sayln!(host, "Usage: cat NAME   (NAME, diskN/NAME, @LABEL/NAME or esp/NAME)");
    }
    let length = match host.length(reference) {
        Ok(length) if length > CAT_LIMIT => {
            return sayln!(host, "cat: {reference} is {length} bytes; only files up to {CAT_LIMIT} bytes are shown (copy it to FOSFS or use a PC).");
        }
        Ok(length) => length as usize,
        Err(error) => return sayln!(host, "cat: {reference}: {error}."),
    };
    host.with_buffer(length.max(1), &mut |host, buffer| match host.read(reference, &mut buffer[..length]) {
        Ok(read) => {
            print_text(host, &buffer[..read]);
            if !buffer[..read].ends_with(b"\n") {
                sayln!(host);
            }
        }
        Err(error) => sayln!(host, "cat: {reference}: {error}."),
    });
}

fn write(host: &mut dyn Host, rest: &str) {
    let mut joined = [0u8; 192];
    let (reference, text) = split_word(join_volume(rest, &mut joined));
    if reference.is_empty() {
        return sayln!(host, "Usage: write NAME TEXT   (NAME, diskN/NAME, @LABEL/NAME or esp/NAME)");
    }
    match host.write(reference, text.as_bytes()) {
        Ok(()) => sayln!(host, "Wrote {} bytes to {reference}.", text.len()),
        Err(FsError::Empty) => sayln!(host, "write: nothing to write ({})", FsError::Empty),
        Err(error) => sayln!(host, "write: {reference}: {error}."),
    }
}

fn mkdir(host: &mut dyn Host, rest: &str) {
    let mut joined = [0u8; 192];
    let (reference, _) = split_word(join_volume(rest, &mut joined));
    if reference.is_empty() {
        return sayln!(host, "Usage: mkdir NAME   (NAME, diskN/NAME or @LABEL/NAME; FOSFS v4 and exFAT volumes only)");
    }
    match host.mkdir(reference) {
        Ok(()) => sayln!(host, "Folder {reference} is there."),
        Err(FsError::BadName) => sayln!(host, "mkdir: {reference}: only FOSFS v4 and exFAT volumes have folders; the name is invalid or taken."),
        Err(FsError::Failed) => sayln!(host, "mkdir: {reference}: the parent folder is missing, or the volume is full."),
        Err(error) => sayln!(host, "mkdir: {reference}: {error}."),
    }
}

fn rm(host: &mut dyn Host, rest: &str) {
    let mut joined = [0u8; 192];
    let (reference, after) = split_word(join_volume(rest, &mut joined));
    if reference.is_empty() {
        return sayln!(host, "Usage: rm NAME|PATTERN [confirm]   (NAME, diskN/NAME, @LABEL/NAME or esp/NAME)");
    }
    if has_wildcards(reference) {
        return rm_matches(host, reference, split_word(after).0 == "confirm");
    }
    match host.remove(reference) {
        Ok(()) => sayln!(host, "Removed {reference}."),
        Err(FsError::NotFound) => sayln!(host, "rm: {reference}: no such file (a folder must be empty)."),
        Err(FsError::Failed) => sayln!(host, "rm: {reference}: not removed (a folder must be empty first, or the volume could not be written)."),
        Err(error) => sayln!(host, "rm: {reference}: {error}."),
    }
}

/// `rm PATTERN`: the files that match (the folders only when no file does). More than one needs `confirm`: `rm *` has no undo.
fn rm_matches(host: &mut dyn Host, pattern: &str, confirmed: bool) {
    let (mut files, mut folders) = (0, 0);
    let counted = expand(host, pattern, &mut |_, _, directory, _| if directory { folders += 1 } else { files += 1 });
    if let Err(error) = counted {
        return sayln!(host, "rm: {pattern}: {error}.");
    }
    let (count, want_folders) = if files > 0 { (files, false) } else { (folders, true) };
    if count == 0 {
        return sayln!(host, "rm: no match for {pattern}.");
    }
    if count > 1 && !confirmed {
        return sayln!(host, "rm: {pattern} matches {count} {}; `rm {pattern} confirm` removes them.", if want_folders { "folders" } else { "files" });
    }
    let (mut removed, mut failed) = (0, 0);
    let _ = expand(host, pattern, &mut |host, reference, directory, _| {
        if directory != want_folders {
            return;
        }
        match host.remove(reference) {
            Ok(()) => {
                removed += 1;
                sayln!(host, "Removed {reference}.");
            }
            Err(error) => {
                failed += 1;
                sayln!(host, "rm: {reference}: {error}.");
            }
        }
    });
    sayln!(host, "Removed {removed} of {count}{}.", if failed > 0 { ", some failed" } else { "" });
}

/// `to` made into the file to write for source `from`: `to` itself, or `to` plus `from`'s file name when `to` is a folder.
fn target<'a>(out: &'a mut [u8; 192], to: &'a str, from: &str, folder: bool) -> Option<&'a str> {
    if !folder && !is_volume(to) && !to.ends_with('/') {
        return Some(to);
    }
    let mut writer = Cursor(out, 0);
    let name = from.rsplit('/').next().unwrap_or(from);
    fmt::write(&mut writer, format_args!("{to}{}{name}", if to.ends_with('/') { "" } else { "/" })).ok()?;
    let end = writer.1;
    core::str::from_utf8(&out[..end]).ok()
}

/// A folder the files of a pattern go to: a volume, an existing folder, or one made now.
fn ensure_folder(host: &mut dyn Host, to: &str) -> bool {
    let folder = to.trim_end_matches('/');
    is_volume(folder) || host.list(folder, &mut |_, _| {}).is_ok() || host.mkdir(folder).is_ok()
}

/// Copies one file; `moving` also checks the copy by reading it back and then removes the source. Whether it worked.
fn transfer(host: &mut dyn Host, from: &str, to: &str, moving: bool) -> bool {
    let verb = if moving { "mv" } else { "cp" };
    if from == to {
        sayln!(host, "{verb}: source and destination are the same file.");
        return false;
    }
    let length = match host.length(from) {
        Ok(length) if length > COPY_LIMIT => {
            sayln!(host, "{verb}: {from} is {} MiB; the limit is {} MiB.", length >> 20, COPY_LIMIT >> 20);
            return false;
        }
        Ok(length) => length as usize,
        Err(error) => {
            sayln!(host, "{verb}: {from}: {error}.");
            return false;
        }
    };
    let mut done = false;
    let found = host.with_buffer((length * if moving { 2 } else { 1 }).max(1), &mut |host, buffer| {
        let (data, check) = buffer.split_at_mut(length);
        match host.read(from, data) {
            Ok(read) if read == length => {}
            Ok(_) => return sayln!(host, "{verb}: {from} changed while it was being read."),
            Err(error) => return sayln!(host, "{verb}: {from}: {error}."),
        }
        if let Err(error) = host.write(to, data) {
            return sayln!(host, "{verb}: {to}: {error}.");
        }
        if !moving {
            done = true;
            return sayln!(host, "Copied {from} ({length} bytes) to {to}.");
        }
        match host.read(to, check) {
            Ok(read) if read == length && check[..read] == data[..length] => {}
            _ => return sayln!(host, "mv: {to} did not read back the same; {from} is kept."),
        }
        match host.remove(from) {
            Ok(()) => {
                done = true;
                sayln!(host, "Moved {from} ({length} bytes) to {to}.");
            }
            Err(error) => sayln!(host, "mv: copied to {to}, but {from} was not removed: {error}."),
        }
    });
    if !found {
        sayln!(host, "{verb}: not enough free memory for {length} bytes.");
    }
    done
}

fn cp(host: &mut dyn Host, rest: &str) {
    copy_or_move(host, rest, false);
}

fn mv(host: &mut dyn Host, rest: &str) {
    copy_or_move(host, rest, true);
}

fn copy_or_move(host: &mut dyn Host, rest: &str, moving: bool) {
    let verb = if moving { "mv" } else { "cp" };
    let (from, to) = split_word(rest);
    let (to, _) = split_word(to);
    if from.is_empty() || to.is_empty() {
        return sayln!(host, "Usage: {verb} SRC|PATTERN DST   (names are NAME, diskN/NAME, @LABEL/NAME or esp/NAME; DST may be a volume or folder)");
    }
    if !has_wildcards(from) {
        // A move also takes an existing folder as DST, the way a shell does.
        let folder = moving && host.list(to.trim_end_matches('/'), &mut |_, _| {}).is_ok();
        let mut buffer = [0u8; 192];
        let Some(to) = target(&mut buffer, to, from, folder) else { return sayln!(host, "{verb}: the name is too long.") };
        transfer(host, from, to, moving);
        return;
    }
    // Several sources cannot share one file name: DST is a folder (made when missing).
    let mut sources = 0;
    if let Err(error) = expand(host, from, &mut |_, _, directory, _| sources += usize::from(!directory)) {
        return sayln!(host, "{verb}: {from}: {error}.");
    }
    if sources == 0 {
        return sayln!(host, "{verb}: no match for {from}.");
    }
    if !ensure_folder(host, to) {
        return sayln!(host, "{verb}: {to} is not a folder and cannot be made one (a pattern needs a folder or volume as DST).");
    }
    let (mut worked, mut failed) = (0, 0);
    let _ = expand(host, from, &mut |host, reference, directory, _| {
        if directory {
            return;
        }
        let mut buffer = [0u8; 192];
        match target(&mut buffer, to, reference, true) {
            Some(destination) if transfer(host, reference, destination, moving) => worked += 1,
            Some(_) => failed += 1,
            None => {
                failed += 1;
                sayln!(host, "{verb}: the name {reference} is too long to place in {to}.");
            }
        }
    });
    sayln!(host, "{} {worked} of {sources} file(s){}.", if moving { "Moved" } else { "Copied" }, if failed > 0 { ", some failed" } else { "" });
}

const RECORD: usize = 64;
const NAME_MAX: usize = RECORD - 10;
/// Pattern depth: folders with a pattern of their own (`rec/*/lp1.flac` has one).
const DEPTH: usize = 6;
const LEVEL: usize = 256 * RECORD;
const SCRATCH: usize = DEPTH * LEVEL + 2048 * RECORD;

type Visit<'a> = &'a mut dyn FnMut(&mut dyn Host, &str, bool, u64);

/// Calls `visit(host, reference, is_folder, length)` for everything `pattern` names, in listing order. The listings are
/// collected before anything is visited, so a visitor may write or remove files. A pattern matching nothing visits nothing.
fn expand(host: &mut dyn Host, pattern: &str, visit: Visit) -> Result<(), FsError> {
    let mut outcome = Err(FsError::Failed);
    host.with_buffer(SCRATCH, &mut |host, scratch| {
        outcome = walk(host, scratch, &mut [0u8; 192], 0, pattern, 0, visit);
    });
    outcome
}

/// Appends `/` (unless the path is empty) and `part` to `path[..length]`.
fn push(path: &mut [u8; 192], length: usize, part: &[u8]) -> Option<usize> {
    let start = if length > 0 {
        *path.get_mut(length)? = b'/';
        length + 1
    } else {
        0
    };
    path.get_mut(start..start + part.len())?.copy_from_slice(part);
    Some(start + part.len())
}

fn walk(host: &mut dyn Host, scratch: &mut [u8], path: &mut [u8; 192], length: usize, rest: &str, depth: usize, visit: Visit) -> Result<(), FsError> {
    let (component, tail) = match rest.split_once('/') {
        Some((component, tail)) => (component, Some(tail)),
        None => (rest, None),
    };
    if component.is_empty() {
        return match tail {
            Some(tail) => walk(host, scratch, path, length, tail, depth, visit),
            None => Ok(()),
        };
    }
    // A component without a pattern (a volume, a mount point, a folder) is followed as it stands.
    if let Some(tail) = tail.filter(|_| !has_wildcards(component)) {
        let mut name = [0u8; 64];
        let used = unescape(component, &mut name).ok_or(FsError::BadName)?;
        let length = push(path, length, &name[..used]).ok_or(FsError::BadName)?;
        return walk(host, scratch, path, length, tail, depth, visit);
    }
    if tail.is_some() && depth >= DEPTH {
        return Err(FsError::Failed);
    }
    let (region, deeper) = if tail.is_some() { scratch.split_at_mut(LEVEL) } else { (&mut scratch[..], &mut [][..]) };
    let capacity = region.len() / RECORD;
    let (mut found, mut skipped) = (0, 0);
    let directory = core::str::from_utf8(&path[..length]).map_err(|_| FsError::BadName)?;
    // Entries are kept when they match ignoring case; the exact test follows once the kind of volume is known.
    let kind = host.list(directory, &mut |_, entry| {
        if tail.is_some() && !entry.directory {
            return;
        }
        if entry.name.len() > NAME_MAX {
            skipped += 1;
            return;
        }
        if found >= capacity {
            skipped += 1;
            return;
        }
        if !matches(component, entry.name, true) {
            return;
        }
        let record = &mut region[found * RECORD..(found + 1) * RECORD];
        record[0] = entry.name.len() as u8;
        record[1] = u8::from(entry.directory);
        record[2..10].copy_from_slice(&entry.length.to_le_bytes());
        record[10..10 + entry.name.len()].copy_from_slice(entry.name);
        found += 1;
    })?;
    if skipped > 0 {
        sayln!(host, "{component}: {skipped} entries were skipped (names over {NAME_MAX} bytes, or more than {capacity} in a folder).");
    }
    let fold = kind != "FOSFS";
    for index in 0..found {
        let record = &region[index * RECORD..(index + 1) * RECORD];
        let name = &record[10..10 + usize::from(record[0])];
        if !fold && !matches(component, name, false) {
            continue;
        }
        let (is_folder, size) = (record[1] != 0, u64::from_le_bytes(record[2..10].try_into().unwrap_or([0; 8])));
        let Some(end) = push(path, length, name) else { continue };
        match tail {
            Some(tail) => walk(host, deeper, path, end, tail, depth + 1, visit)?,
            None => visit(host, core::str::from_utf8(&path[..end]).unwrap_or("?"), is_folder, size),
        }
    }
    Ok(())
}

/// A `fmt::Write` into a byte array.
struct Cursor<'a>(&'a mut [u8], usize);

impl fmt::Write for Cursor<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let end = self.1 + text.len();
        self.0.get_mut(self.1..end).ok_or(fmt::Error)?.copy_from_slice(text.as_bytes());
        self.1 = end;
        Ok(())
    }
}

fn cmp(host: &mut dyn Host, rest: &str) {
    let (first, second) = split_word(rest);
    let (second, _) = split_word(second);
    if first.is_empty() || second.is_empty() {
        return sayln!(host, "Usage: cmp A B   (names are NAME, diskN/NAME, @LABEL/NAME or esp/NAME)");
    }
    let lengths = (host.length(first), host.length(second));
    let (a, b) = match lengths {
        (Ok(a), Ok(b)) if a > COPY_LIMIT || b > COPY_LIMIT => return sayln!(host, "cmp: only files up to {} MiB are compared.", COPY_LIMIT >> 20),
        (Ok(a), Ok(b)) => (a as usize, b as usize),
        (Err(error), _) => return sayln!(host, "cmp: {first}: {error}."),
        (_, Err(error)) => return sayln!(host, "cmp: {second}: {error}."),
    };
    if a != b {
        return sayln!(host, "cmp: DIFFER ({a} and {b} bytes).");
    }
    let found = host.with_buffer((a + b).max(1), &mut |host, buffer| {
        let (left, right) = buffer.split_at_mut(a);
        match (host.read(first, left), host.read(second, &mut right[..b])) {
            (Ok(x), Ok(y)) if x == a && y == b && left[..x] == right[..y] => sayln!(host, "cmp: identical, {a} bytes."),
            (Ok(_), Ok(_)) => sayln!(host, "cmp: DIFFER (same length, {a} bytes, different contents)."),
            _ => sayln!(host, "cmp: cannot read both files."),
        }
    });
    if !found {
        sayln!(host, "cmp: not enough free memory.");
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::string::{String, ToString};
    use std::vec::Vec;

    use super::*;

    /// Volumes in memory: `("", "name")` is the boot volume, `("disk2", "name")` another; `esp` is FAT (no empty files).
    #[derive(Default)]
    struct Mock {
        out: String,
        files: Vec<(String, Vec<u8>)>,
        ran: Vec<String>,
    }

    fn canonical(reference: &str) -> String {
        if reference.contains('/') { reference.to_string() } else { std::format!("boot/{reference}") }
    }

    impl Host for Mock {
        fn print(&mut self, arguments: fmt::Arguments) {
            fmt::Write::write_fmt(&mut self.out, arguments).unwrap();
        }
        fn read(&mut self, reference: &str, buffer: &mut [u8]) -> Result<usize, FsError> {
            let (_, data) = self.files.iter().find(|(name, _)| *name == canonical(reference)).ok_or(FsError::NotFound)?;
            buffer.get_mut(..data.len()).ok_or(FsError::TooBig)?.copy_from_slice(data);
            Ok(data.len())
        }
        fn length(&mut self, reference: &str) -> Result<u64, FsError> {
            self.files.iter().find(|(name, _)| *name == canonical(reference)).map(|(_, data)| data.len() as u64).ok_or(FsError::NotFound)
        }
        fn write(&mut self, reference: &str, data: &[u8]) -> Result<(), FsError> {
            if reference.starts_with("esp/") && data.is_empty() {
                return Err(FsError::Empty);
            }
            let name = canonical(reference);
            self.files.retain(|(existing, _)| *existing != name);
            self.files.push((name, data.to_vec()));
            Ok(())
        }
        fn remove(&mut self, reference: &str) -> Result<(), FsError> {
            let before = self.files.len();
            self.files.retain(|(name, _)| *name != canonical(reference));
            if self.files.len() == before { Err(FsError::NotFound) } else { Ok(()) }
        }
        fn mkdir(&mut self, _reference: &str) -> Result<(), FsError> {
            Ok(())
        }
        fn list(&mut self, volume: &str, each: &mut dyn FnMut(&mut dyn Host, Entry)) -> Result<&'static str, FsError> {
            let prefix = std::format!("{}/", if volume.is_empty() { "boot" } else { volume });
            let found: Vec<(String, u64)> = self.files.iter().filter_map(|(name, data)| name.strip_prefix(&prefix).map(|name| (name.to_string(), data.len() as u64))).collect();
            if volume == "disk9" {
                return Err(FsError::NoVolume);
            }
            // a folder that holds nothing does not exist here
            if volume.contains('/') && found.is_empty() {
                return Err(FsError::NotFound);
            }
            // A name with a `/` is in a folder: the folder is listed once, as a directory.
            let mut folders: Vec<String> = Vec::new();
            for (name, length) in found {
                match name.split_once('/') {
                    Some((folder, _)) if folders.iter().any(|seen| seen == folder) => {}
                    Some((folder, _)) => {
                        folders.push(folder.to_string());
                        each(self, Entry { name: folder.as_bytes(), length: 0, directory: true, modified: None });
                    }
                    None => each(self, Entry { name: name.as_bytes(), length, directory: false, modified: None }),
                }
            }
            Ok(if volume == "esp" { "FAT32" } else { "FOSFS" })
        }
        fn with_buffer(&mut self, size: usize, run: &mut dyn FnMut(&mut dyn Host, &mut [u8])) -> bool {
            let mut buffer = std::vec![0u8; size];
            run(self, &mut buffer);
            true
        }
        fn run_script(&mut self, reference: &str, name: &str, arguments: &str) -> bool {
            self.ran.push(std::format!("script {reference} {name} [{arguments}]"));
            true
        }
        fn run_tape(&mut self, line: &str) {
            self.ran.push(std::format!("tape {line}"));
        }
    }

    fn run(host: &mut Mock, line: &str) -> String {
        host.out.clear();
        assert!(crate::execute(host, &[COMMANDS], line), "{line}");
        host.out.clone()
    }

    #[test]
    fn write_cat_rm_on_each_volume_spelling() {
        let mut host = Mock::default();
        assert_eq!(run(&mut host, "write notes.txt hello  there"), "Wrote 12 bytes to notes.txt.\n");
        assert_eq!(run(&mut host, "cat notes.txt"), "hello  there\n");
        // The older `diskN NAME` spelling means `diskN/NAME`.
        assert_eq!(run(&mut host, "write esp config.txt arm_64bit=1"), "Wrote 11 bytes to esp/config.txt.\n");
        assert_eq!(run(&mut host, "cat esp config.txt"), "arm_64bit=1\n");
        assert_eq!(run(&mut host, "rm esp/config.txt"), "Removed esp/config.txt.\n");
        assert!(run(&mut host, "rm esp/config.txt").starts_with("rm: esp/config.txt: no such file"));
        assert!(run(&mut host, "write esp/x.txt").contains("nothing to write"));
    }

    #[test]
    fn external_runs_scripts_before_tapes_and_ignores_data_files() {
        let mut host = Mock::default();
        for name in ["go.sh", "go", "readme.txt", "disk2/up.sh", "esp/tool"] {
            run(&mut host, &std::format!("write {name} x"));
        }
        assert!(crate::external(&mut host, "go.sh a b"));
        assert!(crate::external(&mut host, "go"));
        assert!(crate::external(&mut host, "disk2/up x"));
        assert!(crate::external(&mut host, "esp/tool  1 2"));
        assert!(!crate::external(&mut host, "readme.txt"));
        assert!(!crate::external(&mut host, "nothing"));
        assert_eq!(host.ran, ["script go.sh go [a b]", "script go.sh go []", "script disk2/up.sh up [x]", "tape esp/tool  1 2"]);
    }

    #[test]
    fn cat_shows_unprintable_bytes_as_dots() {
        let mut host = Mock::default();
        host.files.push(("boot/bin".to_string(), std::vec![b'a', 0, 0xff, b'b']));
        assert_eq!(run(&mut host, "cat bin"), "a..b\n");
    }

    #[test]
    fn ls_names_empty_and_missing_volumes() {
        let mut host = Mock::default();
        assert_eq!(run(&mut host, "ls"), "FOSFS is empty.\n");
        assert_eq!(run(&mut host, "ls esp"), "esp (FAT32) is empty.\n");
        run(&mut host, "write a.txt 12345");
        assert_eq!(run(&mut host, "ls"), "a.txt                     5 bytes\n");
        assert_eq!(run(&mut host, "ls disk9"), "ls: disk9: no such disk, or no FOSFS data partition or FAT volume on it (see `disks`).\n");
    }

    #[test]
    fn cp_keeps_the_name_when_the_destination_is_a_volume() {
        let mut host = Mock::default();
        run(&mut host, "write a.txt abc");
        assert_eq!(run(&mut host, "cp a.txt esp"), "Copied a.txt (3 bytes) to esp/a.txt.\n");
        assert_eq!(run(&mut host, "cp esp/a.txt disk2/b.txt"), "Copied esp/a.txt (3 bytes) to disk2/b.txt.\n");
        assert_eq!(run(&mut host, "cmp a.txt disk2/b.txt"), "cmp: identical, 3 bytes.\n");
        assert_eq!(run(&mut host, "cp a.txt a.txt"), "cp: source and destination are the same file.\n");
        assert!(run(&mut host, "cp nothing.txt esp").starts_with("cp: nothing.txt: no such file"));
    }

    #[test]
    fn cmp_tells_lengths_and_contents_apart() {
        let mut host = Mock::default();
        run(&mut host, "write a abc");
        run(&mut host, "write b abd");
        run(&mut host, "write c abcd");
        assert!(run(&mut host, "cmp a b").contains("different contents"));
        assert_eq!(run(&mut host, "cmp a c"), "cmp: DIFFER (3 and 4 bytes).\n");
    }

    fn put(host: &mut Mock, names: &[&str]) {
        for name in names {
            run(host, &std::format!("write {name} {name}"));
        }
    }

    #[test]
    fn ls_with_a_pattern_lists_matches_in_folders() {
        let mut host = Mock::default();
        put(&mut host, &["disk2/rec/a.flac", "disk2/rec/b.flac", "disk2/rec/c.txt", "disk2/lp/a.flac", "disk2/top.flac"]);
        let shown = run(&mut host, "ls disk2/rec/*.flac");
        assert!(shown.contains("disk2/rec/a.flac") && shown.contains("disk2/rec/b.flac") && !shown.contains("c.txt"), "{shown}");
        let shown = run(&mut host, "ls disk2/*/a.flac");
        assert!(shown.contains("disk2/rec/a.flac") && shown.contains("disk2/lp/a.flac") && !shown.contains("top"), "{shown}");
        assert_eq!(run(&mut host, "ls disk2/rec/*.wav"), "ls: no match for disk2/rec/*.wav.\n");
        // a folder matched by a pattern is listed by its own name
        assert!(run(&mut host, "ls disk2/r*").contains("disk2/rec"));
    }

    #[test]
    fn cp_and_mv_with_patterns_need_a_folder() {
        let mut host = Mock::default();
        put(&mut host, &["disk2/rec/a.flac", "disk2/rec/b.flac", "disk2/rec/c.txt"]);
        let out = run(&mut host, "cp disk2/rec/*.flac disk2/backup");
        assert!(out.contains("Copied 2 of 2 file(s)."), "{out}");
        assert_eq!(run(&mut host, "cat disk2/backup/b.flac"), "disk2/rec/b.flac\n");
        assert_eq!(run(&mut host, "cp disk2/rec/*.wav disk2"), "cp: no match for disk2/rec/*.wav.\n");
        let out = run(&mut host, "mv disk2/rec/*.flac disk2/moved/");
        assert!(out.contains("Moved 2 of 2 file(s)."), "{out}");
        assert!(run(&mut host, "cat disk2/rec/a.flac").contains("no such file"));
        assert_eq!(run(&mut host, "cat disk2/moved/a.flac"), "disk2/rec/a.flac\n");
        assert!(run(&mut host, "cat disk2/rec/c.txt").contains("c.txt"));
    }

    #[test]
    fn mv_of_one_file_to_a_name_or_a_volume() {
        let mut host = Mock::default();
        put(&mut host, &["a.txt"]);
        assert!(run(&mut host, "mv a.txt disk2/b.txt").starts_with("Moved boot/a.txt".replace("boot/", "").as_str()));
        assert!(run(&mut host, "cat a.txt").contains("no such file"));
        assert_eq!(run(&mut host, "cat disk2/b.txt"), "a.txt\n");
        assert!(run(&mut host, "mv disk2/b.txt disk2/b.txt").contains("same file"));
    }

    #[test]
    fn rm_with_a_pattern_asks_before_removing_several() {
        let mut host = Mock::default();
        put(&mut host, &["disk2/a.tmp", "disk2/b.tmp", "disk2/keep.txt", "disk2/one.log"]);
        let out = run(&mut host, "rm disk2/*.tmp");
        assert!(out.contains("matches 2 files") && out.contains("rm disk2/*.tmp confirm"), "{out}");
        assert!(run(&mut host, "cat disk2/a.tmp").contains("a.tmp"));
        let out = run(&mut host, "rm disk2/*.tmp confirm");
        assert!(out.contains("Removed 2 of 2."), "{out}");
        assert!(run(&mut host, "cat disk2/b.tmp").contains("no such file"));
        assert!(run(&mut host, "cat disk2/keep.txt").contains("keep.txt"));
        // one match needs no confirmation; none is an error
        assert!(run(&mut host, "rm disk2/*.log").contains("Removed 1 of 1."));
        assert_eq!(run(&mut host, "rm disk2/*.log"), "rm: no match for disk2/*.log.\n");
    }

    #[test]
    fn help_lists_commands_and_one_usage() {
        let mut host = Mock::default();
        assert!(run(&mut host, "help").contains("ls cat write rm cp mv cmp"));
        assert!(run(&mut host, "help cp").starts_with("cp SRC|PATTERN DST\n"));
        assert!(run(&mut host, "help copy").starts_with("cp SRC|PATTERN DST\n"));
        assert!(run(&mut host, "help nothing").contains("no command"));
        assert!(!crate::execute(&mut host, &[COMMANDS], "nothing"));
    }
}
