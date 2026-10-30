use super::*;
use audio_decode::flac::Decoder;
use audio_rec::recorder::Sink;
use flac_enc::Info;
use outfx::{Layout, Mode, Surround, SurroundSettings};

/// A sink in memory that keeps the length the way a file would.
struct Memory {
    data: Vec<u8>,
    length: u64,
    capacity: u64,
    closed: Option<u64>,
}

impl Memory {
    fn new() -> Memory {
        Memory { data: Vec::new(), length: 0, capacity: u64::MAX, closed: None }
    }

    fn file(&self) -> &[u8] {
        &self.data[..self.length as usize]
    }
}

impl Sink for Memory {
    fn open(&mut self, _name: &[u8], _reserve: u64) -> Result<u64, i32> {
        Ok(self.capacity)
    }

    fn write(&mut self, offset: u64, data: &[u8], length: u64) -> Result<(), i32> {
        assert_eq!(offset % 512, 0);
        let end = offset as usize + data.len();
        if self.data.len() < end {
            self.data.resize(end, 0);
        }
        self.data[offset as usize..end].copy_from_slice(data);
        if length != 0 {
            self.length = length;
        }
        Ok(())
    }

    fn close(&mut self, length: u64) -> Result<(), i32> {
        self.length = length;
        self.closed = Some(length);
        Ok(())
    }
}

/// A noisy two-tone stereo signal, `amp` times full scale in 24-bit counts.
fn signal(frames: usize, amp: f32) -> [Vec<i32>; 2] {
    let mut seed = 12345u32;
    let mut left = Vec::new();
    let mut right = Vec::new();
    for i in 0..frames {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        let noise = ((seed >> 8) as f32 / 16_777_216.0 - 0.5) * 2.0;
        let t = i as f32 / 48000.0;
        let a = (t * 440.0 * 6.2831853).sin() * 0.5 + noise;
        let b = (t * 660.0 * 6.2831853).sin() * 0.5 - noise * 0.5;
        left.push((a * amp) as i32);
        right.push((b * amp) as i32);
    }
    [left, right]
}

fn write_raw(frames: &[Vec<i32>; 2], bits: u32, sink: &mut Memory) -> Vec<u8> {
    let mut file = Box::new(FlacFile::new());
    file.begin(sink, b"x", 0, Info { sample_rate: 48000, channels: 2, bits }, &[("TITLE", "t")]).unwrap();
    // uneven pushes: blocks must form across them
    let mut at = 0;
    let mut size = 1000;
    while at < frames[0].len() {
        let end = (at + size).min(frames[0].len());
        file.push(sink, &[&frames[0][at..end], &frames[1][at..end]]).unwrap();
        at = end;
        size = size % 5000 + 777;
    }
    file.finish(sink).unwrap();
    sink.file().to_vec()
}

fn decode_all(bytes: &[u8]) -> (Vec<Vec<i32>>, u32, u64) {
    let mut dec = Box::new(Decoder::new(Bytes(bytes)));
    dec.open().unwrap();
    let mut out = vec![Vec::new(), Vec::new()];
    while let Some(n) = dec.next_block().unwrap() {
        out[0].extend_from_slice(&dec.samples[0][..n]);
        out[1].extend_from_slice(&dec.samples[1][..n]);
    }
    (out, dec.bits_per_sample, dec.total_samples)
}

#[test]
fn a_flac_file_round_trips_with_its_final_streaminfo() {
    let frames = signal(50_000, 3_000_000.0);
    let mut sink = Memory::new();
    let bytes = write_raw(&frames, 24, &mut sink);
    assert_eq!(sink.closed, Some(bytes.len() as u64));
    let (decoded, bits, total) = decode_all(&bytes);
    assert_eq!((bits, total), (24, 50_000));
    assert_eq!(decoded[0], frames[0]);
    assert_eq!(decoded[1], frames[1]);
}

#[test]
fn a_checkpointed_file_is_complete_up_to_the_checkpoint() {
    let frames = signal(200_000, 1_000_000.0);
    let mut sink = Memory::new();
    let mut file = Box::new(FlacFile::new());
    file.begin(&mut sink, b"x", 0, Info { sample_rate: 48000, channels: 2, bits: 24 }, &[]).unwrap();
    file.push(&mut sink, &[&frames[0][..], &frames[1][..]]).unwrap();
    file.checkpoint(&mut sink).unwrap();
    // No finish: a reset here. The length and STREAMINFO were made to agree at the checkpoint.
    let (decoded, _, total) = {
        let mut dec = Box::new(Decoder::new(Bytes(sink.file())));
        dec.open().unwrap();
        let total = dec.total_samples;
        let mut count = 0u64;
        while let Ok(Some(n)) = dec.next_block() {
            count += n as u64;
        }
        (count, 0, total)
    };
    assert!(total > 0 && total <= 200_000 && total % 4096 == 0, "{total}");
    assert!(decoded >= total - 4096, "{decoded} {total}");
}

#[test]
fn a_tiny_file_has_the_right_header_too() {
    let mut sink = Memory::new();
    let mut file = Box::new(FlacFile::new());
    file.begin(&mut sink, b"x", 0, Info { sample_rate: 44100, channels: 2, bits: 16 }, &[]).unwrap();
    file.push(&mut sink, &[&[1, 2, 3], &[4, 5, 6]]).unwrap();
    let summary = file.finish(&mut sink).unwrap();
    assert_eq!(summary.samples, 3);
    let (decoded, bits, total) = decode_all(sink.file());
    assert_eq!((bits, total, &decoded[0][..], &decoded[1][..]), (16, 3, &[1, 2, 3][..], &[4, 5, 6][..]));
}

#[test]
fn a_full_sink_stops_the_file() {
    let frames = signal(20_000, 4_000_000.0);
    let mut sink = Memory::new();
    sink.capacity = 20_000;
    let mut file = Box::new(FlacFile::new());
    file.begin(&mut sink, b"x", 0, Info { sample_rate: 48000, channels: 2, bits: 24 }, &[]).unwrap();
    assert_eq!(file.push(&mut sink, &[&frames[0][..], &frames[1][..]]), Err(file::Error::Full));
}

#[test]
fn names_follow_the_base_and_number_themselves() {
    let names = Names::parse("lp").unwrap();
    assert_eq!(names.raw(1).as_str(), "lp.lr.flac");
    assert_eq!(names.decoded(1).as_str(), "lp.flac");
    assert_eq!(names.raw(12).as_str(), "lp12.lr.flac");
    let taken = ["lp.lr.flac", "lp2.flac"];
    assert_eq!(names.first_free(|n| taken.contains(&core::str::from_utf8(n).unwrap())), Some(3));
    assert_eq!(Names::decoded_for(b"lp2.lr.flac").unwrap().as_str(), "lp2.flac");
    assert!(Names::decoded_for(b"lp2.flac").is_none());
    assert_eq!(Names::parse("disk2/side-a.1").unwrap().raw(1).as_str(), "disk2/side-a.1.lr.flac");
    assert_eq!(Names::parse("@music/lp").unwrap().decoded(2).as_str(), "@music/lp2.flac");
}

#[test]
fn bad_names_are_refused_by_what_the_volume_allows() {
    assert_eq!(Names::parse("").unwrap_err(), NameError::Empty);
    assert_eq!(Names::parse("disk2/").unwrap_err(), NameError::Empty);
    assert_eq!(Names::parse("Lp").unwrap_err(), NameError::BadChar);
    assert_eq!(Names::parse("a b").unwrap_err(), NameError::BadChar);
    assert_eq!(Names::parse(".hidden").unwrap_err(), NameError::BadChar);
    assert_eq!(Names::parse("usb/lp").unwrap_err(), NameError::BadChar);
    assert_eq!(Names::parse("sevench").unwrap_err(), NameError::TooLong(6));
    assert!(Names::parse("disk3/a-longer-record-name").is_ok());
    assert_eq!(names::typed(b'Q'), Some(b'q'));
    assert_eq!(names::typed(b' '), None);
}

fn settings(mode: Mode, layout: Layout) -> SurroundSettings {
    SurroundSettings { mode, layout, ..SurroundSettings::DEFAULT }
}

/// Runs the second pass over `raw` to the end.
fn transcode(raw: &[u8], settings: SurroundSettings, depth: Depth) -> (Vec<u8>, Report) {
    transcode_trim(raw, settings, depth, 0.0)
}

fn transcode_trim(raw: &[u8], settings: SurroundSettings, depth: Depth, trim_db: f32) -> (Vec<u8>, Report) {
    let mut dec = Box::new(Decoder::new(Bytes(raw)));
    dec.open().unwrap();
    let mut surround = Box::new(Surround::new());
    surround.tune(dec.sample_rate, settings);
    let mut eq = Box::new(phono::Phono::new(dec.sample_rate));
    eq.set_curve(phono::curves::find("flat").unwrap());
    eq.set_trim_db(trim_db);
    let mut transcoder = Box::new(Transcoder::new());
    transcoder.start(&mut dec, &mut surround, &mut eq, depth).unwrap();
    let mut sink = Memory::new();
    let mut file = Box::new(FlacFile::new());
    let mut last = 0;
    loop {
        let mut job = transcode::Job { file: &mut file, sink: &mut sink, eq: &mut eq, name: b"out", tags: &[("DECODER", "test")] };
        match transcoder.step(&mut dec, &mut surround, &mut job, 2).expect("step") {
            Progress::Working(percent) => {
                assert!(percent >= last && percent <= 100, "{percent} after {last}");
                last = percent;
            }
            Progress::Done(report) => return (sink.file().to_vec(), report),
        }
    }
}

#[test]
fn a_noisy_take_gets_a_lower_depth_than_a_quiet_floor() {
    // Noise of about 2^17 counts rms in 24 bits is -42 dBFS: 16 bits is plenty.
    let noisy = signal(120_000, 400_000.0);
    let mut sink = Memory::new();
    let raw = write_raw(&noisy, 24, &mut sink);
    let (out, report) = transcode(&raw, settings(Mode::Qs, Layout::Quad40), Depth::Auto);
    assert_eq!(report.bits, 16, "{report:?}");
    assert!(report.floor_dbfs.unwrap() > -60.0);
    let mut dec = Box::new(Decoder::new(Bytes(&out)));
    assert!(dec.open().is_err(), "the in-tree decoder only takes stereo; four channels are checked with ffmpeg");

    // A near-silent, low-noise take keeps more bits.
    let quiet = signal(120_000, 40.0);
    let mut sink = Memory::new();
    let raw = write_raw(&quiet, 24, &mut sink);
    let (_, report) = transcode(&raw, settings(Mode::Qs, Layout::Quad40), Depth::Auto);
    assert!(report.bits >= 20, "{report:?}");
}

#[test]
fn a_fixed_depth_is_used_and_tagged() {
    let frames = signal(30_000, 1_000_000.0);
    let mut sink = Memory::new();
    let raw = write_raw(&frames, 24, &mut sink);
    let (out, report) = transcode(&raw, settings(Mode::Sq, Layout::Surround51), Depth::Bits(18));
    assert_eq!(report.bits, 18);
    assert!(out.windows(8).any(|w| w == b"BITDEPTH") && out.windows(10).any(|w| w == b"DECODER=te"));
}

#[test]
fn surround_off_and_a_mono_take_are_refused() {
    let frames = signal(5000, 1000.0);
    let mut sink = Memory::new();
    let raw = write_raw(&frames, 24, &mut sink);
    let mut dec = Box::new(Decoder::new(Bytes(&raw)));
    dec.open().unwrap();
    let mut surround = Box::new(Surround::new());
    surround.tune(48000, settings(Mode::Off, Layout::Quad40));
    let mut transcoder = Box::new(Transcoder::new());
    let mut eq = Box::new(phono::Phono::new(48000));
    assert_eq!(transcoder.start(&mut dec, &mut surround, &mut eq, Depth::Auto), Err(transcode::Error::NoDecoder));
}

/// Four and six channels, decoded by ffmpeg: the decoded file must hold what the decoder makes, the centre silent.
#[test]
fn the_decoded_file_has_four_or_six_channels_ffmpeg_reads() {
    if std::process::Command::new("ffmpeg").arg("-version").output().is_err() {
        return;
    }
    let frames = signal(60_000, 2_000_000.0);
    let mut sink = Memory::new();
    let raw = write_raw(&frames, 24, &mut sink);
    for (layout, channels) in [(Layout::Quad40, 4), (Layout::Surround51, 6)] {
        let (out, _) = transcode(&raw, settings(Mode::Qs, layout), Depth::Bits(20));
        let dir = std::env::temp_dir().join(format!("archive-test-{}-{channels}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.flac");
        std::fs::write(&path, &out).unwrap();
        let run = std::process::Command::new("ffmpeg").args(["-v", "error", "-xerror", "-i"]).arg(&path).args(["-f", "s32le", "-"]).output().unwrap();
        assert!(run.status.success(), "{}", String::from_utf8_lossy(&run.stderr));
        assert_eq!(run.stdout.len(), 60_000 * channels * 4, "{channels} channels");
        if channels == 6 {
            for frame in run.stdout.chunks(24) {
                assert_eq!(&frame[8..12], &[0, 0, 0, 0], "the centre channel is silent");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn the_decoded_file_is_equalised() {
    if std::process::Command::new("ffmpeg").arg("-version").output().is_err() {
        return;
    }
    let frames = signal(30_000, 1_000_000.0);
    let mut sink = Memory::new();
    let raw = write_raw(&frames, 24, &mut sink);
    let peak = |out: &[u8], tag: &str| {
        let path = std::env::temp_dir().join(format!("archive-eq-{}-{tag}.flac", std::process::id()));
        std::fs::write(&path, out).unwrap();
        let run = std::process::Command::new("ffmpeg").args(["-v", "error", "-xerror", "-i"]).arg(&path).args(["-f", "s32le", "-"]).output().unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(run.status.success());
        run.stdout.chunks(4).map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]).unsigned_abs()).max().unwrap()
    };
    let (flat, _) = transcode_trim(&raw, settings(Mode::Sq, Layout::Quad40), Depth::Bits(24), 0.0);
    let (boosted, _) = transcode_trim(&raw, settings(Mode::Sq, Layout::Quad40), Depth::Bits(24), 6.0);
    let ratio = peak(&boosted, "b") as f32 / peak(&flat, "f") as f32;
    assert!((ratio - 2.0).abs() < 0.1, "{ratio}");
}
