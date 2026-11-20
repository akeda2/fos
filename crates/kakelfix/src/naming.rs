//! Where the Recorder's takes go, and what they are called. On every volume the same layout:
//!
//! ```text
//! ext1/recordings/                                   the root folder (`rec-root`, the same on every disk)
//! ext1/recordings/Konsert/                           a project (typed, or made up when none is set:
//!                                                    amber-otter_2026-11-20_1435)
//! ext1/recordings/Konsert/take007_2026-11-20_143512/ one folder per take, numbered within the project
//! ext1/recordings/Konsert/take007_2026-11-20_143512/Konsert_take007_2026-11-20_143512_in01-08.flac
//! ```
//!
//! A file's own name carries exactly one timestamp, the take's, so a file copied out of its folder still says when it was
//! recorded and what it is; the project part of the name is the project's name without the timestamp a made-up project
//! has (`stem`). The path may hold more (the project's, the take folder's).
//!
//! Names are ASCII letters, digits, `-` and `_`, never starting with `-` or `_`, so they survive every system they are
//! copied to; projects are compared ignoring case (exFAT does, ext4 does not, and both must find the same project).
//!
//! Lengths: exFAT takes at most 60 characters in a name (`fat::name::MAX_NAME`), so a project's stem is at most
//! `MAX_PROJECT` (20): `{stem}_take007_2026-11-20_143512_in01-08.flac` is the stem and 39 more (40 from take 1000 on, up
//! to `MAX_TAKE`). A whole path stays under
//! `MAX_PATH` (`audio_rec::vfs`, and FOSFS's 192).

use crate::Text;
use core::fmt::Write;

/// The longest project name a person types (and the longest stem of a made-up one).
pub const MAX_PROJECT: usize = 20;
/// The longest root folder (`rec-root`), slashes included.
pub const MAX_ROOT: usize = 40;
/// A project folder's name: a stem and a made-up project's `_2026-11-20_1435`.
pub const MAX_PROJECT_FOLDER: usize = 40;
/// The longest path a take's file can have. Validated parts never reach it: `ext1/` (up to `disk99/`, 7) + root (40) +
/// project folder (40) + take folder (25) + file name (60) + 3 slashes is 175.
pub const MAX_PATH: usize = 192;
pub const DEFAULT_ROOT: &str = "recordings";
/// Takes in one project: `take9999` still fits a file name in 60 characters.
pub const MAX_TAKE: u32 = 9999;
/// Room for a project folder's name: a stem and a timestamp (`_2026-11-20_1435`).
pub type ProjectName = Text<40>;
/// Room for a take's file name (at most 60 characters, see above).
pub type FileName = Text<64>;

/// A moment as the machine's wall clock gives it (local time).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Stamp {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

impl Stamp {
    /// Whether the clock looks set: a fresh RTC, or one never set, says 2000 or so.
    pub fn plausible(&self) -> bool {
        self.year >= 2025 && (1..=12).contains(&self.month) && (1..=31).contains(&self.day) && self.hour < 24 && self.minute < 60 && self.second < 61
    }

    /// `2026-11-20_143512`: a take's.
    pub fn take(&self) -> Text<20> {
        let mut out = Text::new();
        let _ = write!(out, "{:04}-{:02}-{:02}_{:02}{:02}{:02}", self.year, self.month, self.day, self.hour, self.minute, self.second);
        out
    }

    /// `2026-11-20_1435`: a made-up project's.
    pub fn project(&self) -> Text<20> {
        let mut out = Text::new();
        let _ = write!(out, "{:04}-{:02}-{:02}_{:02}{:02}", self.year, self.month, self.day, self.hour, self.minute);
        out
    }

    /// `2026-11-20 14:35:12`, for the page.
    pub fn display(&self) -> Text<20> {
        let mut out = Text::new();
        let _ = write!(out, "{:04}-{:02}-{:02} {:02}:{:02}:{:02}", self.year, self.month, self.day, self.hour, self.minute, self.second);
        out
    }
}

fn name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
}

/// One folder name of ours: 1 to `max` of ASCII letters, digits, `-`, `_`, starting with a letter or digit.
fn valid_component(name: &str, max: usize) -> bool {
    (1..=max).contains(&name.len()) && name.as_bytes()[0].is_ascii_alphanumeric() && name.bytes().all(name_byte)
}

/// A project name a person may type: up to `MAX_PROJECT` of ASCII letters, digits, `-`, `_`, no spaces, starting with a
/// letter or digit.
pub fn valid_project(name: &str) -> bool {
    valid_component(name, MAX_PROJECT)
}

/// A project folder's name as stored (a typed name, or a made-up one with its timestamp).
pub fn valid_project_folder(name: &str) -> bool {
    valid_component(name, MAX_PROJECT_FOLDER) && valid_project(stem(name))
}

/// The root folder: one or more names as `valid_project` takes them, joined by `/` (`recordings`, `fos/recordings`), at
/// most `MAX_ROOT` in all.
pub fn valid_root(root: &str) -> bool {
    root.len() <= MAX_ROOT && !root.is_empty() && root.split('/').all(|part| valid_component(part, MAX_ROOT))
}

/// `2026-11-20_1435` or `2026-11-20_143512`.
fn is_stamp(text: &[u8]) -> bool {
    let shape: &[u8] = match text.len() {
        15 => b"dddd-dd-dd_dddd",
        17 => b"dddd-dd-dd_dddddd",
        _ => return false,
    };
    text.iter().zip(shape).all(|(&b, &s)| if s == b'd' { b.is_ascii_digit() } else { b == s })
}

/// A project's name for its files: without a trailing `_` and timestamp (a made-up project's), so a file's name carries
/// one timestamp only. `amber-otter_2026-11-20_1435` gives `amber-otter`; `Konsert` stays.
pub fn stem(project: &str) -> &str {
    for length in [15, 17] {
        if project.len() > length + 1 {
            let (head, tail) = project.split_at(project.len() - length);
            if head.ends_with('_') && is_stamp(tail.as_bytes()) {
                return &head[..head.len() - 1];
            }
        }
    }
    project
}

/// Whether two project names are the same project (exFAT ignores case; so do we, on every volume).
pub fn same_project(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

const FIRST: [&str; 32] = [
    "amber", "brisk", "calm", "dusky", "eager", "fond", "gentle", "hazy", "ivory", "jolly", "keen", "lucid", "mellow", "noble", "olive", "proud",
    "quiet", "rosy", "silver", "tidy", "umber", "vivid", "warm", "young", "bright", "clear", "deep", "early", "fair", "green", "lofty", "mild",
];
const SECOND: [&str; 32] = [
    "otter", "finch", "heron", "mink", "moose", "owl", "puffin", "raven", "seal", "swan", "tern", "vole", "wren", "badger", "crane", "eider",
    "fox", "hare", "ibis", "jay", "kite", "lark", "marten", "newt", "osprey", "pike", "robin", "stoat", "trout", "wolf", "elk", "gull",
];

/// A project made up when none is set: two words from `random` and the moment, `amber-otter_2026-11-20_1435`.
pub fn made_up_project(random: u32, now: &Stamp) -> ProjectName {
    let mut out = ProjectName::new();
    let first = FIRST[random as usize % FIRST.len()];
    let second = SECOND[(random >> 8) as usize % SECOND.len()];
    let _ = write!(out, "{first}-{second}_{}", now.project().as_str());
    out
}

/// A take's folder: `take007_2026-11-20_143512`.
pub fn take_folder(number: u32, now: &Stamp) -> Text<32> {
    let mut out = Text::new();
    let _ = write!(out, "take{number:03}_{}", now.take().as_str());
    out
}

/// The number of a take folder (`take007_...` gives 7); None for anything else.
pub fn take_number(folder: &str) -> Option<u32> {
    let rest = folder.strip_prefix("take")?;
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if !(3..=4).contains(&digits) || rest.as_bytes().get(digits) != Some(&b'_') {
        return None;
    }
    rest[..digits].parse().ok()
}

/// The inputs of a file, 1-based: `in05`, `in01-08` (its first and last; a group in input order).
fn inputs(mask: u32) -> Text<12> {
    let mut out = Text::new();
    let (first, last) = (mask.trailing_zeros() + 1, 32 - mask.leading_zeros());
    let _ = if mask == 0 || first == last { write!(out, "in{first:02}") } else { write!(out, "in{first:02}-{last:02}") };
    out
}

/// A take's file of the inputs in `mask`: `Konsert_take007_2026-11-20_143512_in01-08.flac`.
pub fn file_name(project: &str, number: u32, now: &Stamp, mask: u32, extension: &str) -> FileName {
    let mut out = FileName::new();
    let _ = write!(out, "{}_take{number:03}_{}_{}.{extension}", stem(project), now.take().as_str(), inputs(mask).as_str());
    out
}

/// `prefix` (`ext1/`) and the folders after it, as one path: `ext1/recordings/Konsert`.
pub fn join(parts: &[&str]) -> Text<MAX_PATH> {
    let mut out = Text::new();
    for (index, part) in parts.iter().enumerate() {
        let separator = if index == 0 || out.as_str().ends_with('/') { "" } else { "/" };
        let _ = write!(out, "{separator}{part}");
    }
    out
}
