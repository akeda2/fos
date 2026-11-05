//! The file commands: `ls cat write rm cp mv cmp mkdir`, on references the host resolves (see `Host`).

use core::fmt;

use crate::glob::{has_wildcards, matches, unescape};
use crate::{Command, Entry, FsError, Host, Owner, say, sayln, split_word};

pub static COMMANDS: &[Command] = &[
    Command::new("ls", "ls [diskN|@LABEL|esp][/FOLDER|PATTERN]", "lists the files of a volume or folder, or the matches of a pattern (* ? [a-z]).", ls),
    Command::new("cat", "cat NAME", "shows a file as text (NAME, diskN/NAME, @LABEL/NAME or esp/NAME); other bytes show as dots.", cat),
    Command::new("write", "write NAME TEXT", "makes or replaces a small text file (NAME, diskN/NAME, @LABEL/NAME or esp/NAME).", write),
    Command::new("rm", "rm NAME|PATTERN [confirm]", "removes a file, or every match of a pattern (more than one needs `confirm` at the end).", rm),
    Command::new("cp", "cp [-r] SRC|PATTERN DST", "copies a file between volumes; DST may be a volume or folder to keep the name (required for a pattern); -r copies folders with everything in them.", cp).alias(&["copy"]),
    Command::new("mv", "mv SRC|PATTERN DST", "moves a file (copy, read back, remove); DST may be a volume or folder (required for a pattern).", mv),
    Command::new("cmp", "cmp A B", "says whether two files are byte-identical (a read-back check of `cp`).", cmp),
    Command::new("mkdir", "mkdir NAME", "makes a folder on a FOSFS v4 or exFAT volume (NAME, diskN/NAME or @LABEL/NAME); `ls diskN/NAME` lists it.", mkdir),
    Command::new("stat", "stat NAME", "shows the owner, permission bits and size of a file or folder on an ext volume (extN/NAME or @LABEL/NAME).", stat),
    Command::new("chmod", "chmod [-R] MODE NAME", "sets the permission bits (octal, e.g. 755) of a file or folder on an ext volume; -R includes everything inside a folder.", chmod),
    Command::new("chown", "chown [-R] UID[:GID] NAME", "sets the owner (and group) of a file or folder on an ext volume; :GID alone changes only the group; -R includes everything inside a folder.", chown),
];

fn rwx(mode: u32) -> [u8; 9] {
    let mut text = *b"---------";
    for (index, letter) in b"rwxrwxrwx".iter().enumerate() {
        if mode & (0o400 >> index) != 0 {
            text[index] = *letter;
        }
    }
    text
}

fn stat(host: &mut dyn Host, rest: &str) {
    let (reference, _) = split_word(rest);
    if reference.is_empty() {
        return sayln!(host, "Usage: stat NAME   (extN/NAME or @LABEL/NAME)");
    }
    match host.owner_of(reference) {
        Ok(Owner { uid, gid, mode, directory, size, links }) => {
            let text = rwx(mode);
            sayln!(host, "{reference}: {}, {size} bytes, mode {:04o} ({}), uid {uid} gid {gid}, {links} link(s)", if directory { "folder" } else { "file" }, mode & 0o7777, core::str::from_utf8(&text).unwrap_or("?"));
        }
        Err(FsError::Denied) => sayln!(host, "stat: {reference}: only ext volumes keep owners and modes."),
        Err(error) => sayln!(host, "stat: {reference}: {error}."),
    }
}

/// What `chmod` and `chown` change; `None` keeps the old value.
struct Change {
    uid: Option<u32>,
    gid: Option<u32>,
    mode: Option<u16>,
}

/// (changed, failed)
type Counts = (usize, usize);

fn change_one(host: &mut dyn Host, verb: &str, reference: &str, change: &Change, counts: &mut Counts) {
    let result = host.owner_of(reference).and_then(|now| host.set_owner(reference, change.uid.unwrap_or(now.uid), change.gid.unwrap_or(now.gid), change.mode));
    match result {
        Ok(()) => counts.0 += 1,
        Err(error) => {
            counts.1 += 1;
            match error {
                FsError::Denied => sayln!(host, "{verb}: {reference}: only a writable ext volume keeps owners and modes."),
                error => sayln!(host, "{verb}: {reference}: {error}."),
            }
        }
    }
}

fn change_tree(host: &mut dyn Host, scratch: &mut [u8], verb: &str, reference: &str, change: &Change, depth: usize, counts: &mut Counts) {
    change_one(host, verb, reference, change, counts);
    let directory = host.owner_of(reference).is_ok_and(|owner| owner.directory);
    if !directory {
        return;
    }
    if depth >= DEPTH {
        counts.1 += 1;
        return sayln!(host, "{verb}: {reference} nests deeper than {DEPTH} folders; the rest is skipped.");
    }
    let (region, deeper) = scratch.split_at_mut(LEVEL);
    let capacity = region.len() / RECORD;
    let (mut found, mut skipped) = (0, 0);
    let listed = host.list(reference.trim_end_matches('/'), &mut |_, entry| {
        if entry.name.len() > NAME_MAX || found >= capacity {
            skipped += 1;
            return;
        }
        let record = &mut region[found * RECORD..(found + 1) * RECORD];
        record[0] = entry.name.len() as u8;
        record[10..10 + entry.name.len()].copy_from_slice(entry.name);
        found += 1;
    });
    if let Err(error) = listed {
        counts.1 += 1;
        return sayln!(host, "{verb}: {reference}: {error}.");
    }
    if skipped > 0 {
        counts.1 += skipped;
        sayln!(host, "{verb}: {skipped} entries of {reference} were skipped (names over {NAME_MAX} bytes, or more than {capacity} in a folder).");
    }
    for index in 0..found {
        let record = &region[index * RECORD..(index + 1) * RECORD];
        let name = &record[10..10 + usize::from(record[0])];
        let mut path = [0u8; 192];
        match join(&mut path, reference, name) {
            Some(path) => change_tree(host, deeper, verb, path, change, depth + 1, counts),
            None => counts.1 += 1,
        }
    }
}

/// Shared by `chmod` and `chown`: `[-R] ARG NAME`, with `parse` turning ARG into the change.
fn change(host: &mut dyn Host, verb: &str, usage: &str, rest: &str, parse: fn(&str) -> Option<Change>) {
    let (first, after) = split_word(rest);
    let (recursive, rest) = if matches!(first, "-R" | "-r") { (true, after) } else { (false, rest) };
    let (argument, after) = split_word(rest);
    let (reference, _) = split_word(after);
    if argument.is_empty() || reference.is_empty() {
        return sayln!(host, "Usage: {usage}");
    }
    let Some(change) = parse(argument) else {
        return sayln!(host, "{verb}: {argument}: not understood. Usage: {usage}");
    };
    let mut counts = (0, 0);
    if recursive {
        let reference = reference.trim_end_matches('/');
        if !host.with_buffer(DEPTH * LEVEL, &mut |host, scratch| change_tree(host, scratch, verb, reference, &change, 0, &mut counts)) {
            return sayln!(host, "{verb}: not enough free memory.");
        }
    } else {
        change_one(host, verb, reference, &change, &mut counts);
    }
    if recursive || counts.1 > 0 {
        sayln!(host, "{} changed{}.", counts.0, if counts.1 > 0 { ", some failed" } else { "" });
    }
}

fn chmod(host: &mut dyn Host, rest: &str) {
    change(host, "chmod", "chmod [-R] MODE NAME   (MODE octal, e.g. 755)", rest, |text| {
        let mode = u16::from_str_radix(text, 8).ok().filter(|&mode| mode <= 0o7777)?;
        Some(Change { uid: None, gid: None, mode: Some(mode) })
    });
}

fn chown(host: &mut dyn Host, rest: &str) {
    change(host, "chown", "chown [-R] UID[:GID] NAME   (:GID alone changes the group)", rest, |text| {
        let (user, group) = match text.split_once(':') {
            Some((user, group)) => (user, Some(group)),
            None => (text, None),
        };
        let uid = if user.is_empty() { None } else { Some(user.parse().ok()?) };
        let gid = match group {
            Some("") | None => None,
            Some(group) => Some(group.parse().ok()?),
        };
        (uid.is_some() || gid.is_some()).then_some(Change { uid, gid, mode: None })
    });
}

/// The largest file `cat` shows.
const CAT_LIMIT: u64 = 64 * 1024;
/// The largest file `cp` copies or `cmp` compares (each of the two).
const COPY_LIMIT: u64 = 32 << 20;

/// A volume word: `diskN`, `@LABEL` or `esp`.
pub fn is_volume(word: &str) -> bool {
    word == "esp" || word.strip_prefix('@').is_some_and(|label| !label.is_empty() && !label.contains('/')) || word.strip_prefix("disk").is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
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

/// Prints `bytes` as text: UTF-8 characters and newlines as they are, controls and bytes that are not UTF-8 as dots.
fn print_text(host: &mut dyn Host, bytes: &[u8]) {
    let mut shown = [0u8; 128];
    let mut used = 0;
    for piece in bytes.utf8_chunks() {
        for c in piece.valid().chars() {
            let c = if c == '\n' || !c.is_control() { c } else { '.' };
            if used + c.len_utf8() > shown.len() {
                say!(host, "{}", core::str::from_utf8(&shown[..used]).unwrap_or("?"));
                used = 0;
            }
            used += c.encode_utf8(&mut shown[used..]).len();
        }
        for _ in piece.invalid() {
            if used == shown.len() {
                say!(host, "{}", core::str::from_utf8(&shown[..used]).unwrap_or("?"));
                used = 0;
            }
            shown[used] = b'.';
            used += 1;
        }
    }
    say!(host, "{}", core::str::from_utf8(&shown[..used]).unwrap_or("?"));
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
    if moving {
        match host.rename(from, to) {
            Ok(true) => {
                sayln!(host, "Moved {from} to {to} (renamed in place).");
                return true;
            }
            Ok(false) => {}
            Err(error) => {
                sayln!(host, "mv: {from}: {error}.");
                return false;
            }
        }
    }
    let length = match host.length(from) {
        Ok(length) if length > COPY_LIMIT => {
            match host.copy_stream(from, to) {
                Some(Ok(copied)) if moving => match host.length(to) {
                    Ok(now) if now == copied => match host.remove(from) {
                        Ok(()) => {
                            sayln!(host, "Moved {from} ({copied} bytes) to {to}.");
                            return true;
                        }
                        Err(error) => sayln!(host, "mv: copied to {to}, but {from} was not removed: {error}."),
                    },
                    _ => sayln!(host, "mv: {to} has the wrong length; {from} is kept."),
                },
                Some(Ok(copied)) => {
                    sayln!(host, "Copied {from} ({copied} bytes) to {to}.");
                    return true;
                }
                Some(Err(error)) => sayln!(host, "{verb}: {to}: {error}."),
                None => sayln!(host, "{verb}: {from} is {} MiB; the limit is {} MiB.", length >> 20, COPY_LIMIT >> 20),
            }
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
    let (first, after) = split_word(rest);
    if matches!(first, "-r" | "-R") && !moving {
        return copy_recursive(host, after);
    }
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
        if moving && !is_volume(from.trim_end_matches('/')) && host.list(from.trim_end_matches('/'), &mut |_, _| {}).is_ok() {
            move_folder(host, from, to);
        } else {
            transfer(host, from, to, moving);
        }
        return;
    }
    // Several sources cannot share one file name: DST is a folder (made when missing).
    let mut sources = 0;
    if let Err(error) = expand(host, from, &mut |_, _, directory, _| sources += usize::from(moving || !directory)) {
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
        if directory && !moving {
            return;
        }
        let mut buffer = [0u8; 192];
        match target(&mut buffer, to, reference, true) {
            Some(destination) if directory && move_folder(host, reference, destination) => worked += 1,
            Some(_) if directory => failed += 1,
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

/// `base/name` in `out`.
fn join<'a>(out: &'a mut [u8; 192], base: &str, name: &[u8]) -> Option<&'a str> {
    let base = base.trim_end_matches('/');
    let end = base.len() + 1 + name.len();
    out.get_mut(..base.len())?.copy_from_slice(base.as_bytes());
    *out.get_mut(base.len())? = b'/';
    out.get_mut(base.len() + 1..end)?.copy_from_slice(name);
    core::str::from_utf8(&out[..end]).ok()
}

/// `cp -r SRC|PATTERN DST`: files copy as without `-r`; a folder is copied with everything in it, into DST when that is a
/// volume or an existing folder (a pattern always needs one), else as DST itself.
fn copy_recursive(host: &mut dyn Host, rest: &str) {
    let (from, to) = split_word(rest);
    let (to, _) = split_word(to);
    if from.is_empty() || to.is_empty() {
        return sayln!(host, "Usage: cp -r SRC|PATTERN DST   (a folder is copied with everything in it)");
    }
    let wild = has_wildcards(from);
    let mut into = is_volume(to.trim_end_matches('/')) || host.list(to.trim_end_matches('/'), &mut |_, _| {}).is_ok();
    if wild {
        if !ensure_folder(host, to) {
            return sayln!(host, "cp: {to} is not a folder and cannot be made one (a pattern needs a folder or volume as DST).");
        }
        into = true;
    }
    // (files copied, folders made, failures)
    let mut totals = (0usize, 0usize, 0usize);
    let mut sources = 0;
    let mut one = |host: &mut dyn Host, reference: &str, directory: bool| {
        sources += 1;
        let mut buffer = [0u8; 192];
        let name = reference.trim_end_matches('/').rsplit('/').next().unwrap_or(reference);
        if !directory {
            match target(&mut buffer, to, reference, into) {
                Some(destination) if transfer(host, reference, destination, false) => totals.0 += 1,
                _ => totals.2 += 1,
            }
            return;
        }
        let destination = if into { join(&mut buffer, to, name.as_bytes()) } else { Some(to.trim_end_matches('/')) };
        let Some(destination) = destination else {
            totals.2 += 1;
            return sayln!(host, "cp: the name {reference} is too long to place in {to}.");
        };
        let (source, inside) = (reference.trim_end_matches('/'), destination.trim_end_matches('/'));
        if inside == source || inside.strip_prefix(source).is_some_and(|tail| tail.starts_with('/')) {
            totals.2 += 1;
            return sayln!(host, "cp: cannot copy {source} into itself ({destination}).");
        }
        let found = host.with_buffer(DEPTH * LEVEL, &mut |host, scratch| copy_tree(host, scratch, source, destination, 0, false, &mut totals));
        if !found {
            totals.2 += 1;
            sayln!(host, "cp: not enough free memory to copy {source}.");
        }
    };
    if wild {
        if let Err(error) = expand(host, from, &mut |host, reference, directory, _| one(host, reference, directory)) {
            return sayln!(host, "cp: {from}: {error}.");
        }
    } else {
        let directory = host.list(from.trim_end_matches('/'), &mut |_, _| {}).is_ok();
        one(host, from, directory);
    }
    if sources == 0 {
        return sayln!(host, "cp: no match for {from}.");
    }
    sayln!(host, "Copied {} file(s) and {} folder(s){}.", totals.0, totals.1, if totals.2 > 0 { ", some failed" } else { "" });
}

/// Copies the contents of folder `from` into folder `to` (made when missing), folder by folder.
fn copy_tree(host: &mut dyn Host, scratch: &mut [u8], from: &str, to: &str, depth: usize, moving: bool, totals: &mut (usize, usize, usize)) {
    let failures_before = totals.2;
    if depth >= DEPTH {
        totals.2 += 1;
        return sayln!(host, "cp: {from} nests deeper than {DEPTH} folders; the rest is skipped.");
    }
    if !ensure_folder(host, to) {
        totals.2 += 1;
        return sayln!(host, "cp: cannot make the folder {to} (only FOSFS v4 and exFAT volumes have folders).");
    }
    totals.1 += 1;
    let (region, deeper) = scratch.split_at_mut(LEVEL);
    let capacity = region.len() / RECORD;
    let (mut found, mut skipped) = (0, 0);
    let listed = host.list(from.trim_end_matches('/'), &mut |_, entry| {
        if entry.name.len() > NAME_MAX || found >= capacity {
            skipped += 1;
            return;
        }
        let record = &mut region[found * RECORD..(found + 1) * RECORD];
        record[0] = entry.name.len() as u8;
        record[1] = u8::from(entry.directory);
        record[10..10 + entry.name.len()].copy_from_slice(entry.name);
        found += 1;
    });
    if let Err(error) = listed {
        totals.2 += 1;
        return sayln!(host, "cp: {from}: {error}.");
    }
    if skipped > 0 {
        totals.2 += skipped;
        sayln!(host, "cp: {skipped} entries of {from} were skipped (names over {NAME_MAX} bytes, or more than {capacity} in a folder).");
    }
    for index in 0..found {
        let record = &region[index * RECORD..(index + 1) * RECORD];
        let (name, directory) = (&record[10..10 + usize::from(record[0])], record[1] != 0);
        let (mut source, mut destination) = ([0u8; 192], [0u8; 192]);
        let (Some(source), Some(destination)) = (join(&mut source, from, name), join(&mut destination, to, name)) else {
            totals.2 += 1;
            continue;
        };
        if directory {
            copy_tree(host, deeper, source, destination, depth + 1, moving, totals);
        } else if transfer(host, source, destination, moving) {
            totals.0 += 1;
        } else {
            totals.2 += 1;
        }
    }
    // A move removes the folder once everything in it has been moved (each file was read back before its removal);
    // after any failure the source folder stays with what is left.
    if moving && totals.2 == failures_before {
        if let Err(error) = host.remove(from.trim_end_matches('/')) {
            totals.2 += 1;
            sayln!(host, "mv: {from}: moved, but the folder was not removed: {error}.");
        }
    }
}

/// `mv` of a folder to `destination`: in place when the volume can (v4), else copied file by file with each source file
/// removed after its copy reads back, then the emptied folders. Returns whether it all moved.
fn move_folder(host: &mut dyn Host, source: &str, destination: &str) -> bool {
    let (source, inside) = (source.trim_end_matches('/'), destination.trim_end_matches('/'));
    if inside == source || inside.strip_prefix(source).is_some_and(|tail| tail.starts_with('/')) {
        sayln!(host, "mv: cannot move {source} into itself ({destination}).");
        return false;
    }
    match host.rename(source, inside) {
        Ok(true) => {
            sayln!(host, "Moved {source} to {inside} (renamed in place).");
            return true;
        }
        Ok(false) => {}
        Err(error) => {
            sayln!(host, "mv: {source}: {error}.");
            return false;
        }
    }
    let mut totals = (0usize, 0usize, 0usize);
    let found = host.with_buffer(DEPTH * LEVEL, &mut |host, scratch| copy_tree(host, scratch, source, inside, 0, true, &mut totals));
    if !found {
        sayln!(host, "mv: not enough free memory to move {source}.");
        return false;
    }
    sayln!(host, "Moved {} file(s) of {source} to {inside}{}.", totals.0, if totals.2 > 0 { "; some failed and stay in the source" } else { "" });
    totals.2 == 0
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
        (Ok(a), Ok(b)) if a > COPY_LIMIT || b > COPY_LIMIT => {
            return match host.compare_stream(first, second) {
                Some(Ok(true)) => sayln!(host, "cmp: identical, {a} bytes."),
                Some(Ok(false)) if a != b => sayln!(host, "cmp: DIFFER ({a} and {b} bytes)."),
                Some(Ok(false)) => sayln!(host, "cmp: DIFFER (same length, {a} bytes, different contents)."),
                Some(Err(_)) => sayln!(host, "cmp: cannot read both files."),
                None => sayln!(host, "cmp: only files up to {} MiB are compared.", COPY_LIMIT >> 20),
            };
        }
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
            // a bare file name is not a folder
            if !volume.is_empty() && !volume.contains('/') && !is_volume(volume) {
                return Err(FsError::NotFound);
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
    fn cp_r_copies_folders_with_their_contents() {
        let mut host = Mock::default();
        put(&mut host, &["disk2/rec/a.txt", "disk2/rec/sub/b.txt", "disk2/rec/sub/deep/c.txt"]);
        let out = run(&mut host, "cp -r disk2/rec disk2/copy");
        assert!(out.contains("Copied 3 file(s) and 3 folder(s)."), "{out}");
        assert_eq!(run(&mut host, "cat disk2/copy/sub/deep/c.txt"), "disk2/rec/sub/deep/c.txt\n");
        // into an existing folder it goes inside, under its own name
        let out = run(&mut host, "cp -r disk2/rec disk2/copy");
        assert!(out.contains("Copied 3 file(s)"), "{out}");
        assert!(run(&mut host, "cat disk2/copy/rec/a.txt").contains("a.txt"));
        assert!(run(&mut host, "cp -r disk2/rec disk2/rec/inner").contains("into itself"));
        // a pattern needs a folder; files in it copy as they do without -r
        let out = run(&mut host, "cp -r disk2/rec/*.txt disk2/flat");
        assert!(out.contains("Copied 1 file(s) and 0 folder(s)."), "{out}");
        assert_eq!(run(&mut host, "cp -r disk2/none disk2/x"), "cp: disk2/none: no such file.\nCopied 0 file(s) and 0 folder(s), some failed.\n");
    }

    #[test]
    fn mv_moves_folders_across_volumes() {
        let mut host = Mock::default();
        put(&mut host, &["disk2/rec/a.txt", "disk2/rec/sub/b.txt"]);
        // (the mock has no real folders, so the final folder removal reports NotFound; the files are what moved)
        let out = run(&mut host, "mv disk2/rec disk3/rec");
        assert!(out.contains("Moved 2 file(s) of disk2/rec to disk3/rec"), "{out}");
        assert_eq!(run(&mut host, "cat disk3/rec/sub/b.txt"), "disk2/rec/sub/b.txt\n");
        assert!(run(&mut host, "cat disk2/rec/a.txt").contains("no such file"));
        assert!(run(&mut host, "mv disk3/rec disk3/rec/inner").contains("into itself"));
    }

    #[test]
    fn help_lists_commands_and_one_usage() {
        let mut host = Mock::default();
        assert!(run(&mut host, "help").contains("ls cat write rm cp mv cmp"));
        assert!(run(&mut host, "help cp").starts_with("cp [-r] SRC|PATTERN DST\n"));
        assert!(run(&mut host, "help copy").starts_with("cp [-r] SRC|PATTERN DST\n"));
        assert!(run(&mut host, "help nothing").contains("no command"));
        assert!(!crate::execute(&mut host, &[COMMANDS], "nothing"));
    }
}
