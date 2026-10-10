//! SNTP (RFC 4330) client arithmetic: the 48-byte request and reply, the offset and delay from the four timestamps,
//! and Unix time to calendar date. No I/O here; `tape-ip` sends the datagrams.
//!
//! NTP counts seconds from 1900 with a 32-bit fraction; Unix time counts from 1970 (`UNIX_TO_NTP` apart). All the
//! functions here use Unix nanoseconds as `i64`, which holds until the year 2262.

pub const PORT: u16 = 123;
pub const PACKET_LEN: usize = 48;
/// Seconds from 1900-01-01 to 1970-01-01.
pub const UNIX_TO_NTP: u64 = 2_208_988_800;
/// TAI minus UTC since 2017-01-01 (PTP's timescale is TAI; NTP's is UTC).
pub const TAI_MINUS_UTC: i64 = 37;

/// A client request (version 4, mode 3). `cookie` goes in the transmit field and comes back as the reply's originate
/// field, which ties the reply to this request; it is any value, it need not be the time.
pub fn build_request(out: &mut [u8; PACKET_LEN], cookie: u64) {
    out.fill(0);
    out[0] = (4 << 3) | 3;
    out[40..48].copy_from_slice(&cookie.to_be_bytes());
}

/// What a valid server reply says: when it received our request (t1) and when it answered (t2), as Unix nanoseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reply {
    pub received: i64,
    pub transmitted: i64,
    pub stratum: u8,
}

fn unix_nanoseconds(bytes: &[u8]) -> Option<i64> {
    let seconds = u64::from(u32::from_be_bytes(bytes[0..4].try_into().ok()?));
    let fraction = u64::from(u32::from_be_bytes(bytes[4..8].try_into().ok()?));
    if seconds == 0 {
        return None;
    }
    // A 32-bit NTP era wraps in 2036; seconds below 2^31 are taken as era 1, as every SNTP client has to.
    let seconds = if seconds < 0x8000_0000 { seconds + (1 << 32) } else { seconds };
    let unix = i64::try_from(seconds).ok()? - UNIX_TO_NTP as i64;
    Some(unix * 1_000_000_000 + ((fraction * 1_000_000_000) >> 32) as i64)
}

/// Checks a datagram is a usable server reply to the request that carried `cookie`: server mode, not the
/// "alarm" leap state, stratum 1 to 15 (0 is a kiss-o'-death), our cookie echoed, and both server timestamps set.
pub fn parse_reply(bytes: &[u8], cookie: u64) -> Option<Reply> {
    if bytes.len() < PACKET_LEN {
        return None;
    }
    let (leap, version, mode, stratum) = (bytes[0] >> 6, (bytes[0] >> 3) & 7, bytes[0] & 7, bytes[1]);
    if mode != 4 || leap == 3 || !(3..=4).contains(&version) || !(1..=15).contains(&stratum) {
        return None;
    }
    if bytes[24..32] != cookie.to_be_bytes() {
        return None;
    }
    let received = unix_nanoseconds(&bytes[32..40])?;
    let transmitted = unix_nanoseconds(&bytes[40..48])?;
    (transmitted >= received).then_some(Reply { received, transmitted, stratum })
}

/// One measurement against a server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    /// Unix time minus the local clock, in nanoseconds: `unix_now = local_now + offset`.
    pub offset: i64,
    /// Round trip minus the server's own processing time.
    pub delay: i64,
}

/// `sent` and `arrived` are the local clock (any epoch) when the request left and the reply came in.
pub fn sample(reply: &Reply, sent: i64, arrived: i64) -> Sample {
    let offset = ((reply.received - sent) + (reply.transmitted - arrived)) / 2;
    let delay = (arrived - sent) - (reply.transmitted - reply.received);
    Sample { offset, delay }
}

/// A calendar date and time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Civil {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

/// Unix seconds (1970 on, no leap seconds) as a calendar date (Howard Hinnant's days-to-civil).
pub fn civil_from_unix(seconds: i64) -> Civil {
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    Civil { year: year as u16, month: month as u8, day: day as u8, hour: (rest / 3600) as u8, minute: (rest % 3600 / 60) as u8, second: (rest % 60) as u8 }
}

/// `+2`, `-5`, `+5:30` or `0` as minutes east of UTC.
pub fn parse_zone(text: &str) -> Option<i32> {
    let (sign, digits) = match text.as_bytes().first()? {
        b'-' => (-1, &text[1..]),
        b'+' => (1, &text[1..]),
        _ => (1, text),
    };
    let (hours, minutes) = digits.split_once(':').unwrap_or((digits, "0"));
    let (hours, minutes): (i32, i32) = (hours.parse().ok()?, minutes.parse().ok()?);
    (hours <= 14 && minutes < 60).then_some(sign * (hours * 60 + minutes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timestamp(unix_seconds: u64, fraction: u32) -> [u8; 8] {
        let mut out = [0u8; 8];
        out[..4].copy_from_slice(&((unix_seconds + UNIX_TO_NTP) as u32).to_be_bytes());
        out[4..].copy_from_slice(&fraction.to_be_bytes());
        out
    }

    fn reply(cookie: u64, received: [u8; 8], transmitted: [u8; 8]) -> [u8; PACKET_LEN] {
        let mut bytes = [0u8; PACKET_LEN];
        bytes[0] = (4 << 3) | 4;
        bytes[1] = 2;
        bytes[24..32].copy_from_slice(&cookie.to_be_bytes());
        bytes[32..40].copy_from_slice(&received);
        bytes[40..48].copy_from_slice(&transmitted);
        bytes
    }

    #[test]
    fn request_is_version_4_client_with_the_cookie() {
        let mut out = [0xffu8; PACKET_LEN];
        build_request(&mut out, 0x0102_0304_0506_0708);
        assert_eq!(out[0], 0x23);
        assert_eq!(&out[40..], &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(out[1..40].iter().all(|&b| b == 0));
    }

    #[test]
    fn reply_converts_the_epoch_and_the_fraction() {
        let bytes = reply(7, timestamp(1_791_000_000, 1 << 31), timestamp(1_791_000_000, 3 << 30));
        let parsed = parse_reply(&bytes, 7).unwrap();
        assert_eq!(parsed.received, 1_791_000_000_500_000_000);
        assert_eq!(parsed.transmitted, 1_791_000_000_750_000_000);
        assert_eq!(parsed.stratum, 2);
    }

    #[test]
    fn reply_refused_when_it_is_not_ours_or_not_usable() {
        let good = reply(7, timestamp(1_791_000_000, 0), timestamp(1_791_000_000, 0));
        assert!(parse_reply(&good, 8).is_none());
        let mut kiss = good;
        kiss[1] = 0;
        assert!(parse_reply(&kiss, 7).is_none());
        let mut alarm = good;
        alarm[0] |= 0xc0;
        assert!(parse_reply(&alarm, 7).is_none());
        let mut client = good;
        client[0] = (4 << 3) | 3;
        assert!(parse_reply(&client, 7).is_none());
        let mut unset = good;
        unset[40..48].fill(0);
        assert!(parse_reply(&unset, 7).is_none());
        assert!(parse_reply(&good[..47], 7).is_none());
    }

    #[test]
    fn offset_and_delay_from_four_timestamps() {
        // The local clock is 1000 s behind Unix time; 20 ms each way, the server takes 1 ms.
        let reply = Reply { received: 5_000_000_000_000 + 20_000_000, transmitted: 5_000_000_000_000 + 21_000_000, stratum: 2 };
        let s = sample(&reply, 5_000_000_000_000 - 1_000_000_000_000, 5_000_000_000_000 - 1_000_000_000_000 + 41_000_000);
        assert_eq!(s.delay, 40_000_000);
        assert_eq!(s.offset, 1_000_000_000_000);
    }

    #[test]
    fn calendar_dates() {
        assert_eq!(civil_from_unix(0), Civil { year: 1970, month: 1, day: 1, hour: 0, minute: 0, second: 0 });
        assert_eq!(civil_from_unix(951_782_400), Civil { year: 2000, month: 2, day: 29, hour: 0, minute: 0, second: 0 });
        assert_eq!(civil_from_unix(1_791_000_000 + 3), Civil { year: 2026, month: 10, day: 3, hour: 4, minute: 0, second: 3 });
        assert_eq!(civil_from_unix(1_709_164_799), Civil { year: 2024, month: 2, day: 28, hour: 23, minute: 59, second: 59 });
        assert_eq!(civil_from_unix(1_709_164_800), Civil { year: 2024, month: 2, day: 29, hour: 0, minute: 0, second: 0 });
    }

    #[test]
    fn zones() {
        assert_eq!(parse_zone("+2"), Some(120));
        assert_eq!(parse_zone("-5"), Some(-300));
        assert_eq!(parse_zone("0"), Some(0));
        assert_eq!(parse_zone("+5:30"), Some(330));
        assert_eq!(parse_zone("+15"), None);
        assert_eq!(parse_zone("x"), None);
    }
}
