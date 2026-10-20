//! The shell both ports share (docs/shell.md). A port implements [`Host`] (console and file system) and supplies its own
//! commands as a [`Command`] table; [`execute`] looks a line up in those tables and then in [`files::COMMANDS`].

#![no_std]

use core::fmt;

pub mod files;

/// Writes formatted text to the console.
#[macro_export]
macro_rules! say {
    ($host:expr, $($argument:tt)*) => { $crate::Host::print($host, format_args!($($argument)*)) };
}

/// `say!` plus a newline.
#[macro_export]
macro_rules! sayln {
    ($host:expr) => { $crate::Host::print($host, format_args!("\n")) };
    ($host:expr, $($argument:tt)*) => {{
        $crate::Host::print($host, format_args!($($argument)*));
        $crate::Host::print($host, format_args!("\n"));
    }};
}

/// A last-modified stamp (local time) of a FAT file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

/// One directory entry, as `Host::list` hands it out.
pub struct Entry<'a> {
    pub name: &'a [u8],
    pub length: u64,
    pub directory: bool,
    pub modified: Option<Stamp>,
}

/// Why a file call failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsError {
    NotFound,
    TooBig,
    BadName,
    /// No such disk, label or volume.
    NoVolume,
    /// A file cannot be made empty here (FAT).
    Empty,
    Failed,
}

impl fmt::Display for FsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            FsError::NotFound => "no such file",
            FsError::TooBig => "the file is too big for this command",
            FsError::BadName => "that name cannot be used on that volume",
            FsError::NoVolume => "no such volume (see `disks`)",
            FsError::Empty => "an empty file cannot be made on a FAT volume",
            FsError::Failed => "the volume could not be read or written (no room, or damaged)",
        })
    }
}

/// What the shell needs from a port. A *reference* names a file: `NAME` (the boot volume), `diskN/NAME`, `@LABEL/NAME` or
/// `esp/NAME`; a *volume* is `""` (the boot volume), `diskN`, `@LABEL` or `esp`.
pub trait Host {
    fn print(&mut self, arguments: fmt::Arguments);
    fn read(&mut self, reference: &str, buffer: &mut [u8]) -> Result<usize, FsError>;
    fn length(&mut self, reference: &str) -> Result<u64, FsError>;
    /// Creates or replaces the file.
    fn write(&mut self, reference: &str, data: &[u8]) -> Result<(), FsError>;
    fn remove(&mut self, reference: &str) -> Result<(), FsError>;
    /// Calls `each` (handing it the host back, to print with) for every entry of the volume and returns its kind (`"FOSFS"`, `"FAT32"`, `"exFAT"`).
    fn list(&mut self, volume: &str, each: &mut dyn FnMut(&mut dyn Host, Entry)) -> Result<&'static str, FsError>;
    /// Runs `run` with a scratch buffer of `size` bytes; false if there is no memory for it.
    fn with_buffer(&mut self, size: usize, run: &mut dyn FnMut(&mut dyn Host, &mut [u8])) -> bool;
    /// Prints the help screen of the tape file `reference` (without running it); false if it has none.
    fn tape_help(&mut self, _reference: &str) -> bool {
        false
    }
}

/// One shell command.
pub struct Command {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    /// `NAME ARGS`, shown by `help NAME`.
    pub usage: &'static str,
    pub summary: &'static str,
    pub run: fn(&mut dyn Host, &str),
}

impl Command {
    pub const fn new(name: &'static str, usage: &'static str, summary: &'static str, run: fn(&mut dyn Host, &str)) -> Command {
        Command { name, aliases: &[], usage, summary, run }
    }

    pub const fn alias(mut self, aliases: &'static [&'static str]) -> Command {
        self.aliases = aliases;
        self
    }

    fn is(&self, word: &str) -> bool {
        self.name == word || self.aliases.contains(&word)
    }
}

/// Splits a line into its first word and the (trimmed) rest.
pub fn split_word(line: &str) -> (&str, &str) {
    let line = line.trim();
    line.split_once(char::is_whitespace).map_or((line, ""), |(word, rest)| (word, rest.trim_start()))
}

/// Finds `word` in the tables (a port's table first, so it may override a shared command).
pub fn find<'a>(tables: &[&'a [Command]], word: &str) -> Option<&'a Command> {
    tables.iter().find_map(|table| table.iter().find(|command| command.is(word)))
}

/// Runs a line: `help` here, any other command from the tables. False if the first word is no command (the port then tries
/// scripts, tape names and so on).
pub fn execute(host: &mut dyn Host, tables: &[&[Command]], line: &str) -> bool {
    let (word, rest) = split_word(line);
    if word == "help" {
        help(host, tables, rest);
        return true;
    }
    match find(tables, word) {
        Some(command) => {
            (command.run)(host, rest);
            true
        }
        None => false,
    }
}

/// `help`: every command name; `help NAME`: the usage line, or a tape's help screen.
fn help(host: &mut dyn Host, tables: &[&[Command]], topic: &str) {
    let topic = topic.trim();
    if topic.is_empty() {
        sayln!(host, "Commands (`help NAME` shows one command, or a tape's usage):");
        let mut column = 0;
        for command in tables.iter().flat_map(|table| table.iter()) {
            if column + command.name.len() + 1 > 78 {
                sayln!(host);
                column = 0;
            }
            say!(host, "{} ", command.name);
            column += command.name.len() + 1;
        }
        sayln!(host);
        sayln!(host, "A tape runs as `NAME [ARGS]` or `run NAME [ARGS]`; `run NAME ARGS &N` runs it in the background on NIC N.");
    } else if let Some(command) = find(tables, topic) {
        sayln!(host, "{}", command.usage);
        sayln!(host, "  {}", command.summary);
    } else if !host.tape_help(topic) {
        sayln!(host, "help: {topic} is no command and has no help screen.");
    }
}
