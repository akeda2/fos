//! The shell's scripting language on mf (docs/shell.md "Scripting"): `fosfor-script` with this kernel's files, output capture
//! and command runner behind its `Env`. A line typed at the prompt that uses the language goes through one interpreter that
//! keeps its variables; `NAME.sh` runs in a fresh one with `$1..` set.

use crate::sync::SpinLock;
use alloc::string::String;
use alloc::vec::Vec;
use fosfor_script::{Env, Interp};
use fosfor_shell::{Entry, Host};

use crate::shellhost::MfHost;

static SESSION: SpinLock<Option<Interp>> = SpinLock::new(None);

/// A prompt line that needs the interpreter: a keyword, an assignment, `;` `&&` `||` `$` `>` or a comment. Everything else
/// keeps the plain command path (`run A | B`, `run NAME ARGS &2` and a `write` of text with quotes are unchanged).
pub fn wants(line: &str) -> bool {
    let line = line.trim_start();
    let first = line.split_whitespace().next().unwrap_or("");
    let assignment = first.split_once('=').is_some_and(|(name, _)| !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
    line.starts_with('#') || assignment || matches!(first, "if" | "for" | "while" | "until") || line.contains([';', '$', '>']) || line.contains("&&") || line.contains("||")
}

pub fn interactive(line: &str) {
    let mut interp = SESSION.lock().take().unwrap_or_default();
    interp.run(&mut MfEnv, line);
    *SESSION.lock() = Some(interp);
}

/// `NAME.sh ARGS`: false when the file cannot be read.
pub fn run_file(reference: &str, arguments: &str) -> bool {
    let mut host = MfHost;
    let Ok(length) = host.length(reference) else { return false };
    let mut text = alloc::vec![0u8; length as usize];
    if !matches!(host.read(reference, &mut text), Ok(read) if read == text.len()) {
        return false;
    }
    let mut interp = Interp::new();
    interp.set_args(split_args(arguments));
    interp.run(&mut MfEnv, &String::from_utf8_lossy(&text));
    true
}

/// Words of the rest of a command line; double quotes keep spaces together.
fn split_args(arguments: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let (mut quoted, mut started) = (false, false);
    for c in arguments.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            c if c.is_whitespace() && !quoted => {
                if started {
                    words.push(core::mem::take(&mut current));
                    started = false;
                }
            }
            c => {
                current.push(c);
                started = true;
            }
        }
    }
    if started {
        words.push(current);
    }
    words
}

struct MfEnv;

impl Env for MfEnv {
    /// The shell has no exit codes: a command failed (1) when it printed a line `NAME: ...` for its own name, and an unknown
    /// one is 127.
    fn run(&mut self, line: &str, capture: Option<&mut Vec<u8>>) -> i32 {
        let command = line.split_whitespace().next().unwrap_or("");
        crate::serial::capture_begin(capture.is_none());
        crate::shell::run_command(line);
        let data = crate::serial::capture_end();
        let text = String::from_utf8_lossy(&data);
        let status = if text.starts_with("Unknown command") {
            127
        } else if text.lines().any(|shown| shown.strip_prefix(command).is_some_and(|rest| rest.starts_with(": "))) {
            1
        } else {
            0
        };
        if let Some(out) = capture {
            out.extend_from_slice(&data);
        }
        status
    }

    fn print(&mut self, text: &str) {
        crate::serial::print(format_args!("{text}"));
    }

    fn glob(&mut self, pattern: &str) -> Vec<String> {
        let (folder, leaf) = pattern.rsplit_once('/').unwrap_or(("", pattern));
        let mut found = Vec::new();
        if fosfor_shell::glob::has_wildcards(folder) {
            return found;
        }
        let _ = MfHost.list(folder, &mut |_, entry: Entry| {
            if fosfor_shell::glob::matches(leaf, entry.name, false) {
                if let Ok(name) = core::str::from_utf8(entry.name) {
                    found.push(if pattern.contains('/') { alloc::format!("{folder}/{name}") } else { String::from(name) });
                }
            }
        });
        found.sort();
        found
    }

    fn stat(&mut self, path: &str) -> Option<bool> {
        if MfHost.length(path).is_ok() {
            return Some(false);
        }
        MfHost.list(path.trim_end_matches('/'), &mut |_, _| {}).ok().map(|_| true)
    }

    fn write(&mut self, path: &str, data: &[u8], append: bool) -> bool {
        let mut all = Vec::new();
        if append {
            if let Ok(length) = MfHost.length(path) {
                all.resize(length as usize, 0);
                if MfHost.read(path, &mut all).is_err() {
                    return false;
                }
            }
        }
        all.extend_from_slice(data);
        MfHost.write(path, &all).is_ok()
    }

    /// Ctrl-C or Escape on the serial line (the keyboard and the network terminal cannot be read from inside a command).
    fn interrupted(&mut self) -> bool {
        matches!(crate::serial::read_byte(), Some(3 | 0x1b))
    }
}
