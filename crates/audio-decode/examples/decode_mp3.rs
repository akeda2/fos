//! `cargo run --release --example decode_mp3 -- IN.mp3 OUT.raw`: decodes to interleaved stereo f32 little-endian.
use audio_decode::{Decode, mp3::Decoder};
use std::io::Write;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data = std::fs::read(&args[1]).unwrap();
    let mut decoder = Box::new(Decoder::new(&data[..]));
    decoder.open().unwrap();
    eprintln!("{} Hz, {} channels, about {} samples", decoder.sample_rate(), decoder.channels(), decoder.total_samples());
    let mut out = std::io::BufWriter::new(std::fs::File::create(&args[2]).unwrap());
    let mut pcm = vec![0f32; 2 * 1152];
    let mut frames = 0;
    let started = std::time::Instant::now();
    while let Some(n) = decoder.next_block().unwrap() {
        decoder.to_f32_stereo(0, n, &mut pcm);
        for x in &pcm[..2 * n] {
            out.write_all(&x.to_le_bytes()).unwrap();
        }
        frames += 1;
    }
    eprintln!("{frames} frames in {:?}", started.elapsed());
}
