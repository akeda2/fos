//! `cargo run --release --example decode -- IN.flac OUT.raw`: decodes to interleaved little-endian PCM (the sample size
//! rounded up to whole bytes), the layout FLAC's own MD5 signature covers. A third argument `A,B,C` seeks to those
//! samples in turn first and decodes from the last (`seek:` and the block's start go to stderr).
use audio_decode::flac::Decoder;
use std::io::Write;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data = std::fs::read(&args[1]).unwrap();
    let mut decoder = Box::new(Decoder::new(&data[..]));
    decoder.open().unwrap();
    eprintln!("{} Hz, {} channels, {} bits, {} samples", decoder.sample_rate, decoder.channels, decoder.bits_per_sample, decoder.total_samples);
    if args.get(3).map(String::as_str) == Some("frames") {
        // One `sample offset` line per frame, for a test that writes a SEEKTABLE of its own.
        let mut text = String::new();
        loop {
            let (sample, offset) = (decoder.position(), decoder.frame_offset());
            if decoder.next_block().unwrap().is_none() {
                break;
            }
            text += &format!("{sample} {offset}\n");
        }
        std::fs::write(&args[2], text).unwrap();
        return;
    }
    if let Some(list) = args.get(3) {
        for target in list.split(',') {
            let start = decoder.seek(target.parse().unwrap()).unwrap();
            eprintln!("seek: {target} -> {start}");
        }
    }
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
