//! The file commands: `ls cat write rm cp cmp`, on references the host resolves (see `Host`).

use core::fmt;

use crate::{Command, Entry, FsError, Host, say, sayln, split_word};

pub static COMMANDS: &[Command] = &[
    Command::new("ls", "ls [diskN|@LABEL|esp]", "lists the files of a volume (the boot volume by default).", ls),
    Command::new("cat", "cat NAME", "shows a file as text (NAME, diskN/NAME, @LABEL/NAME or esp/NAME); other bytes show as dots.", cat),
    Command::new("write", "write NAME TEXT", "makes or replaces a small text file (NAME, diskN/NAME, @LABEL/NAME or esp/NAME).", write),
    Command::new("rm", "rm NAME", "removes a file (NAME, diskN/NAME, @LABEL/NAME or esp/NAME).", rm),
    Command::new("cp", "cp SRC DST", "copies a file between volumes; DST may be just a volume (diskN, @LABEL, esp) to keep the name.", cp).alias(&["copy"]),
    Command::new("cmp", "cmp A B", "says whether two files are byte-identical (a read-back check of `cp`).", cmp),
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

fn rm(host: &mut dyn Host, rest: &str) {
    let mut joined = [0u8; 192];
    let (reference, _) = split_word(join_volume(rest, &mut joined));
    if reference.is_empty() {
        return sayln!(host, "Usage: rm NAME   (NAME, diskN/NAME, @LABEL/NAME or esp/NAME)");
    }
    match host.remove(reference) {
        Ok(()) => sayln!(host, "Removed {reference}."),
        Err(FsError::NotFound) => sayln!(host, "rm: {reference}: no such file (directories are not removed here)."),
        Err(error) => sayln!(host, "rm: {reference}: {error}."),
    }
}

/// The destination of `cp`: `to` itself, or `to` with the source's file name when `to` names only a volume.
struct Destination<'a>(&'a str, &'a str);

impl fmt::Display for Destination<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Destination(to, from) = *self;
        if is_volume(to) || to.ends_with('/') {
            let name = from.rsplit('/').next().unwrap_or(from);
            write!(formatter, "{}{}{name}", to, if to.ends_with('/') { "" } else { "/" })
        } else {
            formatter.write_str(to)
        }
    }
}

fn cp(host: &mut dyn Host, rest: &str) {
    let (from, to) = split_word(rest);
    let (to, _) = split_word(to);
    if from.is_empty() || to.is_empty() {
        return sayln!(host, "Usage: cp SRC DST   (names are NAME, diskN/NAME, @LABEL/NAME or esp/NAME; DST may be a volume)");
    }
    let mut target = [0u8; 192];
    let mut writer = Cursor(&mut target, 0);
    if fmt::write(&mut writer, format_args!("{}", Destination(to, from))).is_err() {
        return sayln!(host, "cp: the name is too long.");
    }
    let end = writer.1;
    let to = core::str::from_utf8(&target[..end]).unwrap_or(to);
    if from == to {
        return sayln!(host, "cp: source and destination are the same file.");
    }
    let length = match host.length(from) {
        Ok(length) if length > COPY_LIMIT => return sayln!(host, "cp: {from} is {} MiB; the limit is {} MiB.", length >> 20, COPY_LIMIT >> 20),
        Ok(length) => length as usize,
        Err(error) => return sayln!(host, "cp: {from}: {error}."),
    };
    let found = host.with_buffer(length.max(1), &mut |host, buffer| {
        match host.read(from, &mut buffer[..length]) {
            Ok(read) if read == length => match host.write(to, &buffer[..length]) {
                Ok(()) => sayln!(host, "Copied {from} ({length} bytes) to {to}."),
                Err(error) => sayln!(host, "cp: {to}: {error}."),
            },
            Ok(_) => sayln!(host, "cp: {from} changed while it was being read."),
            Err(error) => sayln!(host, "cp: {from}: {error}."),
        }
    });
    if !found {
        sayln!(host, "cp: not enough free memory for {length} bytes.");
    }
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
        fn list(&mut self, volume: &str, each: &mut dyn FnMut(&mut dyn Host, Entry)) -> Result<&'static str, FsError> {
            let prefix = std::format!("{}/", if volume.is_empty() { "boot" } else { volume });
            let found: Vec<(String, u64)> = self.files.iter().filter_map(|(name, data)| name.strip_prefix(&prefix).map(|name| (name.to_string(), data.len() as u64))).collect();
            if volume == "disk9" {
                return Err(FsError::NoVolume);
            }
            for (name, length) in found {
                each(self, Entry { name: name.as_bytes(), length, directory: false, modified: None });
            }
            Ok(if volume == "esp" { "FAT32" } else { "FOSFS" })
        }
        fn with_buffer(&mut self, size: usize, run: &mut dyn FnMut(&mut dyn Host, &mut [u8])) -> bool {
            let mut buffer = std::vec![0u8; size];
            run(self, &mut buffer);
            true
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

    #[test]
    fn help_lists_commands_and_one_usage() {
        let mut host = Mock::default();
        assert!(run(&mut host, "help").contains("ls cat write rm cp cmp"));
        assert!(run(&mut host, "help cp").starts_with("cp SRC DST\n"));
        assert!(run(&mut host, "help copy").starts_with("cp SRC DST\n"));
        assert!(run(&mut host, "help nothing").contains("no command"));
        assert!(!crate::execute(&mut host, &[COMMANDS], "nothing"));
    }
}
