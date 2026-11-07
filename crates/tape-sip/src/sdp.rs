//! Reading the SDP of one AES67 audio stream (RFC 4566, RFC 3190 `L16`/`L24`, RFC 7273 clocking).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefClock {
    /// No `ts-refclk` line.
    None,
    /// `ptp=IEEE1588-2008:<grandmaster>:<domain>`: the RTP timestamps are on this grandmaster's timeline.
    Ptp { grandmaster: [u8; 8], domain: u8 },
    /// `ptp=IEEE1588-2008:traceable`: some PTP grandmaster, not named.
    PtpTraceable,
    /// `localmac=`: a free-running sender clock (what a sender without PTP says).
    LocalMac,
    /// `ntp=`, `gps=`, ...
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    SendRecv,
    SendOnly,
    RecvOnly,
    Inactive,
}

/// The audio stream an SDP describes (the first `m=audio` with a payload format that is supported).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Media {
    pub address: [u8; 4],
    pub port: u16,
    pub payload_type: u8,
    pub bits: u8,
    pub channels: u8,
    pub rate: u32,
    /// Microseconds of audio per packet (`a=ptime`; 1000 when absent).
    pub packet_us: u32,
    pub clock: RefClock,
    pub direction: Direction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SdpError {
    /// No `m=audio` line.
    NoAudio,
    NoAddress,
    /// An audio stream, but not one this reader handles (a sentence for a `Warning:` header).
    Unsupported(&'static str),
}

#[derive(Clone, Copy, Default)]
struct Attrs {
    clock: Option<RefClock>,
    packet_us: Option<u32>,
    direction: Option<Direction>,
}

const MAX_FORMATS: usize = 8;

pub fn parse_ipv4(text: &str) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut parts = text.split('.');
    for byte in out.iter_mut() {
        *byte = parts.next()?.parse().ok()?;
    }
    parts.next().is_none().then_some(out)
}

/// `0.125`, `1`, `0.333`: milliseconds with an optional fraction, as microseconds.
fn parse_ptime(text: &str) -> Option<u32> {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    let mut micros = whole.parse::<u32>().ok()?.checked_mul(1000)?;
    let mut scale = 100;
    for digit in fraction.bytes().take(3) {
        if !digit.is_ascii_digit() {
            return None;
        }
        micros += u32::from(digit - b'0') * scale;
        scale /= 10;
    }
    Some(micros)
}

fn parse_clock(value: &str) -> RefClock {
    let mut parts = value.split(':');
    let Some(kind) = parts.next() else { return RefClock::Other };
    if kind.starts_with("ptp=IEEE1588") {
        let Some(identity) = parts.next() else { return RefClock::Other };
        if identity == "traceable" {
            return RefClock::PtpTraceable;
        }
        let mut grandmaster = [0u8; 8];
        let mut octets = identity.split('-');
        for byte in grandmaster.iter_mut() {
            match octets.next().and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
                Some(value) => *byte = value,
                None => return RefClock::Other,
            }
        }
        return RefClock::Ptp { grandmaster, domain: parts.next().and_then(|d| d.parse().ok()).unwrap_or(0) };
    }
    if kind.starts_with("localmac=") { RefClock::LocalMac } else { RefClock::Other }
}

/// `L24/48000/8` (channels default to 1), also `L16`. Anything else is not linear PCM.
fn parse_rtpmap(value: &str) -> Option<(u8, u32, u8)> {
    let mut parts = value.split('/');
    let bits = match parts.next()? {
        "L16" => 16,
        "L24" => 24,
        _ => return None,
    };
    let rate = parts.next()?.parse().ok()?;
    let channels = match parts.next() {
        Some(count) => count.parse().ok()?,
        None => 1,
    };
    Some((bits, rate, channels))
}

fn parse_attribute(attrs: &mut Attrs, line: &str, formats: &mut [(u8, u8, u32, u8); MAX_FORMATS], count: &mut usize, in_media: bool) {
    let (name, value) = line.split_once(':').unwrap_or((line, ""));
    match name {
        "ts-refclk" => attrs.clock = Some(parse_clock(value)),
        "ptime" => attrs.packet_us = parse_ptime(value.trim()),
        "sendrecv" => attrs.direction = Some(Direction::SendRecv),
        "sendonly" => attrs.direction = Some(Direction::SendOnly),
        "recvonly" => attrs.direction = Some(Direction::RecvOnly),
        "inactive" => attrs.direction = Some(Direction::Inactive),
        "rtpmap" if in_media => {
            if let Some((payload, map)) = value.split_once(' ') {
                if let (Ok(payload), Some((bits, rate, channels))) = (payload.parse::<u8>(), parse_rtpmap(map.trim())) {
                    if *count < MAX_FORMATS {
                        formats[*count] = (payload, bits, rate, channels);
                        *count += 1;
                    }
                }
            }
        }
        _ => {}
    }
}

/// The first audio stream of `text` that is 16 or 24-bit linear PCM over RTP/AVP, with its address, port and clock.
/// Session-level attributes (`ts-refclk`, `ptime`, direction) apply when the media section has none of its own.
pub fn parse(text: &str) -> Result<Media, SdpError> {
    let mut session = Attrs::default();
    let mut media = Attrs::default();
    let mut session_address = None;
    let mut media_address = None;
    let mut offered = [0u8; MAX_FORMATS];
    let mut offered_count = 0;
    let mut port = 0u16;
    let mut formats = [(0u8, 0u8, 0u32, 0u8); MAX_FORMATS];
    let mut format_count = 0;
    // 0 = before the first m=, 1 = inside the audio section, 2 = after it (other sections are skipped)
    let mut stage = 0;
    for line in text.split('\n') {
        let line = line.trim_end_matches('\r');
        let Some((kind, value)) = line.split_once('=') else { continue };
        match kind {
            "m" => {
                if stage == 1 {
                    stage = 2;
                }
                let mut words = value.split_whitespace();
                if stage == 0 && words.next() == Some("audio") {
                    port = words.next().and_then(|p| p.split('/').next()).and_then(|p| p.parse().ok()).unwrap_or(0);
                    if words.next() != Some("RTP/AVP") {
                        return Err(SdpError::Unsupported("only RTP/AVP is supported"));
                    }
                    for payload in words {
                        if let (Ok(payload), true) = (payload.parse::<u8>(), offered_count < MAX_FORMATS) {
                            offered[offered_count] = payload;
                            offered_count += 1;
                        }
                    }
                    stage = 1;
                }
            }
            "c" if stage < 2 => {
                let mut words = value.split_whitespace();
                if words.next() == Some("IN") && words.next() == Some("IP4") {
                    let address = words.next().and_then(|a| parse_ipv4(a.split('/').next().unwrap_or(a)));
                    if stage == 1 { media_address = address } else { session_address = address }
                }
            }
            "a" if stage == 0 => parse_attribute(&mut session, value, &mut formats, &mut format_count, false),
            "a" if stage == 1 => parse_attribute(&mut media, value, &mut formats, &mut format_count, true),
            _ => {}
        }
    }
    if offered_count == 0 {
        return Err(SdpError::NoAudio);
    }
    let address = media_address.or(session_address).ok_or(SdpError::NoAddress)?;
    // the first format in m= order that this reader understands; 10 and 11 are RFC 3551's static stereo/mono L16 at 44.1 kHz
    let chosen = offered[..offered_count].iter().find_map(|&payload| {
        formats[..format_count].iter().find(|f| f.0 == payload).map(|f| (payload, f.1, f.2, f.3)).or(match payload {
            10 => Some((10, 16, 44100, 2)),
            11 => Some((11, 16, 44100, 1)),
            _ => None,
        })
    });
    let Some((payload_type, bits, rate, channels)) = chosen else { return Err(SdpError::Unsupported("no L16 or L24 payload offered")) };
    Ok(Media {
        address,
        port,
        payload_type,
        bits,
        channels,
        rate,
        packet_us: media.packet_us.or(session.packet_us).unwrap_or(1000),
        clock: media.clock.or(session.clock).unwrap_or(RefClock::None),
        direction: media.direction.or(session.direction).unwrap_or(Direction::SendRecv),
    })
}

/// What a receiver of this stream needs of its own clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The stream is not on a PTP timeline (no `ts-refclk:ptp`).
    NotPtp,
    /// This machine is not locked to a PTP grandmaster.
    NotLocked,
    /// Locked to a different grandmaster (or domain) than the stream's.
    OtherGrandmaster,
    /// Packet time, rate or channel count outside what this machine will take.
    Format,
}

impl Refusal {
    pub fn text(self) -> &'static str {
        match self {
            Refusal::NotPtp => "stream is not on a PTP clock",
            Refusal::NotLocked => "this machine is not locked to PTP",
            Refusal::OtherGrandmaster => "stream uses another PTP grandmaster",
            Refusal::Format => "audio format not supported",
        }
    }
}

/// AES67 packet times in microseconds (125, 250, 333, 1000 and 4000; 333 is 1/3 ms rounded), accepted within a few percent.
pub fn packet_time_ok(packet_us: u32) -> bool {
    [125, 250, 333, 1000, 4000].iter().any(|&t| packet_us.abs_diff(t) <= t / 20 + 1)
}

/// Whether `media` can be taken with the local clock `locked_to` (the grandmaster this machine follows, if any).
/// With `require_ptp` the stream must name a PTP clock and it must be the one this machine follows; without, a stream
/// on any clock is accepted and the receiver adapts (the audio-net rate estimator).
pub fn check(media: &Media, require_ptp: bool, locked_to: Option<[u8; 8]>, max_channels: u8) -> Result<(), Refusal> {
    if !matches!(media.rate, 44100 | 48000 | 96000) || media.channels == 0 || media.channels > max_channels || !packet_time_ok(media.packet_us) {
        return Err(Refusal::Format);
    }
    if !require_ptp {
        return Ok(());
    }
    match (media.clock, locked_to) {
        (RefClock::Ptp { .. } | RefClock::PtpTraceable, None) => Err(Refusal::NotLocked),
        (RefClock::Ptp { grandmaster, .. }, Some(local)) if grandmaster != local => Err(Refusal::OtherGrandmaster),
        (RefClock::Ptp { .. } | RefClock::PtpTraceable, Some(_)) => Ok(()),
        _ => Err(Refusal::NotPtp),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AES67: &str = "v=0\r\no=- 1311738121 1311738121 IN IP4 192.168.1.10\r\ns=Stage box\r\nc=IN IP4 239.69.1.2/32\r\nt=0 0\r\n\
        m=audio 5004 RTP/AVP 98\r\na=rtpmap:98 L24/48000/8\r\na=ptime:0.125\r\na=ts-refclk:ptp=IEEE1588-2008:00-1B-21-FF-FE-AA-BB-CC:0\r\na=mediaclk:direct=0\r\na=sendonly\r\n";

    #[test]
    fn reads_an_aes67_description() {
        let media = parse(AES67).unwrap();
        assert_eq!(media.address, [239, 69, 1, 2]);
        assert_eq!((media.port, media.payload_type, media.bits, media.channels, media.rate), (5004, 98, 24, 8, 48000));
        assert_eq!(media.packet_us, 125);
        assert_eq!(media.clock, RefClock::Ptp { grandmaster: [0x00, 0x1b, 0x21, 0xff, 0xfe, 0xaa, 0xbb, 0xcc], domain: 0 });
        assert_eq!(media.direction, Direction::SendOnly);
    }

    #[test]
    fn session_level_clock_and_static_payload() {
        let media = parse("v=0\r\nc=IN IP4 10.0.0.5\r\na=ts-refclk:ptp=IEEE1588-2008:traceable\r\nm=audio 4000 RTP/AVP 10\r\n").unwrap();
        assert_eq!((media.payload_type, media.bits, media.channels, media.rate, media.packet_us), (10, 16, 2, 44100, 1000));
        assert_eq!(media.clock, RefClock::PtpTraceable);
    }

    #[test]
    fn skips_other_media_and_unknown_codecs() {
        let sdp = "v=0\r\nc=IN IP4 10.0.0.5\r\nm=video 5000 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\nm=audio 5004 RTP/AVP 0 96 97\r\na=rtpmap:96 opus/48000/2\r\na=rtpmap:97 L16/48000/2\r\n";
        let media = parse(sdp).unwrap();
        assert_eq!((media.port, media.payload_type), (5004, 97));
        assert!(matches!(parse("v=0\r\nc=IN IP4 1.2.3.4\r\nm=audio 5004 RTP/AVP 0\r\n"), Err(SdpError::Unsupported(_))));
        assert_eq!(parse("v=0\r\nc=IN IP4 1.2.3.4\r\n"), Err(SdpError::NoAudio));
        assert_eq!(parse("v=0\r\nm=audio 5004 RTP/AVP 10\r\n"), Err(SdpError::NoAddress));
    }

    #[test]
    fn ptp_requirement_is_optional() {
        let media = parse(AES67).unwrap();
        let gm = [0x00, 0x1b, 0x21, 0xff, 0xfe, 0xaa, 0xbb, 0xcc];
        assert_eq!(check(&media, true, Some(gm), 8), Ok(()));
        assert_eq!(check(&media, true, None, 8), Err(Refusal::NotLocked));
        assert_eq!(check(&media, true, Some([1; 8]), 8), Err(Refusal::OtherGrandmaster));
        assert_eq!(check(&media, false, None, 8), Ok(()));
        let free = Media { clock: RefClock::LocalMac, ..media };
        assert_eq!(check(&free, true, Some(gm), 8), Err(Refusal::NotPtp));
        assert_eq!(check(&free, false, None, 8), Ok(()));
        assert_eq!(check(&media, false, None, 2), Err(Refusal::Format));
        assert_eq!(check(&Media { packet_us: 20_000, ..media }, false, None, 8), Err(Refusal::Format));
    }

    #[test]
    fn ptime_fractions() {
        assert_eq!(parse_ptime("0.125"), Some(125));
        assert_eq!(parse_ptime("0.25"), Some(250));
        assert_eq!(parse_ptime("4"), Some(4000));
        assert_eq!(parse_ptime("x"), None);
    }
}
