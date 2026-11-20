//! The Recorder page's state: which inputs are armed, the file format (FLAC at 16, 20 or 24 bits, or WAV in the device's own
//! sample format), the disk the takes go to, the file name stem (`take` gives `take001.flac`, the next free number on that
//! disk), and the state of the take as the tape reports it. FLAC holds at most 8 channels, so a take of more armed inputs is
//! several files, the armed inputs in groups of 8 in input order (`take001-1-8.flac`, `take001-9-16.flac`, ...): four at
//! most, which is what the kernel can keep open at once (`FS_STREAMS`). Recordings go to exFAT, ext4 or FOSFS, not FAT32. A
//! disk can hold two volumes (rack's boot disk: FOSFS and ext4), so a target is a volume, named as the files on it are:
//! `disk3` (FOSFS or FAT, `disk3/take001.flac`) or `ext1` (`ext1/take001.flac`). The take itself (capture, `audio_rec::Recorder`, the file calls) is the tape's: it
//! reads `request`, acts on it and fills in `take`. A take keeps running when another page is shown.
//!
//! Milestone one is a plain take, started and stopped by hand. Pre-roll and the sound/silence trigger (`audio_rec`
//! `preroll`, `silence`, as the `recorder` tape uses them) are the next step.

use crate::Text;
use core::fmt::Write;

/// Disks offered (`disk_info` numbers them from 1; the recorder tape looks at the same eight).
pub const MAX_DISKS: usize = 8;
/// Inputs a take can record: `audio_rec::select` keeps a 32-bit channel mask.
pub const MAX_ARMED: usize = 32;
/// The longest file name stem: `NAMEnnn.wav` must fit FOSFS v3's 16 characters.
pub const MAX_BASE: usize = 9;
/// Channels in one FLAC file (the format's limit), and so the inputs of one group.
pub const GROUP: usize = 8;
/// Groups a take can have: 32 armed inputs in groups of 8.
pub const MAX_GROUPS: usize = MAX_ARMED / GROUP;

/// The file format of a take.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Encoding {
    /// FLAC with this many bits per sample (16, 20 or 24); below the source's depth with TPDF dither.
    Flac(u32),
    /// WAV holding the device's samples untouched (32-bit float from a float device).
    Wav,
}

impl Encoding {
    /// The order the Format soft key (`f`) steps through.
    pub const ALL: [Encoding; 4] = [Encoding::Flac(24), Encoding::Flac(20), Encoding::Flac(16), Encoding::Wav];

    pub fn label(self) -> &'static str {
        match self {
            Encoding::Flac(24) => "FLAC 24",
            Encoding::Flac(20) => "FLAC 20",
            Encoding::Flac(16) => "FLAC 16",
            Encoding::Flac(_) => "FLAC",
            Encoding::Wav => "WAV",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Volume {
    /// A disk with no volume a file can be written to (listed, refused).
    None,
    Fosfs,
    Fat32,
    Exfat,
    /// A Linux ext2/3/4 volume (`vfs_volume`), reached as `extN/`.
    Ext,
}

impl Volume {
    pub fn name(self) -> &'static str {
        match self {
            Volume::None => "no volume",
            Volume::Fosfs => "FOSFS",
            Volume::Fat32 => "FAT32",
            Volume::Exfat => "exFAT",
            Volume::Ext => "ext4",
        }
    }
}

/// A disk the takes can go to, as `disk_info` describes it.
#[derive(Clone, Copy, Debug)]
pub struct Disk {
    pub number: usize,
    /// Whether the kernel can change the volume (an ext volume with features its writer does not keep up is read-only).
    pub writable: bool,
    pub volume: Volume,
    pub boot: bool,
    pub sectors: u64,
    /// Free bytes in all and in the longest contiguous run (one take needs one run); `u64::MAX` when not known (FAT32).
    pub free: u64,
    pub largest: u64,
    pub model: Text<24>,
    pub label: Text<16>,
}

impl Disk {
    pub const EMPTY: Disk = Disk { number: 0, writable: true, volume: Volume::None, boot: false, sectors: 0, free: u64::MAX, largest: u64::MAX, model: Text::new(), label: Text::new() };

    /// The volume's name, as files on it are named: `disk3` or `ext1`.
    pub fn name(&self) -> Text<16> {
        let mut out = Text::new();
        let _ = write!(out, "{}{}", if self.volume == Volume::Ext { "ext" } else { "disk" }, self.number);
        out
    }

    /// What goes before a file name on it: `disk3/` or `ext1/`.
    pub fn prefix(&self) -> Text<16> {
        let mut out = self.name();
        let _ = out.write_str("/");
        out
    }
}

/// What the tape is asked to do with the take.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Request {
    Start,
    Stop,
}

/// The take as the tape reports it.
#[derive(Clone, Copy, Debug, Default)]
pub struct Take {
    pub running: bool,
    /// Frames recorded in this take (all its files), and bytes written to disk.
    pub frames: u64,
    pub bytes: u64,
    /// Files so far (a FAT32 take continues in a new file at 4 GiB), and the one being written (or the last one).
    pub files: u32,
    pub file: Text<40>,
    /// Capture overruns since the take began: audio that never reached the file.
    pub overruns: u64,
    /// Audio waiting in the kernel's capture ring, as a share of it (a disk falling behind shows here first).
    pub backlog_percent: u32,
    /// The slowest single block write of the take in ms, and the disk's average speed in KiB/s.
    pub worst_ms: u64,
    pub kib_per_second: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct Recording {
    /// Capture channels; set by the tape.
    pub channels: usize,
    /// Bit i: input i is armed. Never empty within `channels`.
    pub armed: u32,
    /// The selected input (its arm button is what Enter flips).
    pub input: usize,
    pub encoding: Encoding,
    /// The meters show the Nordic PPM instead of sample peak.
    pub nordic: bool,
    pub disks: [Disk; MAX_DISKS],
    pub disk_count: usize,
    /// Index into `disks`.
    pub disk: usize,
    /// The file name stem.
    pub base: Text<12>,
    /// The capture's format, for the size of a second of recording: rate, bytes per sample slot. Set by the tape.
    pub rate: u32,
    pub slot: usize,
    pub bits: usize,
    pub float: bool,
    pub request: Option<Request>,
    pub take: Take,
}

impl Recording {
    pub const fn new() -> Self {
        Self {
            channels: 2,
            armed: 0b11,
            input: 0,
            encoding: Encoding::Flac(24),
            nordic: false,
            disks: [Disk::EMPTY; MAX_DISKS],
            disk_count: 0,
            disk: 0,
            base: Text::new(),
            rate: 48_000,
            slot: 4,
            bits: 24,
            float: false,
            request: None,
            take: Take { running: false, frames: 0, bytes: 0, files: 0, file: Text::new(), overruns: 0, backlog_percent: 0, worst_ms: 0, kib_per_second: 0 },
        }
    }

    /// Every input a take can record, as a mask.
    pub fn all(&self) -> u32 {
        let n = self.channels.clamp(1, MAX_ARMED);
        if n >= 32 { u32::MAX } else { (1u32 << n) - 1 }
    }

    pub fn is_armed(&self, input: usize) -> bool {
        input < MAX_ARMED && self.armed & self.all() & 1 << input != 0
    }

    pub fn armed_count(&self) -> usize {
        (self.armed & self.all()).count_ones() as usize
    }

    /// Inputs past the 32 a mask holds are metered but cannot be armed.
    pub fn armable(&self, input: usize) -> bool {
        input < self.channels.min(MAX_ARMED)
    }

    /// After `channels` is set: keeps the arming within it, everything when nothing is left.
    pub fn fix(&mut self) {
        if self.armed & self.all() == 0 {
            self.armed = self.all();
        }
        self.armed &= self.all();
        self.input = self.input.min(self.channels.max(1) - 1);
        self.disk = self.disk.min(self.disk_count.saturating_sub(1));
    }

    /// Arms or disarms `input`; refused during a take, and for the last armed input.
    pub fn toggle(&mut self, input: usize) -> Result<bool, &'static str> {
        if self.take.running {
            return Err("stop the take to change what it records");
        }
        if !self.armable(input) {
            return Err("only the first 32 inputs can be recorded");
        }
        let armed = (self.armed & self.all()) ^ 1 << input;
        if armed == 0 {
            return Err("a take needs at least one input");
        }
        self.armed = armed;
        Ok(self.is_armed(input))
    }

    pub fn arm_all(&mut self) -> Result<(), &'static str> {
        if self.take.running {
            return Err("stop the take to change what it records");
        }
        self.armed = self.all();
        Ok(())
    }

    pub fn step(&mut self, forward: bool) {
        let last = self.channels.max(1) - 1;
        self.input = if forward { (self.input + 1).min(last) } else { self.input.saturating_sub(1) };
    }

    pub fn home(&mut self, end: bool) {
        self.input = if end { self.channels.max(1) - 1 } else { 0 };
    }

    /// The disk takes go to now, if there is one.
    pub fn target(&self) -> Option<&Disk> {
        self.disks[..self.disk_count].get(self.disk)
    }

    /// The next disk (wrapping); refused during a take.
    pub fn next_disk(&mut self) -> Result<(), &'static str> {
        if self.take.running {
            return Err("stop the take to change the disk");
        }
        if self.disk_count < 2 {
            return Err("there is no other disk to record to");
        }
        self.disk = (self.disk + 1) % self.disk_count;
        Ok(())
    }

    /// The next file format (FLAC 24, 20, 16, WAV); refused during a take.
    pub fn next_encoding(&mut self) -> Result<(), &'static str> {
        if self.take.running {
            return Err("stop the take to change the format");
        }
        let at = Encoding::ALL.iter().position(|&e| e == self.encoding).unwrap_or(0);
        self.encoding = Encoding::ALL[(at + 1) % Encoding::ALL.len()];
        Ok(())
    }

    /// The files of a take: FLAC splits the armed inputs into groups of up to `GROUP` in input order; WAV is one file.
    /// The masks, and how many.
    pub fn groups(&self) -> ([u32; MAX_GROUPS], usize) {
        let armed = self.armed & self.all();
        let mut groups = [0u32; MAX_GROUPS];
        if self.encoding == Encoding::Wav {
            groups[0] = armed;
            return (groups, (armed != 0) as usize);
        }
        let mut count = 0;
        let mut filled = 0;
        for input in (0..MAX_ARMED).filter(|&i| armed & 1 << i != 0) {
            if filled == GROUP {
                count += 1;
                filled = 0;
            }
            groups[count] |= 1 << input;
            filled += 1;
        }
        (groups, if armed == 0 { 0 } else { count + 1 })
    }

    /// Picks the volume named `wanted` (`ext1`, `disk3`; a bare number is a `disk`) if it is listed (and not FAT32), else
    /// by priority: the first exFAT disk (kept for recordings), the first ext volume this kernel can write, a FOSFS disk that
    /// is not the boot disk, the boot disk. FAT32 is never chosen: its 4 GiB file limit is not worth carrying.
    pub fn choose_disk(&mut self, wanted: Option<&str>) {
        let mut named = Text::<16>::new();
        if let Some(wanted) = wanted {
            let _ = if wanted.bytes().all(|b| b.is_ascii_digit()) { write!(named, "disk{wanted}") } else { named.write_str(wanted) };
        }
        let disks = &self.disks[..self.disk_count];
        let first = |test: &dyn Fn(&Disk) -> bool| disks.iter().position(test);
        self.disk = first(&|d| !named.is_empty() && d.name() == named && d.volume != Volume::Fat32)
            .or_else(|| first(&|d| d.volume == Volume::Exfat))
            .or_else(|| first(&|d| d.volume == Volume::Ext && d.writable))
            .or_else(|| first(&|d| d.volume == Volume::Fosfs && !d.boot))
            .or_else(|| first(&|d| d.boot))
            .unwrap_or(0);
    }

    /// Bytes a second of the take adds to its files at most: WAV exactly, FLAC uncompressed (a frame never needs more; noise
    /// at the input's floor typically halves it).
    pub fn bytes_per_second(&self) -> u64 {
        let bits = match self.encoding {
            Encoding::Flac(bits) => bits as u64,
            Encoding::Wav => self.slot as u64 * 8,
        };
        self.rate as u64 * bits * self.armed_count() as u64 / 8
    }

    /// Seconds of recording that fit the target disk's longest free run, if it is known.
    pub fn seconds_left(&self) -> Option<u64> {
        let disk = self.target()?;
        if disk.largest == u64::MAX || disk.volume == Volume::None {
            return None;
        }
        let room = if self.take.running { disk.largest.saturating_sub(self.take.bytes) } else { disk.largest };
        Some(room / self.bytes_per_second().max(1))
    }

    /// Why a take cannot start now, if it cannot.
    pub fn refusal(&self) -> Option<&'static str> {
        match self.target() {
            None => Some("no disk to record to"),
            Some(disk) if disk.volume == Volume::None => Some("that disk has no volume (format it first)"),
            Some(disk) if disk.volume == Volume::Fat32 => Some("FAT32 is not used for recordings: exFAT"),
            Some(disk) if !disk.writable => Some("fos cannot write that ext volume (its features)"),
            _ if self.base.is_empty() => Some("no file name (rec-name=)"),
            _ => None,
        }
    }

    /// Asks for a take to start, or to stop when one runs.
    pub fn toggle_take(&mut self) -> Result<(), &'static str> {
        if self.take.running {
            self.request = Some(Request::Stop);
            return Ok(());
        }
        if let Some(reason) = self.refusal() {
            return Err(reason);
        }
        self.request = Some(Request::Start);
        Ok(())
    }

    /// What is saved: the armed inputs, the volume's name, the meter scale, the file name stem, the format.
    pub fn persisted(&self) -> (u32, Text<16>, bool, Text<12>, Encoding) {
        (self.armed & self.all(), self.target().map_or(Text::new(), Disk::name), self.nordic, self.base, self.encoding)
    }

    /// Takes `rec-ch=all|1-2,9`, `rec-disk=ext1|disk3` (a bare number is a `disk`), `rec-scale=dbfs|nordic`,
    /// `rec-name=STEM`, `rec-format=flac|wav` and
    /// `rec-bits=16|20|24` from `value(key)`; invalid values are ignored. `rec-disk` only picks a disk once `disks` is filled (call `choose_disk` with it).
    pub fn load<'a>(&mut self, value: impl Fn(&str) -> Option<&'a str>) -> Option<&'a str> {
        if let Some(mask) = value("rec-ch").and_then(|v| parse_channels(v, self.channels.min(MAX_ARMED))) {
            self.armed = mask;
        }
        match value("rec-scale") {
            Some("nordic") => self.nordic = true,
            Some("dbfs") => self.nordic = false,
            _ => {}
        }
        if let Some(name) = value("rec-name").filter(|name| valid_base(name)) {
            self.base = Text::from(name);
        }
        let bits = match self.encoding {
            Encoding::Flac(bits) => bits,
            Encoding::Wav => 24,
        };
        let bits = value("rec-bits").and_then(|v| v.parse().ok()).filter(|b| [16, 20, 24].contains(b)).unwrap_or(bits);
        self.encoding = match value("rec-format") {
            Some("wav") => Encoding::Wav,
            Some("flac") => Encoding::Flac(bits),
            _ if self.encoding == Encoding::Wav => Encoding::Wav,
            _ => Encoding::Flac(bits),
        };
        self.fix();
        value("rec-disk")
    }

    pub fn save(&self, mut put: impl FnMut(&str, core::fmt::Arguments)) {
        let mut channels = Text::<96>::new();
        describe_channels(self.armed & self.all(), self.channels.min(MAX_ARMED), &mut channels);
        put("rec-ch", format_args!("{}", channels.as_str()));
        if let Some(disk) = self.target() {
            put("rec-disk", format_args!("{}", disk.name().as_str()));
        }
        put("rec-scale", format_args!("{}", if self.nordic { "nordic" } else { "dbfs" }));
        put("rec-name", format_args!("{}", self.base.as_str()));
        match self.encoding {
            Encoding::Flac(bits) => {
                put("rec-format", format_args!("flac"));
                put("rec-bits", format_args!("{bits}"));
            }
            Encoding::Wav => put("rec-format", format_args!("wav")),
        }
    }

    /// A take's file for `group` (a mask from `groups`) of `groups`, numbered `number`, on `prefix` (`disk3/`):
    /// `disk3/take007.wav`, `disk3/take007.flac`, or with several groups `disk3/take007-9-16.flac` (the group's first and
    /// last input).
    pub fn file_name(&self, prefix: &str, number: u32, group: u32, groups: usize) -> Text<48> {
        let mut out = Text::new();
        let _ = write!(out, "{prefix}{}{number:03}", self.base.as_str());
        if groups > 1 && group != 0 {
            let _ = write!(out, "-{}-{}", group.trailing_zeros() + 1, 32 - group.leading_zeros());
        }
        let _ = out.write_str(if self.encoding == Encoding::Wav { ".wav" } else { ".flac" });
        out
    }
}

impl Default for Recording {
    fn default() -> Self {
        Self::new()
    }
}

/// A stem every volume takes: 1 to 9 of lowercase letters, digits, `.`, `_`, `-` (the FOSFS v3 rule, which the others
/// accept too).
pub fn valid_base(name: &str) -> bool {
    (1..=MAX_BASE).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
}

/// `all`, or 1-based inputs and ranges (`1-2,9,11-12`) within `channels`; None when it names nothing or a missing input.
pub fn parse_channels(text: &str, channels: usize) -> Option<u32> {
    let all = if channels >= 32 { u32::MAX } else { (1u32 << channels) - 1 };
    if text == "all" {
        return Some(all);
    }
    let mut mask = 0u32;
    for part in text.split(',') {
        let (a, b) = part.split_once('-').unwrap_or((part, part));
        let (a, b): (usize, usize) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
        if a == 0 || b < a || b > channels {
            return None;
        }
        for input in a - 1..b {
            mask |= 1 << input;
        }
    }
    (mask != 0).then_some(mask)
}

/// The inputs of `mask` as `parse_channels` takes them: `all`, or `1-2,9`.
pub fn describe_channels(mask: u32, channels: usize, out: &mut impl Write) {
    let all = if channels >= 32 { u32::MAX } else { (1u32 << channels) - 1 };
    if mask & all == all {
        let _ = out.write_str("all");
        return;
    }
    let mut input = 0;
    let mut first = true;
    while input < 32 {
        if mask & 1 << input == 0 {
            input += 1;
            continue;
        }
        let start = input;
        while input + 1 < 32 && mask & 1 << (input + 1) != 0 {
            input += 1;
        }
        let _ = out.write_str(if first { "" } else { "," });
        first = false;
        let _ = if input == start { write!(out, "{}", start + 1) } else { write!(out, "{}-{}", start + 1, input + 1) };
        input += 1;
    }
}

/// `frames` at `rate` as `h:mm:ss` (`mm:ss` under an hour).
pub fn clock(frames: u64, rate: u32) -> Text<16> {
    let seconds = frames / rate.max(1) as u64;
    let mut out = Text::new();
    let _ = if seconds >= 3600 {
        write!(out, "{}:{:02}:{:02}", seconds / 3600, seconds / 60 % 60, seconds % 60)
    } else {
        write!(out, "{:02}:{:02}", seconds / 60, seconds % 60)
    };
    out
}

/// A size as `640 KiB`, `850 MiB`, `1.5 GiB`, `120 GiB`.
pub fn size(bytes: u64) -> Text<16> {
    let mut out = Text::new();
    let _ = if bytes < 1 << 20 {
        write!(out, "{} KiB", bytes >> 10)
    } else if bytes >= 10 << 30 {
        write!(out, "{} GiB", bytes >> 30)
    } else if bytes >= 1 << 30 {
        let tenths = (bytes * 10) >> 30;
        write!(out, "{}.{} GiB", tenths / 10, tenths % 10)
    } else {
        write!(out, "{} MiB", bytes >> 20)
    };
    out
}

/// A duration in seconds as `3 h 20 min`, `45 min`, `30 s`.
pub fn duration(seconds: u64) -> Text<16> {
    let mut out = Text::new();
    let _ = if seconds >= 3600 {
        write!(out, "{} h {} min", seconds / 3600, seconds / 60 % 60)
    } else if seconds >= 60 {
        write!(out, "{} min", seconds / 60)
    } else {
        write!(out, "{seconds} s")
    };
    out
}
