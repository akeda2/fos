//! Tab completion for the line editor: what the word at the cursor can be, from the command tables, the tapes and scripts on
//! the boot volume and the files and folders of any volume. [`complete`] calls `add` with every candidate (the whole word, a
//! folder with a trailing `/`); the editor keeps those that extend what was typed and does the rest.

use crate::{Command, Entry, Host};
use core::fmt::{self, Write};

const WORD_MAX: usize = 160;

/// A candidate under construction: no allocation, and a name that does not fit is dropped.
struct Word {
    bytes: [u8; WORD_MAX],
    length: usize,
}

impl Word {
    fn new(prefix: &str) -> Word {
        let mut word = Word { bytes: [0; WORD_MAX], length: 0 };
        let _ = word.write_str(prefix);
        word
    }

    fn text(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.length]).unwrap_or("")
    }
}

impl Write for Word {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let end = self.length + text.len();
        if end > WORD_MAX {
            return Err(fmt::Error);
        }
        self.bytes[self.length..end].copy_from_slice(text.as_bytes());
        self.length = end;
        Ok(())
    }
}

/// The candidates for the last word of `line`. `extra` names the commands a port implements outside its tables.
pub fn complete(host: &mut dyn Host, tables: &[&[Command]], extra: &[&str], line: &str, add: &mut dyn FnMut(&str)) {
    let start = line.rfind(' ').map_or(0, |at| at + 1);
    let word = &line[start..];
    let before = line[..start].trim();
    let command_word = before.is_empty() || before == "help";
    let is_path = word.contains('/') || word.starts_with('@');
    if command_word && !is_path {
        for command in tables.iter().flat_map(|table| table.iter()) {
            for name in core::iter::once(&command.name).chain(command.aliases.iter()) {
                add(name);
            }
        }
        for name in extra {
            add(name);
        }
        add("help");
        if before.is_empty() {
            // Tapes live in bin/ on a v4 volume and in the root on a v3 one; a script is NAME.sh and runs as NAME.
            tapes(host, "bin", add);
            tapes(host, "", add);
        }
        return;
    }
    let (folder, prefix) = match word.rfind('/') {
        Some(slash) => (&word[..slash], &word[..=slash]),
        None => ("", ""),
    };
    if prefix.is_empty() {
        // A bare name: the boot volume's files, and the volumes themselves.
        host.volumes(&mut |volume| {
            let mut candidate = Word::new("");
            if write!(candidate, "{volume}/").is_ok() {
                add(candidate.text());
            }
        });
    }
    entries(host, folder, prefix, &mut |_, name, directory| {
        let mut candidate = Word::new(prefix);
        if candidate.write_str(name).is_ok() && (!directory || candidate.write_char('/').is_ok()) {
            add(candidate.text());
        }
    });
}

/// Calls `each` with the name and kind of every entry of `folder` (`""` is the boot volume's root).
fn entries(host: &mut dyn Host, folder: &str, _prefix: &str, each: &mut dyn FnMut(&mut dyn Host, &str, bool)) {
    let _ = host.list(folder, &mut |host, entry: Entry| {
        if let Ok(name) = core::str::from_utf8(entry.name) {
            each(host, name, entry.directory);
        }
    });
}

fn tapes(host: &mut dyn Host, folder: &str, add: &mut dyn FnMut(&str)) {
    entries(host, folder, "", &mut |_, name, directory| {
        if directory {
            return;
        }
        let base = name.strip_suffix(".sh").unwrap_or(name);
        if !base.is_empty() && (name.ends_with(".sh") || !name.contains('.')) {
            add(base);
        }
    });
}
