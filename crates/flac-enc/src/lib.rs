//! A small FLAC encoder for archiving: the caller hands it blocks of integer samples and gets finished frames back.
//! `no_std`, no allocation: the scratch lives in `Encoder` (about 100 KiB; keep it in a `static` on a tape) and the
//! caller supplies the output buffer.
//!
//! What it does, in the order a frame is built:
//! * **Wasted bits.** If every sample of a channel has its low `w` bits clear, they are dropped and the subframe header
//!   says so. This is how a 24-bit container holding 18-bit audio (see `depth`) costs what an 18-bit file would.
//! * **Subframes.** Constant, verbatim, a fixed polynomial predictor of order 0-4, or linear prediction (LPC,
//!   order 1-12, `lpc`) with Rice-coded residuals in partitions. The best fixed order is picked by the smallest sum of
//!   residual magnitudes and the best LPC order by an estimate of the coded size; the partition order and Rice
//!   parameters, and the choice between the two, by exact bit counts. The LPC analysis is floating point (hard-float
//!   tapes only), the coefficients written are quantised integers, so decoders reproduce the samples exactly.
//! * **Stereo.** For two channels the four assignments (left/right, left/side, side/right, mid/side) are all
//!   costed and the cheapest is written. The side channel needs one more bit than the sample.
//! * **Multichannel** (up to 8): independent channels in the order the caller gives. FLAC's own order is L R C LFE Ls Rs
//!   for 5.1 and L R Ls Rs for 4.0.
//!
//! The STREAMINFO MD5 signature (`md5`) covers the samples as given (interleaved, little-endian, whole bytes per
//! sample), as FLAC defines it. It, the total sample count and the frame sizes are patched into the header with
//! `Encoder::patch_header`, after the last frame or at a checkpoint: the signature then is that of the audio so far.
#![cfg_attr(not(test), no_std)]

pub mod depth;
mod lpc;
pub mod md5;

use md5::Md5;

/// Samples per channel in a full block. Fixed so the scratch is a plain array.
pub const BLOCK: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Channels not 1..=8, bits not 4..=24 or a sample rate that does not fit STREAMINFO's 20 bits.
    BadFormat,
    /// Wrong channel count, unequal or empty or oversized blocks.
    BadBlock,
    /// A sample does not fit in the declared bit depth.
    Range,
    /// The output buffer is smaller than `max_frame_bytes`.
    OutputTooSmall,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Info {
    pub sample_rate: u32,
    pub channels: u32,
    pub bits: u32,
}

impl Info {
    fn check(&self) -> Result<(), Error> {
        if self.channels == 0 || self.channels > 8 || self.bits < 4 || self.bits > 24 || self.sample_rate == 0 || self.sample_rate > 0xF_FFFF {
            return Err(Error::BadFormat);
        }
        Ok(())
    }
}

/// An upper bound for one encoded frame, so the caller can size its buffer.
pub const fn max_frame_bytes(channels: usize) -> usize {
    32 + channels * (BLOCK * 4 + 8)
}

/// Bytes `write_header` can produce at most for `tags` totalling `tag_bytes` (keys, values, `=` and length fields).
pub const fn header_bytes(tag_bytes: usize) -> usize {
    4 + 4 + 34 + 4 + 4 + 16 + 4 + tag_bytes
}

const fn crc8_table() -> [u8; 256] {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u8;
        let mut k = 0;
        while k < 8 {
            c = if c & 0x80 != 0 { (c << 1) ^ 0x07 } else { c << 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

const fn crc16_table() -> [u16; 256] {
    let mut t = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = (i as u16) << 8;
        let mut k = 0;
        while k < 8 {
            c = if c & 0x8000 != 0 { (c << 1) ^ 0x8005 } else { c << 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

static CRC8: [u8; 256] = crc8_table();
static CRC16: [u16; 256] = crc16_table();

fn crc8(data: &[u8]) -> u8 {
    data.iter().fold(0, |c, &b| CRC8[(c ^ b) as usize])
}

fn crc16(data: &[u8]) -> u16 {
    data.iter().fold(0, |c, &b| (c << 8) ^ CRC16[((c >> 8) as u8 ^ b) as usize])
}

/// MSB-first bit packer over a zero-initialised-by-construction byte buffer.
struct Bits<'a> {
    out: &'a mut [u8],
    pos: usize,
    acc: u64,
    nacc: u32,
}

impl<'a> Bits<'a> {
    fn new(out: &'a mut [u8]) -> Self {
        Bits { out, pos: 0, acc: 0, nacc: 0 }
    }

    /// The low `n` (at most 32) bits of `v`.
    fn put(&mut self, v: u64, n: u32) {
        if n == 0 {
            return;
        }
        self.acc = (self.acc << n) | (v & ((1u64 << n) - 1));
        self.nacc += n;
        while self.nacc >= 8 {
            self.nacc -= 8;
            self.out[self.pos] = (self.acc >> self.nacc) as u8;
            self.pos += 1;
        }
    }

    fn put_signed(&mut self, v: i32, n: u32) {
        self.put(v as u32 as u64, n);
    }

    fn zeros(&mut self, mut n: u32) {
        while n > 0 {
            let c = n.min(32);
            self.put(0, c);
            n -= c;
        }
    }

    fn align(&mut self) {
        if self.nacc > 0 {
            self.put(0, 8 - self.nacc);
        }
    }
}

fn ilog2(v: u64) -> u32 {
    63 - v.leading_zeros()
}

fn zigzag(v: i32) -> u32 {
    ((v << 1) ^ (v >> 31)) as u32
}

const FIXED: [[i64; 4]; 5] = [[0, 0, 0, 0], [1, 0, 0, 0], [2, -1, 0, 0], [3, -3, 1, 0], [4, -6, 4, -1]];

/// Residual of the fixed predictor of order `p`; the first `p` samples are the warm-up and not in `res`.
fn residual(x: &[i32], p: usize, res: &mut [i32]) {
    let c = &FIXED[p];
    for i in p..x.len() {
        let mut pred = 0i64;
        for j in 0..p {
            pred += c[j] * x[i - 1 - j] as i64;
        }
        res[i - p] = (x[i] as i64 - pred) as i32;
    }
}

const MAX_PARAM: u32 = 14;

/// Samples in Rice partition `i` of `1 << po` for a block of `n` with `p` warm-up samples.
fn part_len(n: usize, p: usize, po: u32, i: usize) -> usize {
    (n >> po) - if i == 0 { p } else { 0 }
}

/// Bits for the best Rice parameter of one partition and that parameter.
fn best_param(part: &[i32]) -> (u64, u32) {
    if part.is_empty() {
        return (0, 0);
    }
    let sum: u64 = part.iter().map(|&v| zigzag(v) as u64).sum();
    let mean = sum / part.len() as u64;
    let k0 = if mean == 0 { 0 } else { ilog2(mean).min(MAX_PARAM) };
    let lo = k0.saturating_sub(1);
    let hi = (k0 + 1).min(MAX_PARAM);
    let mut best = (u64::MAX, 0);
    for k in lo..=hi {
        let bits: u64 = part.len() as u64 * (k as u64 + 1) + part.iter().map(|&v| (zigzag(v) >> k) as u64).sum::<u64>();
        if bits < best.0 {
            best = (bits, k);
        }
    }
    best
}

/// Partition orders that divide the block and leave every partition longer than the predictor order.
fn max_po(n: usize, p: usize) -> u32 {
    let mut po = 0;
    while po < 6 && n % (2 << po) == 0 && (n >> (po + 1)) > p {
        po += 1;
    }
    po
}

/// What the cheapest coding of one channel is.
#[derive(Clone, Copy)]
struct Plan {
    bits: u64,
    kind: Kind,
    wasted: u32,
    order: usize,
    po: u32,
    coefs: [i32; lpc::MAX_ORDER],
    shift: i32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Constant,
    Verbatim,
    Fixed,
    Lpc,
}

struct Scratch {
    shifted: [i32; BLOCK],
    res: [i32; BLOCK],
    win: [f64; BLOCK],
}

fn wasted_bits(x: &[i32]) -> u32 {
    let or = x.iter().fold(0i32, |a, &v| a | v);
    if or == 0 { 0 } else { or.trailing_zeros() }
}

/// Bits of the residual coding method, partition order and the partitions for the best partition order, and that order.
fn best_partition(res: &[i32], n: usize, order: usize) -> (u64, u32) {
    let mut best = (u64::MAX, 0);
    for po in 0..=max_po(n, order) {
        let mut bits = 2 + 4;
        let mut start = 0;
        for i in 0..1usize << po {
            let len = part_len(n, order, po, i);
            bits += 4 + best_param(&res[start..start + len]).0;
            start += len;
        }
        if bits < best.0 {
            best = (bits, po);
        }
    }
    best
}

fn analyse(x: &[i32], bps: u32, s: &mut Scratch) -> Plan {
    let n = x.len();
    let w = wasted_bits(x);
    let hdr = 8 + if w > 0 { w as u64 } else { 0 };
    for (d, &v) in s.shifted.iter_mut().zip(x) {
        *d = v >> w;
    }
    let sh = &s.shifted[..n];
    let bps = bps - w;
    let plain = Plan { bits: 0, kind: Kind::Verbatim, wasted: w, order: 0, po: 0, coefs: [0; lpc::MAX_ORDER], shift: 0 };
    if sh.iter().all(|&v| v == sh[0]) {
        return Plan { bits: hdr + bps as u64, kind: Kind::Constant, ..plain };
    }
    let mut best = Plan { bits: hdr + n as u64 * bps as u64, ..plain };
    // The order with the smallest residual magnitude; then the exact Rice cost for that one only.
    let mut order = 0;
    let mut least = u64::MAX;
    for p in 0..=4usize.min(n - 1) {
        residual(sh, p, &mut s.res);
        let mag: u64 = s.res[..n - p].iter().map(|&v| v.unsigned_abs() as u64).sum();
        if mag < least {
            least = mag;
            order = p;
        }
    }
    residual(sh, order, &mut s.res);
    let (part, po) = best_partition(&s.res[..n - order], n, order);
    let bits = hdr + order as u64 * bps as u64 + part;
    if bits < best.bits {
        best = Plan { bits, kind: Kind::Fixed, order, po, ..plain };
    }
    if n >= 64 {
        if let Some(lpc) = analyse_lpc(sh, bps, &mut s.win, &mut s.res) {
            let bits = hdr + lpc.order as u64 * bps as u64 + 4 + 5 + lpc.order as u64 * lpc::PRECISION as u64 + lpc.bits;
            if bits < best.bits {
                best = Plan { bits, kind: Kind::Lpc, order: lpc.order, po: lpc.po, coefs: lpc.coefs, shift: lpc.shift, ..plain };
            }
        }
    }
    best
}

struct LpcPlan {
    bits: u64,
    order: usize,
    po: u32,
    coefs: [i32; lpc::MAX_ORDER],
    shift: i32,
}

/// The best linear predictor of the (wasted-bits-shifted) block `sh`: orders 1-12 are ranked by an estimate of
/// their coded size, the best one gets the exact Rice cost (`bits` covers the residual coding only).
fn analyse_lpc(sh: &[i32], bps: u32, win: &mut [f64], res: &mut [i32]) -> Option<LpcPlan> {
    let n = sh.len();
    let mut ac = [0.0; lpc::MAX_ORDER + 1];
    lpc::autocorrelate(sh, win, &mut ac);
    if ac[0] <= 0.0 {
        return None;
    }
    let mut table = [[0.0; lpc::MAX_ORDER]; lpc::MAX_ORDER];
    let usable = lpc::levinson(&ac, &mut table);
    let mut best: Option<(u64, usize, [i32; lpc::MAX_ORDER], i32)> = None;
    for m in 0..usable {
        let order = m + 1;
        let Some((coefs, shift)) = lpc::quantize(&table[m][..order]) else { continue };
        if !lpc::residual(sh, &coefs[..order], shift, res) {
            continue;
        }
        let count = (n - order) as u64;
        let mag: u64 = res[..n - order].iter().map(|&v| v.unsigned_abs() as u64).sum();
        let mean_q8 = (mag << 8) / count + 256;
        let est = count * lpc::log2_q8(mean_q8) + 256 * order as u64 * (lpc::PRECISION as u64 + bps as u64);
        if best.is_none_or(|b| est < b.0) {
            best = Some((est, order, coefs, shift));
        }
    }
    let (_, order, coefs, shift) = best?;
    if !lpc::residual(sh, &coefs[..order], shift, res) {
        return None;
    }
    let (bits, po) = best_partition(&res[..n - order], n, order);
    Some(LpcPlan { bits, order, po, coefs, shift })
}

fn write_subframe(b: &mut Bits, x: &[i32], bps: u32, plan: &Plan, s: &mut Scratch) {
    let n = x.len();
    let w = plan.wasted;
    b.put(0, 1);
    let ty = match plan.kind {
        Kind::Constant => 0,
        Kind::Verbatim => 1,
        Kind::Fixed => 8 + plan.order as u64,
        Kind::Lpc => 32 + plan.order as u64 - 1,
    };
    b.put(ty, 6);
    if w > 0 {
        b.put(1, 1);
        b.zeros(w - 1);
        b.put(1, 1);
    } else {
        b.put(0, 1);
    }
    for (d, &v) in s.shifted.iter_mut().zip(x) {
        *d = v >> w;
    }
    let sh = &s.shifted[..n];
    let bps = bps - w;
    match plan.kind {
        Kind::Constant => b.put_signed(sh[0], bps),
        Kind::Verbatim => sh.iter().for_each(|&v| b.put_signed(v, bps)),
        Kind::Fixed | Kind::Lpc => {
            let p = plan.order;
            for &v in &sh[..p] {
                b.put_signed(v, bps);
            }
            if plan.kind == Kind::Lpc {
                b.put(lpc::PRECISION as u64 - 1, 4);
                b.put(plan.shift as u64 & 0x1f, 5);
                for &c in &plan.coefs[..p] {
                    b.put_signed(c, lpc::PRECISION);
                }
                lpc::residual(sh, &plan.coefs[..p], plan.shift, &mut s.res);
            } else {
                residual(sh, p, &mut s.res);
            }
            let res = &s.res[..n - p];
            b.put(0, 2);
            b.put(plan.po as u64, 4);
            let mut start = 0;
            for i in 0..1usize << plan.po {
                let len = part_len(n, p, plan.po, i);
                let part = &res[start..start + len];
                let k = best_param(part).1;
                b.put(k as u64, 4);
                for &r in part {
                    let u = zigzag(r);
                    b.zeros(u >> k);
                    b.put(1, 1);
                    b.put((u & ((1 << k) - 1)) as u64, k);
                }
                start += len;
            }
        }
    }
}

fn put_utf8(b: &mut Bits, v: u32) {
    if v < 0x80 {
        b.put(v as u64, 8);
        return;
    }
    let mut extra = 1;
    while extra < 6 && v >> (6 * extra + (6 - extra)) != 0 {
        extra += 1;
    }
    let lead = (0xFF00u32 >> (extra + 1)) as u8 | (v >> (6 * extra)) as u8;
    b.put(lead as u64, 8);
    for i in (0..extra).rev() {
        b.put((0x80 | ((v >> (6 * i)) & 0x3F)) as u64, 8);
    }
}

pub struct Encoder {
    info: Info,
    frames: u32,
    total: u64,
    min_frame: u32,
    max_frame: u32,
    min_block: u32,
    max_block: u32,
    mid: [i32; BLOCK],
    side: [i32; BLOCK],
    scratch: Scratch,
    md5: Md5,
}

impl Encoder {
    pub const fn new(info: Info) -> Self {
        Encoder {
            info,
            frames: 0,
            total: 0,
            min_frame: u32::MAX,
            max_frame: 0,
            min_block: u32::MAX,
            max_block: 0,
            mid: [0; BLOCK],
            side: [0; BLOCK],
            scratch: Scratch { shifted: [0; BLOCK], res: [0; BLOCK], win: [0.0; BLOCK] },
            md5: Md5::new(),
        }
    }

    /// Samples per channel encoded so far.
    pub fn total_samples(&self) -> u64 {
        self.total
    }

    fn streaminfo(&self) -> [u8; 34] {
        let mut s = [0u8; 34];
        let (minb, maxb) = if self.max_block == 0 { (BLOCK as u32, BLOCK as u32) } else { (self.min_block, self.max_block) };
        let (minf, maxf) = if self.max_frame == 0 { (0, 0) } else { (self.min_frame, self.max_frame) };
        s[0..2].copy_from_slice(&(minb as u16).to_be_bytes());
        s[2..4].copy_from_slice(&(maxb as u16).to_be_bytes());
        s[4..7].copy_from_slice(&minf.to_be_bytes()[1..]);
        s[7..10].copy_from_slice(&maxf.to_be_bytes()[1..]);
        // 20 bits of rate, 3 of channels-1, 5 of bits-1, 36 of total samples: 64 bits.
        let packed = ((self.info.sample_rate as u64) << 44)
            | (((self.info.channels - 1) as u64) << 41)
            | (((self.info.bits - 1) as u64) << 36)
            | (self.total & 0xF_FFFF_FFFF);
        s[10..18].copy_from_slice(&packed.to_be_bytes());
        if self.total > 0 {
            s[18..34].copy_from_slice(&self.md5.clone().finish());
        }
        s
    }

    /// `fLaC`, STREAMINFO and a VORBIS_COMMENT block holding `tags` ("KEY", "value"); returns the bytes written.
    /// `out` needs `header_bytes` of the tags' total size.
    pub fn write_header(&self, out: &mut [u8], tags: &[(&str, &str)]) -> Result<usize, Error> {
        self.info.check()?;
        let need: usize = tags.iter().map(|(k, v)| k.len() + v.len() + 5).sum();
        if out.len() < header_bytes(need) {
            return Err(Error::OutputTooSmall);
        }
        out[0..4].copy_from_slice(b"fLaC");
        out[4..8].copy_from_slice(&[0, 0, 0, 34]);
        out[8..42].copy_from_slice(&self.streaminfo());
        let vendor = b"fosfor flac-enc";
        let mut len = 4 + vendor.len() + 4;
        for (k, v) in tags {
            len += 4 + k.len() + 1 + v.len();
        }
        out[42] = 0x80 | 4;
        out[43..46].copy_from_slice(&(len as u32).to_be_bytes()[1..]);
        let mut p = 46;
        out[p..p + 4].copy_from_slice(&(vendor.len() as u32).to_le_bytes());
        p += 4;
        out[p..p + vendor.len()].copy_from_slice(vendor);
        p += vendor.len();
        out[p..p + 4].copy_from_slice(&(tags.len() as u32).to_le_bytes());
        p += 4;
        for (k, v) in tags {
            out[p..p + 4].copy_from_slice(&((k.len() + 1 + v.len()) as u32).to_le_bytes());
            p += 4;
            out[p..p + k.len()].copy_from_slice(k.as_bytes());
            p += k.len();
            out[p] = b'=';
            p += 1;
            out[p..p + v.len()].copy_from_slice(v.as_bytes());
            p += v.len();
        }
        Ok(p)
    }

    /// Rewrites STREAMINFO (total samples, block and frame sizes) in a header written earlier; call after the last
    /// frame, on the first 42 bytes of the file.
    pub fn patch_header(&self, header: &mut [u8]) {
        header[8..42].copy_from_slice(&self.streaminfo());
    }

    /// Encodes one block: `chans[c]` holds the samples of channel `c`, all the same length (1..=`BLOCK`). Only the
    /// last block of a stream may be shorter than the ones before it. Returns the frame's length in `out`.
    pub fn encode_frame(&mut self, chans: &[&[i32]], out: &mut [u8]) -> Result<usize, Error> {
        self.info.check()?;
        let ch = self.info.channels as usize;
        if chans.len() != ch || chans[0].is_empty() || chans[0].len() > BLOCK || chans.iter().any(|c| c.len() != chans[0].len()) {
            return Err(Error::BadBlock);
        }
        if out.len() < max_frame_bytes(ch) {
            return Err(Error::OutputTooSmall);
        }
        let n = chans[0].len();
        let bps = self.info.bits;
        let lim = 1i32 << (bps - 1);
        if chans.iter().any(|c| c.iter().any(|&v| v < -lim || v >= lim)) {
            return Err(Error::Range);
        }

        // Stereo: cost all four assignments; the choice is made before any bit is written.
        let mut assign = ch as u32 - 1;
        let mut plans = [None; 4];
        if ch == 2 {
            let (l, r) = (chans[0], chans[1]);
            for i in 0..n {
                self.mid[i] = (l[i] + r[i]) >> 1;
                self.side[i] = l[i] - r[i];
            }
            let pl = analyse(l, bps, &mut self.scratch);
            let pr = analyse(r, bps, &mut self.scratch);
            let pm = analyse(&self.mid[..n], bps, &mut self.scratch);
            let ps = analyse(&self.side[..n], bps + 1, &mut self.scratch);
            let options = [(pl.bits + pr.bits, 1), (pl.bits + ps.bits, 8), (ps.bits + pr.bits, 9), (pm.bits + ps.bits, 10)];
            assign = options.iter().min_by_key(|o| o.0).map(|o| o.1).unwrap();
            plans = [Some(pl), Some(pr), Some(pm), Some(ps)];
        }

        let (bs_code, bs_ext): (u64, Option<(u64, u32)>) = match n {
            4096 => (12, None),
            256 => (8, None),
            512 => (9, None),
            1024 => (10, None),
            2048 => (11, None),
            _ if n <= 256 => (6, Some((n as u64 - 1, 8))),
            _ => (7, Some((n as u64 - 1, 16))),
        };
        let rate_code = match self.info.sample_rate {
            88200 => 1,
            176400 => 2,
            192000 => 3,
            8000 => 4,
            16000 => 5,
            22050 => 6,
            24000 => 7,
            32000 => 8,
            44100 => 9,
            48000 => 10,
            96000 => 11,
            _ => 0,
        };
        let bps_code = match bps {
            8 => 1,
            12 => 2,
            16 => 4,
            20 => 5,
            24 => 6,
            _ => 0,
        };

        let mut b = Bits::new(out);
        b.put(0xFFF8, 16);
        b.put(bs_code, 4);
        b.put(rate_code, 4);
        b.put(assign as u64, 4);
        b.put(bps_code, 3);
        b.put(0, 1);
        put_utf8(&mut b, self.frames);
        if let Some((v, bits)) = bs_ext {
            b.put(v, bits);
        }
        let hl = b.pos;
        let c8 = crc8(&b.out[..hl]);
        b.put(c8 as u64, 8);

        if ch == 2 {
            // Assignment codes: 1 = independent L/R, 8 = left/side, 9 = side/right, 10 = mid/side.
            let (l, r) = (chans[0], chans[1]);
            // plans: left, right, mid, side
            let [pl, pr, pm, ps] = plans.map(|p| p.unwrap());
            let (first, second, bits1, bits2, p1, p2): (&[i32], &[i32], u32, u32, Plan, Plan) = match assign {
                1 => (l, r, bps, bps, pl, pr),
                8 => (l, &self.side[..n], bps, bps + 1, pl, ps),
                9 => (&self.side[..n], r, bps + 1, bps, ps, pr),
                _ => (&self.mid[..n], &self.side[..n], bps, bps + 1, pm, ps),
            };
            write_subframe(&mut b, first, bits1, &p1, &mut self.scratch);
            write_subframe(&mut b, second, bits2, &p2, &mut self.scratch);
        } else {
            for c in chans {
                let p = analyse(c, bps, &mut self.scratch);
                write_subframe(&mut b, c, bps, &p, &mut self.scratch);
            }
        }
        b.align();
        let body = b.pos;
        let c16 = crc16(&b.out[..body]);
        b.put(c16 as u64, 16);
        let len = b.pos;

        let width = (bps as usize).div_ceil(8);
        for i in 0..n {
            for c in chans {
                self.md5.update(&c[i].to_le_bytes()[..width]);
            }
        }
        self.frames += 1;
        self.total += n as u64;
        self.min_frame = self.min_frame.min(len as u32);
        self.max_frame = self.max_frame.max(len as u32);
        self.min_block = self.min_block.min(n as u32);
        self.max_block = self.max_block.max(n as u32);
        Ok(len)
    }
}

#[cfg(test)]
mod tests;
