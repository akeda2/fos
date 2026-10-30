use super::*;
use audio_decode::flac::Decoder;
use depth::*;

struct Rng(u32);
impl Rng {
    fn next(&mut self) -> i32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        self.0 as i32
    }
}

/// Interleaved test signal: a few tones plus noise of `noise` LSBs, in a `bits`-bit range, low `waste` bits clear.
fn signal(ch: usize, n: usize, bits: u32, noise: i32, waste: u32) -> Vec<Vec<i32>> {
    let mut rng = Rng(12345);
    let amp = (1i64 << (bits - 2)) as f64;
    (0..ch)
        .map(|c| {
            (0..n)
                .map(|i| {
                    let t = i as f64;
                    let v = amp * (0.6 * (t * 0.031 * (c + 1) as f64).sin() + 0.3 * (t * 0.173).sin());
                    let nz = if noise > 0 { rng.next() % noise } else { 0 };
                    (((v as i32 + nz) >> waste) << waste).clamp(-(1 << (bits - 1)), (1 << (bits - 1)) - 1)
                })
                .collect()
        })
        .collect()
}

fn encode(info: Info, data: &[Vec<i32>], block: usize) -> Vec<u8> {
    let mut enc = Box::new(Encoder::new(info));
    let mut file = vec![0u8; 1024];
    let h = enc.write_header(&mut file, &[("TITLE", "t"), ("FOSDEPTH", "18")]).unwrap();
    file.truncate(h);
    let n = data[0].len();
    let mut out = vec![0u8; max_frame_bytes(data.len())];
    let mut at = 0;
    while at < n {
        let end = (at + block).min(n);
        let chans: Vec<&[i32]> = data.iter().map(|c| &c[at..end]).collect();
        let len = enc.encode_frame(&chans, &mut out).unwrap();
        file.extend_from_slice(&out[..len]);
        at = end;
    }
    enc.patch_header(&mut file[..42]);
    file
}

fn decode(file: &[u8]) -> (Vec<Vec<i32>>, u64) {
    let mut d = Box::new(Decoder::new(file));
    d.open().unwrap();
    let ch = d.channels as usize;
    let mut out = vec![Vec::new(); ch];
    while let Some(n) = d.next_block().unwrap() {
        for c in 0..ch {
            out[c].extend_from_slice(&d.samples[c][..n]);
        }
    }
    (out, d.total_samples)
}

fn roundtrip(ch: usize, n: usize, bits: u32, noise: i32, waste: u32) -> usize {
    let data = signal(ch, n, bits, noise, waste);
    let file = encode(Info { sample_rate: 48000, channels: ch as u32, bits }, &data, BLOCK);
    let (back, total) = decode(&file);
    assert_eq!(total, n as u64);
    assert!(back == data, "{ch} ch {bits} bit round trip differs");
    file.len()
}

#[test]
fn mono_and_stereo_round_trip() {
    roundtrip(1, 10_000, 16, 50, 0);
    roundtrip(2, 10_000, 16, 50, 0);
    roundtrip(2, 10_000, 24, 300, 0);
}

#[test]
fn short_final_block_and_tiny_blocks() {
    for n in [1, 2, 3, 5, 17, 255, 257, 4097, 8200] {
        roundtrip(2, n, 24, 100, 0);
    }
}

#[test]
fn stereo_modes_all_get_used_and_decode() {
    // L = R (mid/side wins), R silent (left/side or independent), uncorrelated noise (independent).
    let n = 8192;
    let base = signal(1, n, 24, 200, 0).remove(0);
    let cases: [Vec<Vec<i32>>; 3] = [
        vec![base.clone(), base.clone()],
        vec![base.clone(), vec![0; n]],
        signal(2, n, 24, 100_000, 0),
    ];
    for data in cases {
        let file = encode(Info { sample_rate: 44100, channels: 2, bits: 24 }, &data, BLOCK);
        assert!(decode(&file).0 == data);
    }
}

#[test]
fn silence_and_full_scale() {
    let data = vec![vec![0i32; 5000]; 2];
    let file = encode(Info { sample_rate: 48000, channels: 2, bits: 24 }, &data, BLOCK);
    assert!(file.len() < 200, "silence should be tiny, was {}", file.len());
    assert!(decode(&file).0 == data);
    let mut rng = Rng(7);
    let extreme: Vec<Vec<i32>> = (0..2).map(|_| (0..4096).map(|_| if rng.next() & 1 == 0 { -(1 << 23) } else { (1 << 23) - 1 }).collect()).collect();
    let file = encode(Info { sample_rate: 48000, channels: 2, bits: 24 }, &extreme, BLOCK);
    assert!(decode(&file).0 == extreme);
}

#[test]
fn wasted_bits_make_a_smaller_file() {
    let full = roundtrip(2, 20_000, 24, 3_000_000, 0);
    let low = roundtrip(2, 20_000, 24, 3_000_000, 6);
    // Six of 24 bits gone from noisy audio is about a quarter of the size.
    assert!((low as f64) < full as f64 * 0.85, "{low} vs {full}");
}

#[test]
fn rejects_bad_input() {
    let mut enc = Box::new(Encoder::new(Info { sample_rate: 48000, channels: 2, bits: 16 }));
    let mut out = vec![0u8; max_frame_bytes(2)];
    let big = vec![40000i32; 10];
    assert_eq!(enc.encode_frame(&[&big, &big], &mut out), Err(Error::Range));
    assert_eq!(enc.encode_frame(&[&big], &mut out), Err(Error::BadBlock));
    assert_eq!(enc.encode_frame(&[&big[..5], &big], &mut out), Err(Error::BadBlock));
    let mut small = [0u8; 100];
    assert_eq!(enc.encode_frame(&[&[1, 2], &[1, 2]], &mut small), Err(Error::OutputTooSmall));
}

/// ffmpeg checks every CRC and reads more than two channels, which the in-tree decoder does not. Skipped when it
/// is not installed.
#[test]
fn ffmpeg_reads_multichannel() {
    if std::process::Command::new("ffmpeg").arg("-version").output().is_err() {
        eprintln!("ffmpeg not installed: skipped");
        return;
    }
    for (ch, bits) in [(2usize, 24u32), (4, 24), (6, 24), (6, 16), (1, 16)] {
        let n = 9000;
        let data = signal(ch, n, bits, 40, if bits == 24 { 5 } else { 0 });
        let file = encode(Info { sample_rate: 48000, channels: ch as u32, bits }, &data, BLOCK);
        let path = std::env::temp_dir().join(format!("flac-enc-test-{ch}-{bits}.flac"));
        std::fs::write(&path, &file).unwrap();
        let o = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-xerror", "-i"])
            .arg(&path)
            .args(["-f", "s32le", "-"])
            .output()
            .unwrap();
        std::fs::remove_file(&path).ok();
        assert!(o.status.success() && o.stderr.is_empty(), "{ch} ch: {}", String::from_utf8_lossy(&o.stderr));
        assert_eq!(o.stdout.len(), n * ch * 4);
        for i in 0..n {
            for c in 0..ch {
                let at = (i * ch + c) * 4;
                let got = i32::from_le_bytes(o.stdout[at..at + 4].try_into().unwrap());
                assert_eq!(got, data[c][i] << (32 - bits), "{ch} ch {bits} bit sample {i} ch {c}");
            }
        }
    }
}

fn noise_at(dbfs: f32, n: usize) -> Vec<i32> {
    let mut rng = Rng(99);
    // uniform noise of peak p has rms p/sqrt(3)
    let rms = 8_388_608.0 * 10f32.powf(dbfs / 20.0);
    let p = (rms * 3f32.sqrt()) as i64;
    (0..n).map(|_| (rng.next() as i64 % (p + 1)) as i32).collect()
}

#[test]
fn meter_finds_the_floor_and_ignores_silence() {
    let mut m = NoiseMeter::new();
    m.feed(&vec![0; 10 * METER_BLOCK]);
    assert_eq!(m.floor_bin(), None);
    // 20 blocks of -60 dB noise, 80 of loud music-like level.
    m.feed(&noise_at(-60.0, 20 * METER_BLOCK));
    m.feed(&noise_at(-18.0, 80 * METER_BLOCK));
    let f = m.floor_dbfs().unwrap();
    assert!((f + 60.0).abs() < 2.5, "floor {f}");
    // Chunk sizes do not matter.
    let mut m2 = NoiseMeter::new();
    for c in noise_at(-60.0, 20 * METER_BLOCK).chunks(777) {
        m2.feed(c);
    }
    assert_eq!(m2.blocks(), 20);
}

#[test]
fn depth_follows_the_floor() {
    let bin = |db: f32| {
        let mut m = NoiseMeter::new();
        m.feed(&noise_at(db, 50 * METER_BLOCK));
        m.floor_bin()
    };
    // dithered N-bit noise is -6.02 N dBFS: with 12 dB margin a -66 dB floor needs 13 bits, clamped to the minimum.
    assert_eq!(pick_bits(bin(-66.0), 12, 16), 16);
    // -100 dB floor: 6.02 N >= 112 -> 19 bits.
    let n = pick_bits(bin(-100.0), 12, 16);
    assert!((18..=20).contains(&n), "{n}");
    assert_eq!(pick_bits(bin(-130.0), 12, 16), 24);
    assert_eq!(pick_bits(None, 12, 16), 24);
}

#[test]
fn quantizer_keeps_the_grid_and_dither_is_unbiased() {
    let mut q = Quantizer::new(18, 5);
    let mut sum = 0f64;
    let n = 50_000;
    for i in 0..n {
        let x = 0.3 * (i as f32 * 0.01).sin();
        let v = q.quantize(x);
        assert_eq!(v & 63, 0, "low 6 bits of an 18-bit sample in a 24-bit container");
        sum += (v as f64 / 8_388_608.0 - x as f64).abs();
    }
    // error is about one LSB of the 18-bit grid (2^-17)
    assert!(sum / (n as f64) < 3.0 / 131_072.0);
    assert_eq!(q.quantize(2.0), (131_071) << 6);
    assert_eq!(q.quantize(-2.0), (-131_072) << 6);
    // archive-sized proof: quantised noisy audio compresses like 18 bits
    let data = signal(2, 20_000, 24, 3_000_000, 0);
    let mut q = Quantizer::new(16, 1);
    let qd: Vec<Vec<i32>> = data.iter().map(|c| c.iter().map(|&s| q.quantize_i24(s)).collect()).collect();
    let a = encode(Info { sample_rate: 48000, channels: 2, bits: 24 }, &data, BLOCK).len();
    let b = encode(Info { sample_rate: 48000, channels: 2, bits: 24 }, &qd, BLOCK).len();
    assert!(b < a * 3 / 4, "{b} vs {a}");
}
