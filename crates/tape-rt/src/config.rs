//! Remembered options for a tape: `key=value` words in a text file on FOSFS, the same words the command line takes.
//!
//! `Config::load` builds one string of words, in the order of precedence (`arg_value` returns the first match):
//! 1. the words on the command line, except `@NAME`;
//! 2. the file: `@NAME` from the command line (`diskN/NAME` and `@LABEL/NAME` work, a missing file is an error), or, when
//!    there is no `@NAME`, `TAPE.cfg` (a missing one is fine: first run).
//!
//! A file has `key=value` words separated by spaces or newlines, and `#` starts a comment to the end of the line. `save`
//! rewrites the file that was loaded (or `TAPE.cfg`) with one `key=value` per line, which is what a tape's save key does.

use crate::{FS_NOT_FOUND, TapeApi, alloc_bytes};
use core::fmt::Write;

/// The most the words of a command line and one file may take together.
pub const MAX_BYTES: usize = 2048;
const MAX_NAME: usize = 40;

pub struct Config {
    text: &'static str,
    file: [u8; MAX_NAME],
    file_length: usize,
}

impl Config {
    /// The words, command line first.
    pub fn text(&self) -> &'static str {
        self.text
    }

    /// The file `save` writes (and that was read, if it existed).
    pub fn file(&self) -> &str {
        core::str::from_utf8(&self.file[..self.file_length]).unwrap_or("")
    }

    pub fn value(&self, key: &str) -> Option<&'static str> {
        crate::arg_value(self.text, key)
    }

    /// Reads the file through the tape ABI. `tape` names the default file (`TAPE.cfg`).
    pub fn load(api: &TapeApi, tape: &str, arguments: &str) -> Result<Self, &'static str> {
        let Some(buffer) = alloc_bytes(api, MAX_BYTES) else { return Err("out of memory") };
        let mut read = |name: &str, out: &mut [u8]| -> Result<Option<usize>, &'static str> {
            match (api.fs_read)(name.as_ptr(), name.len(), out.as_mut_ptr(), out.len()) {
                length if length >= 0 => Ok(Some(length as usize)),
                FS_NOT_FOUND => Ok(None),
                crate::FS_TOO_BIG => Err("the configuration file is too long"),
                _ => Err("cannot read the configuration file"),
            }
        };
        let (length, file, file_length) = compose(buffer, tape, arguments, &mut read)?;
        let text = core::str::from_utf8(&buffer[..length]).map_err(|_| "the configuration file is not text")?;
        let text: &'static str = unsafe { core::mem::transmute::<&str, &'static str>(text) };
        Ok(Self { text, file, file_length })
    }

    /// Writes `body` (usually built with `Settings`) to the config file. Returns the file name on success.
    pub fn save(&self, api: &TapeApi, body: &str) -> Result<&str, &'static str> {
        let name = self.file();
        match (api.fs_write)(name.as_ptr(), name.len(), body.as_ptr(), body.len()) {
            0 => Ok(name),
            crate::FS_BAD_NAME => Err("not a valid file name (lowercase, at most 16 characters)"),
            crate::FS_NO_VOLUME => Err("no disk for the file"),
            _ => Err("could not write the file"),
        }
    }
}

/// Builds the words into `out` (see the module header); returns their length and the file name used. `read(name, buffer)`
/// gives the file's length, None when it does not exist.
pub fn compose(
    out: &mut [u8],
    tape: &str,
    arguments: &str,
    read: &mut dyn FnMut(&str, &mut [u8]) -> Result<Option<usize>, &'static str>,
) -> Result<(usize, [u8; MAX_NAME], usize), &'static str> {
    let mut used = 0;
    let mut chosen: Option<&str> = None;
    for word in arguments.split_ascii_whitespace() {
        if let Some(name) = word.strip_prefix('@') {
            chosen = Some(name);
            continue;
        }
        if used + word.len() + 1 > out.len() {
            return Err("the command line is too long");
        }
        out[used..used + word.len()].copy_from_slice(word.as_bytes());
        out[used + word.len()] = b' ';
        used += word.len() + 1;
    }
    let mut default_name = [0u8; MAX_NAME];
    let name = match chosen {
        Some(name) => name,
        None => {
            let mut text = crate::Line::<MAX_NAME>::new();
            let _ = write!(text, "{tape}.cfg");
            default_name[..text.length].copy_from_slice(text.as_str().as_bytes());
            core::str::from_utf8(&default_name[..text.length]).unwrap_or("")
        }
    };
    if name.len() > MAX_NAME {
        return Err("the configuration file name is too long");
    }
    let start = used;
    // the file is read into the tail of `out` and the words are then moved down, comments and newlines taken out
    let room = out.len() - start;
    match read(name, &mut out[start..])? {
        None if chosen.is_some() => return Err("cannot find that configuration file"),
        None => {}
        Some(length) if length <= room => {
            let mut write = start;
            let mut comment = false;
            for index in start..start + length {
                let byte = out[index];
                if byte == b'\n' {
                    comment = false;
                }
                if byte == b'#' {
                    comment = true;
                }
                let kept = if comment { b' ' } else if byte.is_ascii_whitespace() { b' ' } else { byte };
                out[write] = kept;
                write += 1;
            }
            used = write;
        }
        Some(_) => return Err("the configuration file is too long"),
    }
    let mut file = [0u8; MAX_NAME];
    file[..name.len()].copy_from_slice(name.as_bytes());
    Ok((used, file, name.len()))
}

/// Builds the body of a config file: one `key=value` per line, in a fixed buffer.
pub struct Settings<const N: usize> {
    line: crate::Line<N>,
}

impl<const N: usize> Settings<N> {
    pub fn new() -> Self {
        Self { line: crate::Line::new() }
    }

    pub fn put(&mut self, key: &str, value: core::fmt::Arguments) {
        let _ = write!(self.line, "{key}=");
        let _ = self.line.write_fmt(value);
        let _ = writeln!(self.line);
    }

    pub fn as_str(&self) -> &str {
        self.line.as_str()
    }
}

impl<const N: usize> Default for Settings<N> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::string::String;
    use std::vec::Vec;

    fn run(arguments: &str, files: &[(&str, &str)]) -> Result<(String, String), &'static str> {
        let mut out = [0u8; 256];
        let mut read = |name: &str, buffer: &mut [u8]| -> Result<Option<usize>, &'static str> {
            match files.iter().find(|(n, _)| *n == name) {
                Some((_, text)) if text.len() > buffer.len() => Err("the configuration file is too long"),
                Some((_, text)) => {
                    buffer[..text.len()].copy_from_slice(text.as_bytes());
                    Ok(Some(text.len()))
                }
                None => Ok(None),
            }
        };
        let (length, file, file_length) = compose(&mut out, "gonio", arguments, &mut read)?;
        Ok((String::from_utf8(out[..length].to_vec()).unwrap(), String::from_utf8(file[..file_length].to_vec()).unwrap()))
    }

    #[test]
    fn the_default_file_is_read_and_missing_is_fine() {
        let (text, file) = run("law=log", &[("gonio.cfg", "scale=dbfs\n# a comment law=linear\nrange=50\n")]).unwrap();
        assert_eq!(file, "gonio.cfg");
        let words: Vec<&str> = text.split_ascii_whitespace().collect();
        assert_eq!(words, ["law=log", "scale=dbfs", "range=50"]);
        let (text, file) = run("", &[]).unwrap();
        assert_eq!((text.trim(), file.as_str()), ("", "gonio.cfg"));
    }

    #[test]
    fn the_command_line_wins_and_an_at_file_replaces_the_default() {
        let (text, file) = run("range=30 @disk2/night.cfg", &[("gonio.cfg", "scale=dbfs"), ("disk2/night.cfg", "range=60 gain=6")]).unwrap();
        assert_eq!(file, "disk2/night.cfg");
        assert_eq!(crate::arg_value(&text, "range"), Some("30"));
        assert_eq!(crate::arg_value(&text, "gain"), Some("6"));
        assert_eq!(crate::arg_value(&text, "scale"), None);
    }

    #[test]
    fn a_named_file_must_exist_and_too_long_is_refused() {
        assert!(run("@nothere.cfg", &[]).is_err());
        let long = "x=1 ".repeat(100);
        assert!(run("", &[("gonio.cfg", long.as_str())]).is_err());
    }

    #[test]
    fn settings_make_lines() {
        let mut s = Settings::<64>::new();
        s.put("range", format_args!("{}", 40));
        s.put("scale", format_args!("nordic"));
        assert_eq!(s.as_str(), "range=40\nscale=nordic\n");
    }
}
