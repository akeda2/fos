//! `cargo run --release --example decode -- IN.flac OUT.raw`: decodes to interleaved little-endian PCM (the sample size
//! rounded up to whole bytes), the layout FLAC's own MD5 signature covers.
use audio_decode::flac::Decoder;
use std::io::Write;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data = std::fs::read(&args[1]).unwrap();
    let mut decoder = Box::new(Decoder::new(&data[..]));
    decoder.open().unwrap();
    eprintln!("{} Hz, {} channels, {} bits, {} samples", decoder.sample_rate, decoder.channels, decoder.bits_per_sample, decoder.total_samples);
    let width = decoder.bits_per_sample.div_ceil(8) as usize;
    let mut out = std::io::BufWriter::new(std::fs::File::create(&args[2]).unwrap());
    let mut blocks = 0;
    let started = std::time::Instant::now();
    while let Some(n) = decoder.next_block().unwrap() {
        for i in 0..n {
            for c in 0..decoder.channels as usize {
                out.write_all(&decoder.samples[c][i].to_le_bytes()[..width]).unwrap();
            }
        }
        blocks += 1;
    }
    eprintln!("{blocks} blocks in {:?}", started.elapsed());
}
