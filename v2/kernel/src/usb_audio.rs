//! USB Audio Class devices: recognise them and report what they offer.
//!
//! An audio device's configuration holds one *AudioControl* interface (class 1, subclass 1:
//! the topology, and for Audio Class 2.0 the clock sources) and one or more *AudioStreaming*
//! interfaces (subclass 2). A streaming interface has an *alternate setting 0* with no
//! endpoints (the idle state) and further alternate settings, each describing one format
//! (channels, bit resolution) and carrying the isochronous endpoint audio flows over; the host
//! selects an alternate setting to start streaming. Class 1.0 (what the old TerraTec card used)
//! lists its sample rates in the format descriptor; class 2.0 (most modern interfaces) asks the
//! *clock source* entity with a `GET RANGE` request instead.
//!
//! This module only parses and prints, so the first look at a new device shows exactly what it
//! presents before any streaming code exists.

#[derive(Clone, Copy, Default)]
pub struct StreamAlt {
    pub interface: u8,
    pub alt: u8,
    pub channels: u8,
    /// Bytes per sample slot in the isochronous packets, and how many of its bits carry audio.
    pub subslot_bytes: u8,
    pub bits: u8,
    pub endpoint: u8,
    /// Bits 3:2 of the endpoint's attributes: 0 none, 1 asynchronous, 2 adaptive, 3 synchronous.
    pub sync: u8,
    pub max_packet: u16,
    pub interval: u8,
    /// The explicit feedback endpoint of an asynchronous OUT stream, if any.
    pub feedback: u8,
    /// Class 1.0 lists its discrete sample rates in the format descriptor (0 rates = continuous range).
    pub rates: [u32; 6],
    pub rate_count: usize,
    pub continuous_range: (u32, u32),
}

pub const MAX_ALTS: usize = 16;

pub struct Description {
    /// 1 or 2 (Audio Class 1.0 / 2.0), 0 if the control interface said nothing.
    pub class_version: u8,
    pub control_interface: u8,
    pub clock_sources: [u8; 4],
    pub clock_count: usize,
    pub alts: [StreamAlt; MAX_ALTS],
    pub alt_count: usize,
}

/// True if the configuration has an AudioStreaming interface.
pub fn is_audio(configuration: &[u8]) -> bool {
    let mut at = 0;
    while at + 9 <= configuration.len() {
        let length = usize::from(configuration[at]);
        if length < 2 || at + length > configuration.len() {
            break;
        }
        if configuration[at + 1] == 4 && configuration[at + 5] == 1 && configuration[at + 6] == 2 {
            return true;
        }
        at += length;
    }
    false
}

/// Walks the configuration descriptor and collects every streaming alternate setting that has a
/// data endpoint.
pub fn parse(configuration: &[u8]) -> Description {
    let mut description = Description { class_version: 0, control_interface: 0, clock_sources: [0; 4], clock_count: 0, alts: [StreamAlt::default(); MAX_ALTS], alt_count: 0 };
    let (mut class, mut subclass) = (0u8, 0u8);
    let mut current = StreamAlt::default();
    let mut uac2_stream = false;
    let mut at = 0;
    let flush = |description: &mut Description, current: &StreamAlt| {
        if current.endpoint != 0 && description.alt_count < MAX_ALTS {
            description.alts[description.alt_count] = *current;
            description.alt_count += 1;
        }
    };
    while at + 2 <= configuration.len() {
        let length = usize::from(configuration[at]);
        if length < 2 || at + length > configuration.len() {
            break;
        }
        let d = &configuration[at..at + length];
        match d[1] {
            4 if length >= 9 => {
                if subclass == 2 {
                    flush(&mut description, &current);
                }
                class = d[5];
                subclass = d[6];
                uac2_stream = d[7] == 0x20;
                current = StreamAlt { interface: d[2], alt: d[3], ..StreamAlt::default() };
                if class == 1 && subclass == 1 {
                    description.control_interface = d[2];
                }
                if class == 1 && uac2_stream && description.class_version == 0 {
                    description.class_version = 2;
                }
            }
            0x24 if class == 1 && length >= 4 => match (subclass, d[2]) {
                (1, 1) if length >= 6 => description.class_version = if u16::from_le_bytes([d[3], d[4]]) >= 0x0200 { 2 } else { 1 },
                (1, 0x0a) if description.clock_count < description.clock_sources.len() => {
                    description.clock_sources[description.clock_count] = d[3];
                    description.clock_count += 1;
                }
                (2, 1) if uac2_stream && length >= 11 => current.channels = d[10], // AS_GENERAL (class 2.0)
                (2, 2) if uac2_stream && length >= 6 => {
                    current.subslot_bytes = d[4];
                    current.bits = d[5];
                }
                (2, 2) if !uac2_stream && length >= 8 => {
                    // FORMAT_TYPE_I (class 1.0): channels, subframe size, bit resolution
                    current.channels = d[4];
                    current.subslot_bytes = d[5];
                    current.bits = d[6];
                    let rate = |at: usize| u32::from(d[at]) | u32::from(d[at + 1]) << 8 | u32::from(d[at + 2]) << 16;
                    let kinds = usize::from(d[7]);
                    if kinds == 0 && length >= 14 {
                        current.continuous_range = (rate(8), rate(11)); // lower and upper limit
                    }
                    for index in 0..kinds.min(current.rates.len()) {
                        if length >= 8 + 3 * (index + 1) {
                            current.rates[index] = rate(8 + 3 * index);
                            current.rate_count = index + 1;
                        }
                    }
                }
                _ => {}
            },
            5 if subclass == 2 && length >= 7 => {
                let attributes = d[3];
                if attributes & 3 == 1 {
                    // isochronous: a data endpoint (usage 0 or 2) or an explicit feedback one (usage 1)
                    if (attributes >> 4) & 3 == 1 {
                        current.feedback = d[2];
                    } else if current.endpoint == 0 {
                        current.endpoint = d[2];
                        current.sync = (attributes >> 2) & 3;
                        current.max_packet = u16::from_le_bytes([d[4], d[5]]) & 0x7ff;
                        current.interval = d[6];
                    }
                }
            }
            _ => {}
        }
        at += length;
    }
    if subclass == 2 {
        flush(&mut description, &current);
    }
    description
}

fn sync_name(sync: u8) -> &'static str {
    match sync {
        0 => "no synchronisation",
        1 => "asynchronous",
        2 => "adaptive",
        _ => "synchronous",
    }
}

/// Prints what a device offers, one line per usable alternate setting.
pub fn report(description: &Description, path: &dyn core::fmt::Display, vendor: u16, product: u16, speed: &str) {
    crate::println!(
        "USB audio {vendor:04x}:{product:04x} on port {path} ({speed}): Audio Class {}, {} clock source(s), {} streaming alternate setting(s).",
        match description.class_version {
            2 => "2.0",
            1 => "1.0",
            _ => "(version not stated)",
        },
        description.clock_count,
        description.alt_count
    );
    for alt in &description.alts[..description.alt_count] {
        crate::println!(
            "  interface {} alt {}: {} {} ch, {}-bit in {}-byte slots, endpoint {:#04x} isochronous ({}), max packet {}, interval {}",
            alt.interface,
            alt.alt,
            if alt.endpoint & 0x80 != 0 { "capture" } else { "playback" },
            alt.channels,
            alt.bits,
            alt.subslot_bytes,
            alt.endpoint,
            sync_name(alt.sync),
            alt.max_packet,
            alt.interval
        );
        if alt.rate_count > 0 {
            crate::print(format_args!("    sample rates:"));
            for rate in &alt.rates[..alt.rate_count] {
                crate::print(format_args!(" {rate}"));
            }
            crate::println!(" Hz");
        } else if alt.continuous_range.1 != 0 {
            crate::println!("    sample rates: {} to {} Hz (continuous)", alt.continuous_range.0, alt.continuous_range.1);
        }
        if alt.feedback != 0 {
            crate::println!("    feedback endpoint {:#04x} (the device's real sample clock)", alt.feedback);
        }
    }
}
