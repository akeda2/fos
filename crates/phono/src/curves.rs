//! The playback curves, as time constants. Source: the table of H. O. Graumann, "Schallplatten-Schneidkennlinien und ihre
//! Entzerrung" (Funkschau 1958), as compiled in the Audacity plug-in documentation "Playback equalization for 78 rpm shellacs
//! and early 33 1/3 LPs"; the Columbia LP and London figures follow G. A. Galo and W. Leister (ARSC).
//!
//! A playback (de-emphasis) curve is `H(s) = (1 + s*t2) / ((1 + s*t1) * (1 + s*t3))`:
//! * `t2`: the bass turnover (`f2 = 1/(2*pi*t2)`), where the playback boost starts rising towards the bass;
//! * `t1`: the bass shelf (`f1`), below which the boost stops (RIAA: 3180 us, 50 Hz, 20 dB). Without it (`0`) the boost would
//!   rise for ever; the engine then puts the pole at 10 Hz, which changes nothing above 30 Hz;
//! * `t3`: the treble cut (`f3`); `0` means none ("flat" treble, the early European curves).

/// Which kind of disc a curve is for, to group a list.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Groove {
    /// 78 rpm, normal groove.
    Normal78,
    /// 33 1/3 and 45 rpm, microgroove.
    Micro,
}

#[derive(Clone, Copy, Debug)]
pub struct Curve {
    /// What a settings file or the command line calls it: lower case letters, digits, `-`.
    pub id: &'static str,
    pub name: &'static str,
    /// At most 8 characters, for a button.
    pub short: &'static str,
    pub period: &'static str,
    pub groove: Groove,
    /// Bass shelf time constant in microseconds, 0 = none.
    pub t1_us: f32,
    /// Bass turnover time constant in microseconds.
    pub t2_us: f32,
    /// Treble cut time constant in microseconds, 0 = none.
    pub t3_us: f32,
    /// True for the IEC variant of RIAA: the standard adds a 20 Hz rumble roll-off (7950 us) to the curve.
    pub iec_rumble: bool,
}

const fn c(id: &'static str, name: &'static str, short: &'static str, period: &'static str, groove: Groove, t1_us: f32, t2_us: f32, t3_us: f32) -> Curve {
    Curve { id, name, short, period, groove, t1_us, t2_us, t3_us, iec_rumble: false }
}

use Groove::{Micro, Normal78};

/// Every curve, microgroove first, RIAA at index `RIAA`.
pub const CURVES: [Curve; 21] = [
    c("riaa", "RIAA (RCA New Orthophonic)", "RIAA", "1953 -", Micro, 3180.0, 318.0, 75.0),
    Curve { id: "iec", name: "IEC RIAA (with 20 Hz roll-off)", short: "IEC", period: "1955 -", groove: Micro, t1_us: 3180.0, t2_us: 318.0, t3_us: 75.0, iec_rumble: true },
    c("columbia-lp", "Columbia LP (M33)", "Col LP", "1948 - 1956", Micro, 1590.0, 318.0, 100.0),
    c("nab", "NAB / NARTB", "NAB", "1949 - 1958", Micro, 3180.0, 318.0, 100.0),
    c("nab-1942", "NAB transcriptions", "NAB 42", "1942 - 1949", Micro, 0.0, 318.0, 100.0),
    c("aes", "AES", "AES", "1951 - 1958", Micro, 0.0, 398.0, 63.7),
    c("london-lp", "London LP (Decca LP)", "London", "1949 - 1956", Micro, 1590.0, 318.0, 50.0),
    c("teldec", "Teldec (DIN proposal)", "Teldec", "1955 - 1962", Micro, 3180.0, 318.0, 50.0),
    c("rca-lp", "RCA Old Orthophonic (LP)", "RCA LP", "1947 - 1952", Micro, 0.0, 318.0, 75.0),
    c("rca-45", "RCA 45 rpm", "RCA 45", "1947 - 1952", Micro, 0.0, 200.0, 75.0),
    c("iec-78", "IEC N78 / BS 1928", "IEC 78", "1955 -", Normal78, 3180.0, 450.0, 50.0),
    c("ccir-78", "CCIR 78", "CCIR 78", "1952 - 1955", Normal78, 0.0, 450.0, 50.0),
    c("decca-78", "Decca / FFRR 78", "Decca 78", "1944 - 1956", Normal78, 0.0, 531.0, 25.0),
    c("columbia-78", "Columbia 78", "Col 78", "1938 - 1948", Normal78, 0.0, 530.0, 100.0),
    c("rca-78", "Old RCA 78", "RCA 78", "1941 - 1947", Normal78, 0.0, 199.0, 36.7),
    c("euro-78", "European 78 / EMI 250 Hz", "Euro 78", "1926 - 1946", Normal78, 0.0, 636.0, 0.0),
    c("blumlein-300", "Blumlein 300 Hz", "Blum 300", "1927 - 1946", Normal78, 0.0, 531.0, 0.0),
    c("flat-500", "500 Hz flat treble (Blumlein 500)", "Flat 500", "1926 - 1950", Normal78, 0.0, 318.0, 0.0),
    c("american-78-6", "American 78, -6 dB at 10 kHz", "Am 78-6", "1926 - 1951", Normal78, 0.0, 636.0, 26.97),
    c("american-78-8", "American 78, -8 dB at 10 kHz", "Am 78-8", "1926 - 1951", Normal78, 0.0, 636.0, 36.2),
    c("flat", "Flat (no equalisation)", "Flat", "", Micro, 0.0, 0.0, 0.0),
];

pub const RIAA: usize = 0;
pub const FLAT: usize = CURVES.len() - 1;

/// The curve with this id (case-insensitive, `_` accepted for `-`).
pub fn find(name: &str) -> Option<usize> {
    let same = |id: &str| id.len() == name.len() && id.bytes().zip(name.bytes()).all(|(a, b)| a == if b == b'_' { b'-' } else { b.to_ascii_lowercase() });
    CURVES.iter().position(|curve| same(curve.id))
}
