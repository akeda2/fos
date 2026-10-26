//! Ogg Opus (RFC 7845): the Ogg pages and packets are read here; the packets go to a `PacketDecoder` (the tape gives
//! it libopus, the tests a stand-in), so this crate stays plain Rust.
//!
//! An Ogg page is `"OggS"`, version 0, a type byte (bit 0: its first packet continues from the page before), the
//! 64-bit granule position (for Opus: 48 kHz samples decoded once the last packet that ends on the page has played,
//! counting the pre-skip; all ones when no packet ends there), a stream serial, a sequence number, a CRC (not
//! checked: the file is read over TCP or from a disk, both checked), a segment count and one lacing byte per segment.
//! A packet is a run of 255-byte segments closed by a shorter one. The first packet is `OpusHead` (channels, pre-skip),
//! the second `OpusTags` (Vorbis comments), the rest audio, each a 2.5 to 120 ms frame set whose length is in its
//! first byte. Output is always 48 kHz. The first `pre-skip` samples are decoder warm-up and are dropped, and the
//! last page's granule says where the audio really ends.
//!
//! Seeking has no index: bisect on file offsets for a page with a granule below the target minus 80 ms, then decode
//! packets (dropping the output) so the codec state has converged when the wanted packet comes.
//! Not handled: chained streams, several logical streams, more than two channels, the header's output gain.
use crate::tags::Tags;
use crate::{Decode, Error, Source};

/// A codec for Opus packets.
pub trait PacketDecoder {
    /// Prepares for 1 or 2 channels; false when it cannot.
    fn start(&mut self, channels: u32) -> bool;
    /// Decodes one packet into interleaved `f32` (room for `max_frames` frames of the stream's channels); returns
    /// the frames decoded per channel, `None` when the packet is bad.
    fn decode(&mut self, packet: &[u8], out: &mut [f32], max_frames: usize) -> Option<usize>;
    /// Forgets all history (before decoding from somewhere else).
    fn reset(&mut self);
}

/// The longest packet a decode block holds: 120 ms.
const MAX_FRAMES: usize = 5760;
const PACKET: usize = 8192;
const CACHE: usize = 32 * 1024;
/// What a decoder needs to converge after a reset: 80 ms at 48 kHz.
const PREROLL: u64 = 3840;

/// Samples per channel a packet decodes to (RFC 6716 section 3.1), `None` for a malformed one.
pub fn packet_samples(packet: &[u8]) -> Option<usize> {
    let toc = *packet.first()?;
    let config = usize::from(toc >> 3);
    let frame = match config {
        0..=11 => [480, 960, 1920, 2880][config % 4],
        12..=15 => [480, 960][config % 2],
        _ => [120, 240, 480, 960][config % 4],
    };
    let frames = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => usize::from(*packet.get(1)? & 0x3f),
    };
    (frames != 0).then_some(frame * frames)
}

#[derive(Clone, Copy)]
struct Page {
    header_type: u8,
    granule: u64,
    serial: u32,
    segments: usize,
    lacing: [u8; 255],
    body: u64,
    end: u64,
}

const NO_PAGE: Page = Page { header_type: 0, granule: 0, serial: 0, segments: 0, lacing: [0; 255], body: 0, end: 0 };

/// Where the next packet comes from: a position in the current page, and where the next page starts.
#[derive(Clone, Copy)]
struct Reader {
    page: Page,
    segment: usize,
    at: u64,
    next_page: u64,
}

pub struct Decoder<S: Source, P: PacketDecoder> {
    source: S,
    codec: P,
    channels: u32,
    pre_skip: u64,
    serial: u32,
    audio_start: u64,
    total: u64,
    reader: Reader,
    /// Samples (incl. pre-skip) before the packet the reader is at.
    cur: u64,
    /// Samples handed out (after the pre-skip).
    pos: u64,
    packet: [u8; PACKET],
    truncated: bool,
    /// The current block: its frames in `pcm` from `skip`, `len` of them.
    skip: usize,
    len: usize,
    pcm: [f32; 2 * MAX_FRAMES],
    tags: Tags,
    cache: [u8; CACHE],
    cache_at: u64,
    cache_len: usize,
}

impl<S: Source, P: PacketDecoder> Decoder<S, P> {
    pub const fn new(source: S, codec: P) -> Self {
        Decoder {
            source,
            codec,
            channels: 0,
            pre_skip: 0,
            serial: 0,
            audio_start: 0,
            total: 0,
            reader: Reader { page: NO_PAGE, segment: 0, at: 0, next_page: 0 },
            cur: 0,
            pos: 0,
            packet: [0; PACKET],
            truncated: false,
            skip: 0,
            len: 0,
            pcm: [0.0; 2 * MAX_FRAMES],
            tags: Tags::new(),
            cache: [0; CACHE],
            cache_at: 0,
            cache_len: 0,
        }
    }

    pub fn source(&mut self) -> &mut S {
        &mut self.source
    }

    /// Reads through a window of the file, so the many small reads of lacing and packets cost few source reads.
    fn read(&mut self, offset: u64, out: &mut [u8]) -> Result<usize, Error> {
        let mut done = 0;
        while done < out.len() {
            let at = offset + done as u64;
            if at < self.cache_at || at >= self.cache_at + self.cache_len as u64 {
                let n = self.source.read_at(at, &mut self.cache)?;
                self.cache_at = at;
                self.cache_len = n;
                if n == 0 {
                    break;
                }
            }
            let from = (at - self.cache_at) as usize;
            let n = (self.cache_len - from).min(out.len() - done);
            out[done..done + n].copy_from_slice(&self.cache[from..from + n]);
            done += n;
        }
        Ok(done)
    }

    /// The page starting at `offset`; `None` when there is none (the end of the file, or no `OggS` there).
    fn parse_page(&mut self, offset: u64) -> Result<Option<Page>, Error> {
        let mut head = [0u8; 27];
        if self.read(offset, &mut head)? < 27 || &head[..4] != b"OggS" || head[4] != 0 {
            return Ok(None);
        }
        let segments = head[26] as usize;
        let mut page = Page {
            header_type: head[5],
            granule: u64::from_le_bytes(head[6..14].try_into().unwrap()),
            serial: u32::from_le_bytes(head[14..18].try_into().unwrap()),
            segments,
            lacing: [0; 255],
            body: offset + 27 + segments as u64,
            end: 0,
        };
        if self.read(offset + 27, &mut page.lacing[..segments])? < segments {
            return Ok(None);
        }
        page.end = page.body + page.lacing[..segments].iter().map(|&l| l as u64).sum::<u64>();
        Ok(Some(page))
    }

    /// The first page of this stream that starts at or after `from` and before `limit`.
    fn find_page(&mut self, mut from: u64, limit: u64) -> Result<Option<(u64, Page)>, Error> {
        let limit = limit.min(self.source.size());
        let mut buf = [0u8; 2048];
        while from + 27 <= limit {
            let n = self.read(from, &mut buf)?;
            if n < 27 {
                return Ok(None);
            }
            let mut i = 0;
            while i + 27 <= n {
                if &buf[i..i + 4] == b"OggS" && buf[i + 4] == 0 && from + (i as u64) < limit {
                    if let Some(page) = self.parse_page(from + i as u64)? {
                        if page.serial == self.serial {
                            return Ok(Some((from + i as u64, page)));
                        }
                    }
                }
                i += 1;
            }
            from += (n - 26) as u64;
        }
        Ok(None)
    }

    /// Points the reader at the page starting at `offset`.
    fn rewind_to(&mut self, offset: u64) {
        self.reader = Reader { page: NO_PAGE, segment: 0, at: 0, next_page: offset };
    }

    /// Loads the next page of the stream; false at the end of the file.
    fn load_page(&mut self) -> Result<bool, Error> {
        loop {
            let Some(page) = self.parse_page(self.reader.next_page)? else { return Ok(false) };
            self.reader.next_page = page.end;
            if page.serial != self.serial && self.serial != 0 {
                continue;
            }
            self.reader.page = page;
            self.reader.segment = 0;
            self.reader.at = page.body;
            return Ok(true);
        }
    }

    /// Reads the next packet into `self.packet` and returns its length (`None` at the end of the stream). A packet
    /// longer than the buffer is cut and `truncated` says so.
    fn read_packet(&mut self) -> Result<Option<usize>, Error> {
        let mut len = 0;
        let mut started = false;
        self.truncated = false;
        loop {
            if self.reader.segment >= self.reader.page.segments {
                if !self.load_page()? {
                    return Ok(None);
                }
                if started && self.reader.page.header_type & 1 == 0 {
                    // The rest of the packet is missing; start over with what this page begins.
                    len = 0;
                    started = false;
                    self.truncated = false;
                }
                continue;
            }
            let lace = self.reader.page.lacing[self.reader.segment] as usize;
            let room = PACKET - len;
            let take = lace.min(room);
            let at = self.reader.at;
            let mut piece = [0u8; 255];
            if self.read(at, &mut piece[..take])? < take {
                return Ok(None);
            }
            self.packet[len..len + take].copy_from_slice(&piece[..take]);
            len += take;
            self.truncated |= take < lace;
            self.reader.at += lace as u64;
            self.reader.segment += 1;
            started = true;
            if lace < 255 {
                return Ok(Some(len));
            }
        }
    }

    pub fn open(&mut self) -> Result<(), Error> {
        self.tags.clear();
        self.serial = 0;
        self.cache_len = 0;
        self.rewind_to(0);
        let Some(len) = self.read_packet()? else { return Err(Error::Format("not an Ogg file")) };
        if len < 19 || &self.packet[..8] != b"OpusHead" {
            return Err(Error::Format("not an Ogg Opus file (no OpusHead)"));
        }
        if self.packet[8] >> 4 != 0 {
            return Err(Error::Unsupported("this OpusHead version"));
        }
        self.serial = self.reader.page.serial;
        self.channels = u32::from(self.packet[9]);
        self.pre_skip = u64::from(u16::from_le_bytes([self.packet[10], self.packet[11]]));
        if self.packet[18] != 0 || !(1..=2).contains(&self.channels) {
            return Err(Error::Unsupported("Opus with more than two channels"));
        }
        if !self.codec.start(self.channels) {
            return Err(Error::Unsupported("the Opus decoder could not start"));
        }
        let Some(len) = self.read_packet()? else { return Err(Error::Format("the Ogg Opus file has no OpusTags")) };
        if &self.packet[..len.min(8)] != b"OpusTags" {
            return Err(Error::Format("the second Ogg Opus packet is not OpusTags"));
        }
        self.parse_tags(len);
        self.audio_start = self.reader.next_page;
        self.total = self.find_total()?;
        self.rewind_to(self.audio_start);
        self.cur = 0;
        self.pos = 0;
        self.len = 0;
        Ok(())
    }

    /// `OpusTags`: vendor string, then comments, each a 32-bit length and `NAME=value`.
    fn parse_tags(&mut self, len: usize) {
        let data = &self.packet[..len];
        let word = |at: usize| data.get(at..at + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize);
        let Some(vendor) = word(8) else { return };
        let mut at = 12 + vendor;
        let Some(count) = word(at) else { return };
        at += 4;
        for _ in 0..count {
            let Some(n) = word(at) else { return };
            let Some(comment) = data.get(at + 4..at + 4 + n) else { return };
            self.tags.set_vorbis(comment);
            at += 4 + n;
        }
    }

    /// The audio length: the last page's granule less the pre-skip, found by walking the pages of the file's tail.
    fn find_total(&mut self) -> Result<u64, Error> {
        let size = self.source.size();
        let mut from = size.saturating_sub(70_000).max(self.audio_start);
        let mut last = 0;
        while let Some((_, page)) = self.find_page(from, size)? {
            if page.granule != u64::MAX {
                last = page.granule;
            }
            from = page.end;
        }
        Ok(last.saturating_sub(self.pre_skip))
    }

    pub fn next_block(&mut self) -> Result<Option<usize>, Error> {
        loop {
            if self.total > 0 && self.pos >= self.total {
                return Ok(None);
            }
            let Some(len) = self.read_packet()? else { return Ok(None) };
            if self.truncated {
                return Err(Error::Format("an Opus packet is larger than expected"));
            }
            let channels = self.channels as usize;
            let frames = self.codec.decode(&self.packet[..len], &mut self.pcm[..channels * MAX_FRAMES], MAX_FRAMES);
            let Some(frames) = frames else { return Err(Error::Format("a corrupt Opus packet")) };
            let skip = (self.pre_skip.saturating_sub(self.cur) as usize).min(frames);
            self.cur += frames as u64;
            let mut out = frames - skip;
            if self.total > 0 {
                out = out.min((self.total - self.pos) as usize);
            }
            self.skip = skip;
            self.len = out;
            self.pos += out as u64;
            if out > 0 {
                return Ok(Some(out));
            }
        }
    }

    pub fn seek(&mut self, sample: u64) -> Result<u64, Error> {
        let target = sample + self.pre_skip;
        let want = target.saturating_sub(PREROLL);
        let (mut lo, mut granule, mut hi) = (self.audio_start, 0u64, self.source.size());
        while lo < hi && hi - lo > 8192 {
            let mid = lo + (hi - lo) / 2;
            match self.find_page(mid, hi)? {
                Some((_, page)) if page.granule == u64::MAX || page.granule <= want => {
                    lo = page.end;
                    if page.granule != u64::MAX {
                        granule = page.granule;
                    }
                }
                Some((offset, _)) => hi = offset,
                None => hi = mid,
            }
        }
        // Start on a page that begins a packet; the granule of the page before it is then exact.
        let mut offset = lo;
        while let Some(page) = self.parse_page(offset)? {
            if page.header_type & 1 == 0 {
                break;
            }
            if page.granule != u64::MAX {
                granule = page.granule;
            }
            offset = page.end;
        }
        self.rewind_to(offset);
        self.cur = granule;
        self.codec.reset();
        self.len = 0;
        loop {
            let saved = (self.reader, self.cur);
            let Some(len) = self.read_packet()? else { break };
            let Some(frames) = packet_samples(&self.packet[..len]) else { return Err(Error::Format("a corrupt Opus packet")) };
            if self.cur + frames as u64 > target {
                (self.reader, self.cur) = saved;
                break;
            }
            // Only the state matters; the samples are dropped.
            let channels = self.channels as usize;
            if self.codec.decode(&self.packet[..len], &mut self.pcm[..channels * MAX_FRAMES], MAX_FRAMES).is_none() {
                return Err(Error::Format("a corrupt Opus packet"));
            }
            self.cur += frames as u64;
        }
        self.pos = self.cur.saturating_sub(self.pre_skip);
        Ok(self.pos)
    }
}

impl<S: Source, P: PacketDecoder> Decode for Decoder<S, P> {
    fn open(&mut self) -> Result<(), Error> {
        Decoder::open(self)
    }

    fn next_block(&mut self) -> Result<Option<usize>, Error> {
        Decoder::next_block(self)
    }

    fn to_f32_stereo(&self, start: usize, count: usize, out: &mut [f32]) {
        let first = self.skip + start;
        for i in 0..count {
            if self.channels == 2 {
                out[2 * i] = self.pcm[2 * (first + i)];
                out[2 * i + 1] = self.pcm[2 * (first + i) + 1];
            } else {
                out[2 * i] = self.pcm[first + i];
                out[2 * i + 1] = self.pcm[first + i];
            }
        }
    }

    fn sample_rate(&self) -> u32 {
        48_000
    }

    fn channels(&self) -> u32 {
        self.channels
    }

    fn bits_per_sample(&self) -> u32 {
        0
    }

    fn total_samples(&self) -> u64 {
        self.total
    }

    fn tags(&self) -> &Tags {
        &self.tags
    }

    fn position(&self) -> u64 {
        self.pos
    }

    fn seek(&mut self, sample: u64) -> Result<u64, Error> {
        Decoder::seek(self, sample)
    }

    fn format_name(&self) -> &'static str {
        "Opus"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decodes packet `[toc, lo, hi]` to 960 frames of the value `lo + 256 * hi`.
    struct Stand {
        resets: u32,
    }

    impl PacketDecoder for Stand {
        fn start(&mut self, _: u32) -> bool {
            true
        }
        fn decode(&mut self, packet: &[u8], out: &mut [f32], max: usize) -> Option<usize> {
            let n = packet_samples(packet)?;
            assert!(n <= max);
            let value = f32::from(packet[1]) + 256.0 * f32::from(packet[2]);
            out[..n * 2].fill(value);
            Some(n)
        }
        fn reset(&mut self) {
            self.resets += 1;
        }
    }

    fn page(kind: u8, granule: u64, packet: &[u8], out: &mut Vec<u8>) {
        let segments = packet.len() / 255 + 1;
        out.extend_from_slice(b"OggS");
        out.extend_from_slice(&[0, kind]);
        out.extend_from_slice(&granule.to_le_bytes());
        out.extend_from_slice(&[7, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        out.push(segments as u8);
        out.extend(std::iter::repeat(255).take(segments - 1));
        out.push((packet.len() % 255) as u8);
        out.extend_from_slice(packet);
    }

    const PRE_SKIP: u64 = 312;

    /// `packets` 20 ms packets, the last one cut 100 samples short; two channels (1 for `mono`).
    fn file(packets: u16, mono: bool) -> Vec<u8> {
        let mut out = Vec::new();
        let mut head = b"OpusHead".to_vec();
        head.extend_from_slice(&[1, if mono { 1 } else { 2 }]);
        head.extend_from_slice(&(PRE_SKIP as u16).to_le_bytes());
        head.extend_from_slice(&[0x80, 0xbb, 0, 0, 0, 0, 0]);
        page(2, 0, &head, &mut out);
        let mut tags = b"OpusTags".to_vec();
        tags.extend_from_slice(&5u32.to_le_bytes());
        tags.extend_from_slice(b"stand");
        tags.extend_from_slice(&2u32.to_le_bytes());
        for comment in [&b"TITLE=Opus Song"[..], b"artist=Bj\xc3\xb6rk"] {
            tags.extend_from_slice(&(comment.len() as u32).to_le_bytes());
            tags.extend_from_slice(comment);
        }
        page(0, 0, &tags, &mut out);
        for k in 0..packets {
            let mut granule = 960 * (u64::from(k) + 1);
            if k + 1 == packets {
                granule -= 100;
            }
            // CELT 20 ms, one frame; some padded to several lacing segments
            let mut packet = vec![19 << 3, k as u8, (k >> 8) as u8];
            if k % 7 == 3 {
                packet.resize(600, 0);
            }
            page(if k + 1 == packets { 4 } else { 0 }, granule, &packet, &mut out);
        }
        out
    }

    fn open(bytes: &[u8]) -> Decoder<&[u8], Stand> {
        let mut decoder = Decoder::new(bytes, Stand { resets: 0 });
        decoder.open().unwrap();
        decoder
    }

    #[test]
    fn plays_with_pre_skip_and_end_trim() {
        let bytes = file(40, false);
        let mut d = open(&bytes);
        assert_eq!((d.channels(), d.sample_rate(), d.total_samples()), (2, 48000, 40 * 960 - 100 - 312));
        assert_eq!((d.tags().title.as_str(), d.tags().artist.as_str()), ("Opus Song", "Bj\u{f6}rk"));
        assert_eq!(d.next_block().unwrap(), Some(960 - 312));
        let mut out = [0.0f32; 2];
        d.to_f32_stereo(0, 1, &mut out);
        assert_eq!(out, [0.0, 0.0]);
        let mut all = 960 - 312;
        let mut blocks = 1;
        while let Some(n) = d.next_block().unwrap() {
            all += n;
            blocks += 1;
        }
        assert_eq!((all as u64, blocks, d.position()), (d.total_samples(), 40, d.total_samples()));
    }

    #[test]
    fn seeks_to_a_packet() {
        let bytes = file(60, false);
        let mut d = open(&bytes);
        let at = 960 * 30 + 5;
        let landed = d.seek(at).unwrap();
        // Packet 30 starts at 960 * 30 once the pre-skip is dropped from the front.
        assert_eq!(landed, 960 * 30 - 312);
        assert!(landed <= at && at - landed < 960, "{landed}");
        assert_eq!(d.position(), landed);
        let n = d.next_block().unwrap().unwrap();
        assert_eq!(n, 960);
        let mut out = [0.0f32; 2];
        d.to_f32_stereo(0, 1, &mut out);
        assert_eq!(out, [30.0, 30.0]);
        assert!(d.codec.resets >= 1);
        // The start, and then the very end.
        assert_eq!(d.seek(0).unwrap(), 0);
        assert_eq!(d.next_block().unwrap(), Some(960 - 312));
        d.seek(d.total_samples() + 1000).unwrap();
        assert_eq!(d.next_block().unwrap(), None);
    }

    #[test]
    fn seeks_by_bisection_in_a_longer_file() {
        let bytes = file(400, false);
        assert!(bytes.len() > 40_000);
        let mut d = open(&bytes);
        for k in [399u64, 1, 250, 123, 0, 398] {
            let at = if k == 0 { 0 } else { 960 * k - 312 + 17 };
            let landed = d.seek(at).unwrap();
            assert!(landed <= at && at - landed < 960, "{k}: {landed}");
            let n = d.next_block().unwrap().unwrap();
            assert!(n > 0);
            let mut out = [0.0f32; 2];
            d.to_f32_stereo(0, 1, &mut out);
            assert_eq!(out[0], k as f32, "{k}");
        }
    }

    #[test]
    fn mono_is_duplicated() {
        let bytes = file(3, true);
        let mut d = open(&bytes);
        d.next_block().unwrap();
        d.next_block().unwrap();
        let mut out = [0.0f32; 4];
        d.to_f32_stereo(0, 2, &mut out);
        assert_eq!(out, [1.0; 4]);
    }

    #[test]
    fn rejects_other_files() {
        let mut d = Decoder::new(&b"RIFF....WAVE........................"[..], Stand { resets: 0 });
        assert!(d.open().is_err());
    }
}
