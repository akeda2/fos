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

fn hex(d: [u8; 16]) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn md5_matches_the_rfc_vectors() {
    let md5 = |s: &[u8], chunk: usize| {
        let mut m = Md5::new();
        for c in s.chunks(chunk.max(1)) {
            m.update(c);
        }
        hex(m.finish())
    };
    assert_eq!(md5(b"", 1), "d41d8cd98f00b204e9800998ecf8427e");
    assert_eq!(md5(b"abc", 1), "900150983cd24fb0d6963f7d28e17f72");
    assert_eq!(md5(b"message digest", 5), "f96b697d7cb7938d525a2f31aaf161d0");
    let long = b"12345678901234567890123456789012345678901234567890123456789012345678901234567890";
    for chunk in [1, 7, 63, 64, 65, 1000] {
        assert_eq!(md5(long, chunk), "57edf4a22be3c955ac49da2e2107b67a");
    }
    // padding boundary: 55, 56 and 64 bytes
    assert_eq!(md5(&[b'a'; 56], 3), "3b0c8ac703f828b04c6c197006d17218");
}

/// The signature in STREAMINFO is MD5 of the interleaved little-endian samples, whole bytes each.
fn expected_md5(data: &[Vec<i32>], bits: u32) -> String {
    let width = (bits as usize).div_ceil(8);
    let mut m = Md5::new();
    for i in 0..data[0].len() {
        for c in data {
            m.update(&c[i].to_le_bytes()[..width]);
        }
    }
    hex(m.finish())
}

#[test]
fn streaminfo_carries_the_md5() {
    for (ch, bits) in [(2usize, 24u32), (2, 16), (1, 18), (4, 24)] {
        let data = signal(ch, 9000, bits, 30, 0);
        let file = encode(Info { sample_rate: 48000, channels: ch as u32, bits }, &data, BLOCK);
        assert_eq!(hex(file[26..42].try_into().unwrap()), expected_md5(&data, bits), "{ch} ch {bits} bit");
    }
    // Before any audio it stays zero ("not computed"); a checkpoint mid-stream signs the audio so far.
    let info = Info { sample_rate: 48000, channels: 2, bits: 24 };
    let mut enc = Box::new(Encoder::new(info));
    let mut header = vec![0u8; 256];
    let h = enc.write_header(&mut header, &[]).unwrap();
    assert_eq!(&header[26..42], &[0u8; 16]);
    let data = signal(2, 4096, 24, 30, 0);
    let mut out = vec![0u8; max_frame_bytes(2)];
    enc.encode_frame(&[&data[0], &data[1]], &mut out).unwrap();
    enc.patch_header(&mut header[..42]);
    assert_eq!(hex(header[26..42].try_into().unwrap()), expected_md5(&data, 24));
    assert!(h > 42);
}

/// ffmpeg's own MD5 of the decoded PCM must equal the signature we wrote.
#[test]
fn ffmpeg_agrees_with_the_signature() {
    if std::process::Command::new("ffmpeg").arg("-version").output().is_err() {
        eprintln!("ffmpeg not installed: skipped");
        return;
    }
    let data = signal(2, 12_000, 24, 30, 0);
    let file = encode(Info { sample_rate: 48000, channels: 2, bits: 24 }, &data, BLOCK);
    let path = std::env::temp_dir().join("flac-enc-test-md5.flac");
    std::fs::write(&path, &file).unwrap();
    let o = std::process::Command::new("ffmpeg").args(["-v", "error", "-i"]).arg(&path).args(["-c:a", "pcm_s24le", "-f", "md5", "-"]).output().unwrap();
    std::fs::remove_file(&path).ok();
    assert_eq!(String::from_utf8_lossy(&o.stdout).trim(), format!("MD5={}", hex(file[26..42].try_into().unwrap())));
}

/// A resonant signal (two poles near the unit circle, a little noise) that the fixed polynomials predict badly.
fn resonant(n: usize, bits: u32) -> Vec<i32> {
    let mut rng = Rng(4242);
    let (mut y1, mut y2) = (0f64, 0f64);
    let amp = (1i64 << (bits - 6)) as f64;
    (0..n)
        .map(|_| {
            let e = (rng.next() % 1000) as f64 / 1000.0 * amp * 0.02;
            let y = 1.97 * y1 - 0.985 * y2 + e;
            y2 = y1;
            y1 = y;
            y as i32
        })
        .collect()
}

#[test]
fn lpc_is_chosen_for_predictable_audio_and_beats_fixed() {
    let x = resonant(BLOCK, 20);
    let mut s = Box::new(Scratch { shifted: [0; BLOCK], res: [0; BLOCK], win: [0.0; BLOCK] });
    let plan = analyse(&x, 20, &mut s);
    assert!(plan.kind == Kind::Lpc, "expected LPC");
    // The fixed order-2 predictor on the same block, exact cost.
    let mut res = vec![0i32; BLOCK];
    residual(&x, 2, &mut res);
    let (fixed, _) = best_partition(&res[..BLOCK - 2], BLOCK, 2);
    assert!(plan.bits < fixed + 40 + 2 * 20, "lpc {} vs fixed {}", plan.bits, fixed);
    // It must round trip through our decoder and ffmpeg's rules (the in-tree decoder follows the spec).
    for bits in [16, 24] {
        let data = vec![resonant(20_000, bits), resonant(20_000, bits).iter().map(|v| v / 3).collect()];
        let file = encode(Info { sample_rate: 48000, channels: 2, bits }, &data, BLOCK);
        assert!(decode(&file).0 == data, "{bits}-bit LPC round trip");
    }
}

#[test]
fn lpc_shrinks_a_tonal_file() {
    // Reference: what the fixed predictors alone would need (orders 0-4, same Rice coder), per channel, summed.
    let data = vec![resonant(40_960, 24), resonant(40_960, 24)];
    let file = encode(Info { sample_rate: 48000, channels: 2, bits: 24 }, &data, BLOCK);
    let mut fixed_bits = 0u64;
    for ch in &data {
        for blk in ch.chunks(BLOCK) {
            let mut res = vec![0i32; BLOCK];
            let mut best = u64::MAX;
            for p in 0..=4 {
                residual(blk, p, &mut res);
                best = best.min(p as u64 * 24 + best_partition(&res[..BLOCK - p], BLOCK, p).0);
            }
            fixed_bits += best;
        }
    }
    assert!((file.len() as u64 * 8) < fixed_bits * 9 / 10, "{} bytes vs fixed {} bytes", file.len(), fixed_bits / 8);
}
