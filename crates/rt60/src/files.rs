//! The files a measurement leaves: a CSV of the band results and the impulse response as a 32-bit float WAV.

use crate::{Band, Report};
use core::fmt::{self, Write};

/// Header facts for the CSV.
pub struct Header<'a> {
    pub name: &'a str,
    pub method: &'a str,
    pub rate: u32,
    pub stamp: &'a str,
}

fn cell(out: &mut impl Write, value: f32, digits: usize) -> fmt::Result {
    if value.is_nan() { out.write_char(',') } else { write!(out, ",{value:.digits$}") }
}

/// One row per band (the last may be the overall figure, centre 0): `hz,edt_s,t20_s,t30_s,c50_db,c80_db,d50,dynamic_db`.
pub fn write_csv(out: &mut impl Write, header: &Header, band: &Band, reports: &[Report]) -> fmt::Result {
    writeln!(out, "# frt60 {} ({}), {} Hz, {}", header.name, header.method, header.rate, header.stamp)?;
    writeln!(out, "# {} bands; times in seconds scaled to 60 dB, clarity in dB, d50 a share, dynamic range in dB", band.name())?;
    writeln!(out, "hz,edt_s,t20_s,t30_s,c50_db,c80_db,d50,dynamic_db")?;
    for r in reports {
        if r.centre_hz.is_nan() {
            continue;
        }
        write!(out, "{}", if r.centre_hz == 0.0 { "all" } else { "" })?;
        if r.centre_hz != 0.0 {
            write!(out, "{:.1}", r.centre_hz)?;
        }
        cell(out, r.edt, 3)?;
        cell(out, r.t20, 3)?;
        cell(out, r.t30, 3)?;
        cell(out, r.c50, 1)?;
        cell(out, r.c80, 1)?;
        cell(out, r.d50, 3)?;
        cell(out, r.dynamic_db, 1)?;
        writeln!(out)?;
    }
    Ok(())
}

/// The 44-byte header of a mono IEEE-float WAV holding `frames` samples.
pub fn wav_header(rate: u32, frames: usize) -> [u8; 44] {
    let data = (frames * 4) as u32;
    let mut h = [0u8; 44];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(36 + data).to_le_bytes());
    h[8..16].copy_from_slice(b"WAVEfmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&3u16.to_le_bytes()); // IEEE float
    h[22..24].copy_from_slice(&1u16.to_le_bytes());
    h[24..28].copy_from_slice(&rate.to_le_bytes());
    h[28..32].copy_from_slice(&(rate * 4).to_le_bytes());
    h[32..34].copy_from_slice(&4u16.to_le_bytes());
    h[34..36].copy_from_slice(&32u16.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data.to_le_bytes());
    h
}
