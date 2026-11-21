//! The Play page: a take picked from the disk's projects, shown as one envelope row per recorded input over the whole
//! take, played to an output pair of the device while capture (and a take) goes on.
//!
//! What is here is what the tape needs and the host can test: which files make up a take and which inputs each holds
//! (`take_file`), a WAV header as the Recorder writes it (`WavLayout`), how the inputs listened to are mixed to the two
//! sides of the output pair (`Playing::mix`), the whole-take overview (`Overview`), the default output pair
//! (`default_output`) and the page's state (`Playing`). Reading, decoding and playing are the tape's (`tapes/kakelfix/src/play.rs`).

use crate::Text;
use crate::naming::{FileName, ProjectName};

/// Projects and takes listed (the newest first; older ones are not shown).
pub const MAX_LIST: usize = 64;
/// Files a take can have: 32 inputs in groups of up to 8 (FLAC's limit; a WAV take is one file).
pub const MAX_FILES: usize = 4;
/// Inputs a take can hold.
pub const MAX_ROWS: usize = 32;
/// Columns of the whole-take overview (drawn scaled to the page's width).
pub const OVERVIEW_COLUMNS: usize = 1024;
/// A take folder's name: `take007_2026-11-20_143512`.
pub type TakeName = Text<32>;

/// One file of a take: its name, the first of its inputs (0-based) and how many it holds, and whether it is FLAC.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TakeFile {
    pub name: FileName,
    pub first: u8,
    pub channels: u8,
    pub flac: bool,
}

impl TakeFile {
    const EMPTY: TakeFile = TakeFile { name: FileName::new(), first: 0, channels: 0, flac: false };
}

/// A take's file from its name (`Konsert_take007_2026-11-20_143512_in01-08.flac`): its inputs from the `inNN` or
/// `inNN-MM` before the extension. None for anything else (a peak file, a stray file).
pub fn take_file(name: &str) -> Option<TakeFile> {
    let (stem, flac) = if let Some(stem) = name.strip_suffix(".flac") {
        (stem, true)
    } else {
        (name.strip_suffix(".wav")?, false)
    };
    let inputs = &stem[stem.rfind("_in")? + 3..];
    let number = |text: &str| -> Option<u8> { (text.len() == 2 && text.bytes().all(|b| b.is_ascii_digit())).then(|| text.parse().ok())? };
    let (first, last) = match inputs.split_once('-') {
        Some((a, b)) => (number(a)?, number(b)?),
        None => (number(inputs)?, number(inputs)?),
    };
    if first == 0 || last < first || usize::from(last) > MAX_ROWS {
        return None;
    }
    Some(TakeFile { name: FileName::from(name), first: first - 1, channels: last - first + 1, flac })
}

/// Where a WAV file's samples are, as the Recorder writes them (`audio_rec::wav`): RIFF or RF64 (`ds64`), plain PCM,
/// IEEE float or WAVE_FORMAT_EXTENSIBLE, samples left-justified in `slot` bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WavLayout {
    pub data_offset: u64,
    pub data_bytes: u64,
    pub channels: u16,
    pub rate: u32,
    pub slot: u16,
    pub float: bool,
}

impl WavLayout {
    pub fn frame_bytes(&self) -> u64 {
        u64::from(self.channels) * u64::from(self.slot)
    }

    pub fn frames(&self) -> u64 {
        self.data_bytes / self.frame_bytes().max(1)
    }

    /// Reads the header from the file's first bytes (`head`, 512 cover any header the Recorder writes) and the file's
    /// length on disk (`file_bytes`: a take still being written, or cut short, has fewer bytes than its header says).
    pub fn parse(head: &[u8], file_bytes: u64) -> Option<WavLayout> {
        let u16_at = |at: usize| Some(u16::from_le_bytes(head.get(at..at + 2)?.try_into().ok()?));
        let u32_at = |at: usize| Some(u32::from_le_bytes(head.get(at..at + 4)?.try_into().ok()?));
        let u64_at = |at: usize| Some(u64::from_le_bytes(head.get(at..at + 8)?.try_into().ok()?));
        let rf64 = match head.get(..4)? {
            b"RIFF" => false,
            b"RF64" => true,
            _ => return None,
        };
        if head.get(8..12)? != b"WAVE" {
            return None;
        }
        let (mut at, mut big_data, mut format) = (12usize, None, None);
        while at + 8 <= head.len() {
            let (id, size) = (head.get(at..at + 4)?, u32_at(at + 4)?);
            let body = at + 8;
            match id {
                b"ds64" if rf64 => big_data = Some(u64_at(body + 8)?),
                b"fmt " => {
                    let tag = u16_at(body)?;
                    let tag = if tag == 0xfffe { u16_at(body + 24)? } else { tag };
                    let bits = u16_at(body + 14)?;
                    format = Some((tag, u16_at(body + 2)?, u32_at(body + 4)?, bits.div_ceil(8).max(u16_at(body + 12)? / u16_at(body + 2)?.max(1))));
                }
                b"data" => {
                    let (tag, channels, rate, slot) = format?;
                    if channels == 0 || slot == 0 || !(tag == 1 || tag == 3) || (tag == 3 && slot != 4) || slot > 4 {
                        return None;
                    }
                    let stated = if rf64 && size == u32::MAX { big_data? } else { u64::from(size) };
                    let data_offset = body as u64;
                    let present = file_bytes.saturating_sub(data_offset);
                    let layout = WavLayout { data_offset, data_bytes: stated.min(present), channels, rate, slot, float: tag == 3 };
                    return Some(WavLayout { data_bytes: layout.data_bytes / layout.frame_bytes() * layout.frame_bytes(), ..layout });
                }
                _ => {}
            }
            at = body + size as usize + (size as usize & 1);
        }
        None
    }

    /// Sample `channel` of the frame at `frame` (its bytes) as -1..1.
    pub fn sample(&self, frame: &[u8], channel: usize) -> f32 {
        let slot = usize::from(self.slot);
        let bytes = &frame[channel * slot..channel * slot + slot];
        if self.float {
            return f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        }
        // Left-justified: the sample's top byte is the slot's last, so it reads as a full-width signed number.
        let mut word = [0u8; 4];
        word[4 - slot..].copy_from_slice(bytes);
        i32::from_le_bytes(word) as f32 / 2_147_483_648.0
    }
}

/// A take's inputs at a glance: the lowest and highest sample of each input per column, over the whole take. Filled as
/// the take is read through (`push`, any order); a column not reached yet is empty.
pub struct Overview {
    rows: usize,
    frames: u64,
    per_column: u64,
    columns: usize,
    data: [[[i16; 2]; OVERVIEW_COLUMNS]; MAX_ROWS],
    reached: [u64; MAX_ROWS],
}

impl Overview {
    pub const fn new() -> Self {
        Overview { rows: 0, frames: 0, per_column: 1, columns: 0, data: [[[0; 2]; OVERVIEW_COLUMNS]; MAX_ROWS], reached: [0; MAX_ROWS] }
    }

    /// Empties it for a take of `rows` inputs and `frames` frames.
    pub fn configure(&mut self, rows: usize, frames: u64) {
        self.rows = rows.min(MAX_ROWS);
        self.frames = frames;
        self.per_column = frames.div_ceil(OVERVIEW_COLUMNS as u64).max(1);
        self.columns = (frames.div_ceil(self.per_column) as usize).min(OVERVIEW_COLUMNS);
        for row in &mut self.data[..self.rows] {
            row.fill([i16::MAX, i16::MIN]);
        }
        self.reached = [0; MAX_ROWS];
    }

    /// Takes `samples` of `row` that start at frame `at`.
    pub fn push(&mut self, row: usize, at: u64, samples: &[f32]) {
        if row >= self.rows {
            return;
        }
        for (index, &sample) in samples.iter().enumerate() {
            let column = ((at + index as u64) / self.per_column) as usize;
            if column >= self.columns {
                break;
            }
            let value = (sample.clamp(-1.0, 1.0) * 32767.0) as i16;
            let cell = &mut self.data[row][column];
            *cell = [cell[0].min(value), cell[1].max(value)];
        }
        self.reached[row] = self.reached[row].max(at + samples.len() as u64);
    }

    /// Columns of the take (at most `OVERVIEW_COLUMNS`; fewer for a very short take).
    pub fn columns(&self) -> usize {
        self.columns
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Column `column` of `row` as (lowest, highest) in -1..1; None where nothing was read yet.
    pub fn column(&self, row: usize, column: usize) -> Option<(f32, f32)> {
        let [low, high] = *self.data.get(row)?.get(column)?;
        (row < self.rows && column < self.columns && low <= high).then(|| (f32::from(low) / 32767.0, f32::from(high) / 32767.0))
    }

    /// How far the slowest row has been read, 0..=1.
    pub fn progress(&self) -> f32 {
        if self.rows == 0 || self.frames == 0 {
            return 0.0;
        }
        self.reached[..self.rows].iter().min().map_or(0.0, |&least| (least as f32 / self.frames as f32).min(1.0))
    }
}

impl Default for Overview {
    fn default() -> Self {
        Self::new()
    }
}

/// The output pair a take plays to unless one was picked (`play-out`): the first output named "Phones..." (a second
/// USB model's), else the last pair named "Analog..." (a first USB model's phones are its analog 7/8), else the first
/// pair. Outputs are numbered from 0; the pair is (n, n+1).
pub fn default_output<'a>(count: usize, name: impl Fn(usize) -> &'a str) -> usize {
    if count < 2 {
        return 0;
    }
    let has = |index: usize, word: &str| name(index).get(..word.len()).is_some_and(|start| start.eq_ignore_ascii_case(word));
    if let Some(index) = (0..count - 1).find(|&index| has(index, "phones")) {
        return index;
    }
    if let Some(index) = (0..count - 1).rev().find(|&index| has(index, "analog") && has(index + 1, "analog") && index % 2 == 0) {
        return index;
    }
    0
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PlayState {
    Stopped,
    Playing,
    Paused,
}

/// What the page asks of the tape: list the projects (and the picked project's takes), open the picked take, start,
/// pause, stop (back to the start), go to a frame.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PlayRequest {
    List,
    Open,
    Play,
    Pause,
    Stop,
    Seek(u64),
}

/// Which list the Up/Down keys move in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Focus {
    Projects,
    Takes,
}

/// The Play page's state: the lists, the take open, which of its inputs are listened to, the output pair, the transport.
#[derive(Clone, Copy, Debug)]
pub struct Playing {
    pub projects: [ProjectName; MAX_LIST],
    pub project_count: usize,
    pub project: usize,
    pub takes: [TakeName; MAX_LIST],
    pub take_count: usize,
    pub take: usize,
    pub focus: Focus,
    /// The open take: its project and folder, its files, and its inputs as rows (input numbers, 0-based, ascending).
    pub open: Option<(ProjectName, TakeName)>,
    pub files: [TakeFile; MAX_FILES],
    pub file_count: usize,
    pub rows: [u8; MAX_ROWS],
    pub row_count: usize,
    /// The selected row (`,` `.`), and the rows listened to (bit per row).
    pub row: usize,
    pub listen: u32,
    /// The first playback channel of the output pair, and the device's playback channels; None until the tape says
    /// whether playback runs (`available`).
    pub output: Option<usize>,
    pub outputs: usize,
    pub available: bool,
    pub state: PlayState,
    /// Frames from the take's start: where playing is, and the take's length.
    pub position: u64,
    pub length: u64,
    pub rate: u32,
    pub request: Option<PlayRequest>,
    /// Play once the take asked for (`PlayRequest::Open`) is open.
    pub autoplay: bool,
}

impl Playing {
    pub const fn new() -> Self {
        Playing {
            projects: [ProjectName::new(); MAX_LIST],
            project_count: 0,
            project: 0,
            takes: [TakeName::new(); MAX_LIST],
            take_count: 0,
            take: 0,
            focus: Focus::Projects,
            open: None,
            files: [TakeFile::EMPTY; MAX_FILES],
            file_count: 0,
            rows: [0; MAX_ROWS],
            row_count: 0,
            row: 0,
            listen: 0,
            output: None,
            outputs: 0,
            available: false,
            state: PlayState::Stopped,
            position: 0,
            length: 0,
            rate: 0,
            request: None,
            autoplay: false,
        }
    }

    /// The take's files (from its folder, any order): kept in input order, the rows from them, the first two listened to
    /// (a stereo pair is usually inputs 1 and 2).
    pub fn set_files(&mut self, files: &[TakeFile]) {
        self.file_count = files.len().min(MAX_FILES);
        self.files[..self.file_count].copy_from_slice(&files[..self.file_count]);
        self.files[..self.file_count].sort_unstable_by_key(|file| file.first);
        self.row_count = 0;
        for file in &self.files[..self.file_count] {
            for input in file.first..file.first + file.channels {
                if self.row_count < MAX_ROWS && !self.rows[..self.row_count].contains(&input) {
                    self.rows[self.row_count] = input;
                    self.row_count += 1;
                }
            }
        }
        self.rows[..self.row_count].sort_unstable();
        self.row = 0;
        self.listen = if self.row_count >= 2 { 0b11 } else { 0b1 } & ((1u64 << self.row_count) - 1) as u32;
    }

    /// The row of take input `input`, if the take has it.
    pub fn row_of(&self, input: u8) -> Option<usize> {
        self.rows[..self.row_count].iter().position(|&r| r == input)
    }

    /// Listens to row `row`, or stops listening to it.
    pub fn toggle_listen(&mut self, row: usize) {
        if row < self.row_count {
            self.listen ^= 1 << row;
        }
    }

    /// Where each listened row goes: in row order, alternately left and right (inputs 1+2 are the stereo pair), one
    /// alone to both sides. `[left gain, right gain]` per row; each side is divided by how many rows it sums, so a
    /// full-scale pair stays full scale and more rows never clip.
    pub fn gains(&self) -> [[f32; 2]; MAX_ROWS] {
        let mut gains = [[0.0; 2]; MAX_ROWS];
        let listened = |row: usize| row < self.row_count && self.listen & 1 << row != 0;
        let count = (0..MAX_ROWS).filter(|&row| listened(row)).count();
        if count == 1 {
            let row = (0..MAX_ROWS).find(|&row| listened(row)).unwrap_or(0);
            gains[row] = [1.0, 1.0];
            return gains;
        }
        let (left, right) = (count.div_ceil(2) as f32, (count / 2) as f32);
        for (index, row) in (0..MAX_ROWS).filter(|&row| listened(row)).enumerate() {
            gains[row] = if index % 2 == 0 { [1.0 / left, 0.0] } else { [0.0, 1.0 / right.max(1.0)] };
        }
        gains
    }

    /// One frame of the output pair from one frame of the take's rows (`frame[row]`).
    pub fn mix(gains: &[[f32; 2]; MAX_ROWS], frame: &[f32]) -> (f32, f32) {
        frame.iter().zip(gains).fold((0.0, 0.0), |(l, r), (&sample, gain)| (l + sample * gain[0], r + sample * gain[1]))
    }

    /// The take's position and length as seconds.
    pub fn seconds(&self, frames: u64) -> u64 {
        frames / u64::from(self.rate.max(1))
    }

    /// Asks to go `seconds` from where playing is (negative: back), within the take.
    pub fn seek_by(&mut self, seconds: i64) {
        let rate = i64::from(self.rate.max(1));
        let target = (self.position as i64 + seconds * rate).clamp(0, self.length as i64);
        self.request = Some(PlayRequest::Seek(target as u64));
        self.position = target as u64;
    }

    /// Asks to go to `fraction` (0..=1) of the take.
    pub fn seek_to(&mut self, fraction: f32) {
        let target = (self.length as f64 * f64::from(fraction.clamp(0.0, 1.0))) as u64;
        self.request = Some(PlayRequest::Seek(target.min(self.length)));
        self.position = target.min(self.length);
    }

    /// `play-out=N`: the output pair picked (its first output, 1-based), when one was.
    pub fn load<'a>(&mut self, value: impl Fn(&str) -> Option<&'a str>) {
        if let Some(first) = value("play-out").and_then(|v| v.parse::<usize>().ok()).filter(|&n| n >= 1) {
            self.output = Some(first - 1);
        }
    }

    pub fn save(&self, mut put: impl FnMut(&str, core::fmt::Arguments)) {
        if let Some(output) = self.output {
            put("play-out", format_args!("{}", output + 1));
        }
    }

    /// The next output pair (wrapping).
    pub fn next_output(&mut self) {
        if self.outputs >= 2 {
            let next = self.output.map_or(0, |o| o + 2);
            self.output = Some(if next + 1 < self.outputs { next } else { 0 });
        }
    }
}

impl Default for Playing {
    fn default() -> Self {
        Self::new()
    }
}
