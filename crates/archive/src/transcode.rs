//! The second pass of the archive: a raw Lt/Rt take (a stereo FLAC) decoded into a 4.0, 4.1 or 5.1 FLAC.
//!
//! The bit depth of the decoded file follows the noise of the take: with `Depth::Auto` the whole take is decoded once to
//! measure the noise floor of the full-range channels (`flac_enc::depth::NoiseMeter`), then again to write the file at the
//! depth that keeps the rounding noise (TPDF dithered) `MARGIN_DB` under that floor, at least `MIN_BITS`. A fixed depth skips
//! the measuring pass. The centre channel of 5.1 is silent (written as zeros, not dithered); the LFE channel is a low-pass
//! of the fronts and is rounded like the others.
//!
//! The work is done in steps of a few blocks (`Transcoder::step`), so a tape keeps playing while it runs.

use crate::file::{self, FlacFile};
use audio_decode::flac::Decoder;
use audio_decode::Source;
use audio_rec::recorder::Sink;
use flac_enc::Info;
use flac_enc::depth::{NoiseMeter, Quantizer, lowest_floor, pick_bits};
use outfx::{Layout, Mode, Surround};

/// Rounding noise this far under the take's noise floor.
pub const MARGIN_DB: u32 = 12;
pub const MIN_BITS: u32 = 16;
/// Samples converted at a time.
const CHUNK: usize = 1024;
const FULL_SCALE_24: f32 = 8_388_608.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Depth {
    Auto,
    Bits(u32),
}

impl Depth {
    pub const CHOICES: [Depth; 5] = [Depth::Auto, Depth::Bits(16), Depth::Bits(18), Depth::Bits(20), Depth::Bits(24)];

    pub fn parse(text: &str) -> Option<Depth> {
        match text {
            "auto" => Some(Depth::Auto),
            "16" => Some(Depth::Bits(16)),
            "18" => Some(Depth::Bits(18)),
            "20" => Some(Depth::Bits(20)),
            "24" => Some(Depth::Bits(24)),
            _ => None,
        }
    }

    pub fn next(self) -> Depth {
        let at = Depth::CHOICES.iter().position(|&d| d == self).unwrap_or(0);
        Depth::CHOICES[(at + 1) % Depth::CHOICES.len()]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Decode(audio_decode::Error),
    File(file::Error),
    /// The raw take is not stereo.
    NotStereo,
    /// Surround is off: there is nothing to decode.
    NoDecoder,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Report {
    pub bits: u32,
    /// The noise floor of the take (dBFS, 10th percentile of 1024-sample blocks), when it was measured and not silent.
    pub floor_dbfs: Option<f32>,
    pub samples: u64,
    pub bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Progress {
    /// 0..=100 over both passes.
    Working(u32),
    Done(Report),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Measure,
    Begin,
    Write,
}

/// The output file and what goes into its header, for `Transcoder::step`.
pub struct Job<'a, S: Sink> {
    pub file: &'a mut FlacFile,
    pub sink: &'a mut S,
    /// The record equalisation the listener hears: the raw take is flat, the decoded file is equalised. `start` resets it.
    pub eq: &'a mut phono::Phono,
    pub name: &'a [u8],
    /// Vorbis comments for the header; the depth is added.
    pub tags: &'a [(&'a str, &'a str)],
}

pub struct Transcoder {
    phase: Phase,
    depth: Depth,
    layout: Layout,
    rate: u32,
    bits: u32,
    floor: Option<f32>,
    meters: [NoiseMeter; 4],
    quantizers: [Quantizer; file::MAX_CHANNELS],
    chunk: [[i32; CHUNK]; file::MAX_CHANNELS],
    done: u64,
    total: u64,
}

impl Transcoder {
    pub const fn new() -> Transcoder {
        Transcoder {
            phase: Phase::Idle,
            depth: Depth::Auto,
            layout: Layout::Quad40,
            rate: 48000,
            bits: 24,
            floor: None,
            meters: [NoiseMeter::new(), NoiseMeter::new(), NoiseMeter::new(), NoiseMeter::new()],
            quantizers: [Quantizer::new(24, 1), Quantizer::new(24, 1), Quantizer::new(24, 1), Quantizer::new(24, 1), Quantizer::new(24, 1), Quantizer::new(24, 1)],
            chunk: [[0; CHUNK]; file::MAX_CHANNELS],
            done: 0,
            total: 0,
        }
    }

    pub fn running(&self) -> bool {
        self.phase != Phase::Idle
    }

    /// Prepares a run over `dec` (already `open`ed) with the settings `surround` has been tuned to.
    pub fn start<Src: Source>(&mut self, dec: &mut Decoder<Src>, surround: &mut Surround, eq: &mut phono::Phono, depth: Depth) -> Result<(), Error> {
        if dec.channels != 2 {
            return Err(Error::NotStereo);
        }
        let settings = *surround.settings();
        if settings.mode == Mode::Off {
            return Err(Error::NoDecoder);
        }
        self.layout = settings.layout;
        self.rate = dec.sample_rate;
        self.total = dec.total_samples;
        self.depth = depth;
        self.floor = None;
        self.done = 0;
        self.meters = [NoiseMeter::new(), NoiseMeter::new(), NoiseMeter::new(), NoiseMeter::new()];
        dec.rewind();
        surround.restart(dec.sample_rate);
        eq.reset();
        self.phase = match depth {
            Depth::Auto => Phase::Measure,
            Depth::Bits(bits) => {
                self.bits = bits;
                Phase::Begin
            }
        };
        Ok(())
    }

    /// Stops a run without finishing the file (the caller abandons it).
    pub fn cancel(&mut self) {
        self.phase = Phase::Idle;
    }

    /// Works through up to `blocks` blocks of the take.
    pub fn step<Src: Source, S: Sink>(&mut self, dec: &mut Decoder<Src>, surround: &mut Surround, job: &mut Job<S>, blocks: usize) -> Result<Progress, Error> {
        let result = self.work(dec, surround, job, blocks);
        if result.is_err() {
            job.file.abandon(job.sink);
            self.phase = Phase::Idle;
        }
        result
    }

    fn percent(&self) -> u32 {
        if self.total == 0 {
            return 0;
        }
        let part = (self.done.min(self.total) * 100 / self.total) as u32;
        match (self.depth, self.phase) {
            (Depth::Auto, Phase::Measure) => part / 2,
            (Depth::Auto, _) => 50 + part / 2,
            _ => part,
        }
    }

    fn work<Src: Source, S: Sink>(&mut self, dec: &mut Decoder<Src>, surround: &mut Surround, job: &mut Job<S>, blocks: usize) -> Result<Progress, Error> {
        if self.phase == Phase::Begin {
            let mut digits = [0u8; 2];
            let text = bits_text(self.bits, &mut digits);
            let mut tags = [("", ""); 8];
            let base = job.tags.len().min(6);
            tags[..base].copy_from_slice(&job.tags[..base]);
            tags[base] = ("BITDEPTH", text);
            let info = Info { sample_rate: self.rate, channels: self.layout.channels() as u32, bits: self.bits };
            job.file.begin(job.sink, job.name, 0, info, &tags[..base + 1]).map_err(Error::File)?;
            let bits = self.bits;
            self.quantizers = core::array::from_fn(|c| Quantizer::new(bits, 0x9E37_79B9u32.wrapping_mul(c as u32 + 1)));
            self.phase = Phase::Write;
        }
        for _ in 0..blocks {
            match self.phase {
                Phase::Measure => {
                    let Some(count) = dec.next_block().map_err(Error::Decode)? else {
                        self.floor = self.floor_db();
                        let bin = lowest_floor(&self.meters[..self.layout.full_range().len()]);
                        self.bits = pick_bits(bin, MARGIN_DB, MIN_BITS);
                        self.done = 0;
                        dec.rewind();
                        surround.restart(self.rate);
                        job.eq.reset();
                        self.phase = Phase::Begin;
                        return Ok(Progress::Working(self.percent()));
                    };
                    self.convert(dec, surround, count, false, job)?;
                }
                Phase::Write => {
                    let Some(count) = dec.next_block().map_err(Error::Decode)? else {
                        let summary = job.file.finish(job.sink).map_err(Error::File)?;
                        self.phase = Phase::Idle;
                        return Ok(Progress::Done(Report { bits: self.bits, floor_dbfs: self.floor, samples: summary.samples, bytes: summary.bytes }));
                    };
                    self.convert(dec, surround, count, true, job)?;
                }
                _ => break,
            }
        }
        Ok(Progress::Working(self.percent()))
    }

    fn floor_db(&self) -> Option<f32> {
        self.meters[..self.layout.full_range().len()].iter().filter_map(|m| m.floor_dbfs()).fold(None, |lowest: Option<f32>, v| Some(lowest.map_or(v, |l| l.min(v))))
    }

    /// Decodes `count` samples of the block in `dec` to the layout's channels in `self.chunk`, a chunk at a time, and
    /// either measures them or writes them.
    fn convert<Src: Source, S: Sink>(&mut self, dec: &Decoder<Src>, surround: &mut Surround, count: usize, write: bool, job: &mut Job<S>) -> Result<(), Error> {
        let scale = 1.0 / (1u32 << (dec.bits_per_sample - 1)) as f32;
        let channels = self.layout.channels();
        let lfe = self.layout.lfe_index();
        let full = self.layout.full_range();
        let mut start = 0;
        while start < count {
            let n = (count - start).min(CHUNK);
            for i in 0..n {
                let (left, right) = job.eq.process_frame(dec.samples[0][start + i] as f32 * scale, dec.samples[1][start + i] as f32 * scale);
                let out = surround.process([left, right]);
                for c in 0..channels {
                    let silent_centre = !full.contains(&c) && Some(c) != lfe;
                    self.chunk[c][i] = if silent_centre {
                        0
                    } else if write {
                        self.quantizers[c].quantize(out[c]) >> (24 - self.bits)
                    } else {
                        to_i24(out[c])
                    };
                }
            }
            if write {
                let refs: [&[i32]; file::MAX_CHANNELS] = core::array::from_fn(|c| &self.chunk[c][..n]);
                job.file.push(job.sink, &refs[..channels]).map_err(Error::File)?;
            } else {
                for (meter, &c) in self.meters.iter_mut().zip(full) {
                    meter.feed(&self.chunk[c][..n]);
                }
            }
            start += n;
            self.done += n as u64;
        }
        Ok(())
    }
}

impl Default for Transcoder {
    fn default() -> Self {
        Self::new()
    }
}

fn to_i24(x: f32) -> i32 {
    let v = x * FULL_SCALE_24;
    let v = if v >= 0.0 { v + 0.5 } else { v - 0.5 };
    (v as i32).clamp(-8_388_608, 8_388_607)
}

fn bits_text(bits: u32, out: &mut [u8; 2]) -> &str {
    out[0] = b'0' + (bits / 10) as u8;
    out[1] = b'0' + (bits % 10) as u8;
    core::str::from_utf8(&out[..]).unwrap_or("24")
}
