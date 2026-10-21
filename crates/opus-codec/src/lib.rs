//! A thin Rust wrapper around a vendored libopus (BSD-3-Clause, `crates/opus-codec/vendor/opus`, v1.6.1, the
//! floating-point build), built freestanding for the 64-bit hard-float tape target by `make opus-lib`: no libc, no malloc. libopus's own allocations
//! (a scratch "pseudo stack" and the codec state) go through the tape API's `alloc`/`free`, remembered with a
//! small size header in front of each block, so a tape needs nothing from the system but the table it already
//! has. Only one encoder and one decoder at a time are expected per stream, but any number of instances work.
#![no_std]

use core::ffi::c_void;
use tape_rt::api;

unsafe extern "C" {
    fn opus_decoder_get_size(channels: i32) -> i32;
    fn opus_decoder_init(state: *mut c_void, sample_rate: i32, channels: i32) -> i32;
    fn opus_decode(state: *mut c_void, data: *const u8, len: i32, pcm: *mut i16, frame_size: i32, decode_fec: i32) -> i32;
    fn opus_decode_float(state: *mut c_void, data: *const u8, len: i32, pcm: *mut f32, frame_size: i32, decode_fec: i32) -> i32;
    fn opus_encode_float(state: *mut c_void, pcm: *const f32, frame_size: i32, data: *mut u8, max_data_bytes: i32) -> i32;
    fn opus_encoder_get_size(channels: i32) -> i32;
    fn opus_encoder_init(state: *mut c_void, sample_rate: i32, channels: i32, application: i32) -> i32;
    fn opus_encode(state: *mut c_void, pcm: *const i16, frame_size: i32, data: *mut u8, max_data_bytes: i32) -> i32;
    /// Variadic in C; every call here passes exactly one trailing `i32`.
    fn opus_encoder_ctl(state: *mut c_void, request: i32, value: i32) -> i32;
}

const OPUS_APPLICATION_AUDIO: i32 = 2049;
const OPUS_APPLICATION_RESTRICTED_LOWDELAY: i32 = 2051;
const OPUS_SET_COMPLEXITY_REQUEST: i32 = 4010;
const OPUS_SET_BITRATE_REQUEST: i32 = 4002;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpusError {
    BadArgument,
    BufferTooSmall,
    InternalError,
    InvalidPacket,
    Unimplemented,
    InvalidState,
    AllocFail,
    OutputTooSmall,
    Unknown(i32),
}

impl OpusError {
    fn from_code(code: i32) -> Self {
        match code {
            -1 => Self::BadArgument,
            -2 => Self::BufferTooSmall,
            -3 => Self::InternalError,
            -4 => Self::InvalidPacket,
            -5 => Self::Unimplemented,
            -6 => Self::InvalidState,
            -7 => Self::AllocFail,
            other => Self::Unknown(other),
        }
    }
}

pub type Result<T> = core::result::Result<T, OpusError>;

/// Bytes in front of every block libopus asks for, holding the block's total size for `free`.
const HEADER: usize = 16;

#[unsafe(no_mangle)]
extern "C" fn opus_alloc(size: usize) -> *mut c_void {
    let total = size + HEADER;
    let base = (api().alloc)(total);
    if base == 0 {
        return core::ptr::null_mut();
    }
    unsafe { (base as *mut usize).write(total) };
    (base + HEADER) as *mut c_void
}

#[unsafe(no_mangle)]
extern "C" fn opus_free(pointer: *mut c_void) {
    if pointer.is_null() {
        return;
    }
    let base = pointer as usize - HEADER;
    let total = unsafe { (base as *const usize).read() };
    (api().free)(base, total);
}

/// Plain libc `abs`, referenced by a few vendored files (the freestanding build disables the compiler's builtin).
#[unsafe(no_mangle)]
extern "C" fn abs(value: i32) -> i32 {
    value.wrapping_abs()
}

pub struct Decoder {
    state: *mut c_void,
    state_len: usize,
    channels: i32,
}

impl Decoder {
    pub fn new(sample_rate: i32, channels: i32) -> Result<Self> {
        let size = unsafe { opus_decoder_get_size(channels) };
        if size <= 0 {
            return Err(OpusError::BadArgument);
        }
        let address = (api().alloc)(size as usize);
        if address == 0 {
            return Err(OpusError::AllocFail);
        }
        let state = address as *mut c_void;
        let code = unsafe { opus_decoder_init(state, sample_rate, channels) };
        if code != 0 {
            (api().free)(address, size as usize);
            return Err(OpusError::from_code(code));
        }
        Ok(Self { state, state_len: size as usize, channels })
    }

    /// Decodes one packet into `pcm` (interleaved 16-bit); returns the frames decoded per channel.
    pub fn decode(&mut self, packet: &[u8], pcm: &mut [i16], frame_size: usize) -> Result<usize> {
        if pcm.len() < frame_size * self.channels as usize {
            return Err(OpusError::OutputTooSmall);
        }
        let n = unsafe { opus_decode(self.state, packet.as_ptr(), packet.len() as i32, pcm.as_mut_ptr(), frame_size as i32, 0) };
        if n < 0 { Err(OpusError::from_code(n)) } else { Ok(n as usize) }
    }
}

impl Decoder {
    /// Like `decode`, into interleaved 32-bit float (full scale 1.0): no integer step, so a float device gets the
    /// decoder's own output.
    pub fn decode_float(&mut self, packet: &[u8], pcm: &mut [f32], frame_size: usize) -> Result<usize> {
        if pcm.len() < frame_size * self.channels as usize {
            return Err(OpusError::OutputTooSmall);
        }
        let n = unsafe { opus_decode_float(self.state, packet.as_ptr(), packet.len() as i32, pcm.as_mut_ptr(), frame_size as i32, 0) };
        if n < 0 { Err(OpusError::from_code(n)) } else { Ok(n as usize) }
    }

    /// `conceal` into float.
    pub fn conceal_float(&mut self, pcm: &mut [f32], frame_size: usize) -> Result<usize> {
        if pcm.len() < frame_size * self.channels as usize {
            return Err(OpusError::OutputTooSmall);
        }
        let n = unsafe { opus_decode_float(self.state, core::ptr::null(), 0, pcm.as_mut_ptr(), frame_size as i32, 0) };
        if n < 0 { Err(OpusError::from_code(n)) } else { Ok(n as usize) }
    }

    /// Packet-loss concealment: asks libopus to synthesise `frame_size` frames per channel for a packet that never
    /// arrived (it extrapolates from the last good audio).
    pub fn conceal(&mut self, pcm: &mut [i16], frame_size: usize) -> Result<usize> {
        if pcm.len() < frame_size * self.channels as usize {
            return Err(OpusError::OutputTooSmall);
        }
        let n = unsafe { opus_decode(self.state, core::ptr::null(), 0, pcm.as_mut_ptr(), frame_size as i32, 0) };
        if n < 0 { Err(OpusError::from_code(n)) } else { Ok(n as usize) }
    }
}

/// Samples per channel (at 48 kHz) a packet decodes to, read from its first byte (the TOC, RFC 6716 section 3.1):
/// the configuration number gives the frame duration, the code says how many frames the packet holds. `None` for
/// an empty or malformed packet.
pub fn packet_samples(packet: &[u8]) -> Option<usize> {
    let toc = *packet.first()?;
    let config = usize::from(toc >> 3);
    // Frame duration in 48 kHz samples: SILK-only 10/20/40/60 ms (configs 0-11), hybrid 10/20 ms (12-15),
    // CELT-only 2.5/5/10/20 ms (16-31).
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

impl Drop for Decoder {
    fn drop(&mut self) {
        (api().free)(self.state as usize, self.state_len);
    }
}

pub struct Encoder {
    state: *mut c_void,
    state_len: usize,
    channels: i32,
}

impl Encoder {
    /// `complexity` is libopus's 0 (cheapest) to 10 (best) scale; `low_delay` selects the restricted-lowdelay
    /// application (no look-ahead, frames of 2.5 to 20 ms) instead of general audio.
    pub fn new(sample_rate: i32, channels: i32, bitrate_bps: i32, complexity: i32, low_delay: bool) -> Result<Self> {
        let size = unsafe { opus_encoder_get_size(channels) };
        if size <= 0 {
            return Err(OpusError::BadArgument);
        }
        let address = (api().alloc)(size as usize);
        if address == 0 {
            return Err(OpusError::AllocFail);
        }
        let state = address as *mut c_void;
        let application = if low_delay { OPUS_APPLICATION_RESTRICTED_LOWDELAY } else { OPUS_APPLICATION_AUDIO };
        let code = unsafe { opus_encoder_init(state, sample_rate, channels, application) };
        if code != 0 {
            (api().free)(address, size as usize);
            return Err(OpusError::from_code(code));
        }
        let (a, b) = unsafe { (opus_encoder_ctl(state, OPUS_SET_COMPLEXITY_REQUEST, complexity), opus_encoder_ctl(state, OPUS_SET_BITRATE_REQUEST, bitrate_bps)) };
        if a != 0 || b != 0 {
            (api().free)(address, size as usize);
            return Err(OpusError::from_code(a.min(b)));
        }
        Ok(Self { state, state_len: size as usize, channels })
    }

    /// Encodes `frame_size` frames per channel of interleaved 16-bit PCM; returns the packet's length.
    pub fn encode(&mut self, pcm: &[i16], frame_size: usize, packet: &mut [u8]) -> Result<usize> {
        if pcm.len() < frame_size * self.channels as usize {
            return Err(OpusError::OutputTooSmall);
        }
        let n = unsafe { opus_encode(self.state, pcm.as_ptr(), frame_size as i32, packet.as_mut_ptr(), packet.len() as i32) };
        if n < 0 { Err(OpusError::from_code(n)) } else { Ok(n as usize) }
    }
}

impl Encoder {
    /// Encodes `frame_size` frames per channel of interleaved 32-bit float (full scale 1.0); returns the packet's length.
    pub fn encode_float(&mut self, pcm: &[f32], frame_size: usize, packet: &mut [u8]) -> Result<usize> {
        if pcm.len() < frame_size * self.channels as usize {
            return Err(OpusError::OutputTooSmall);
        }
        let n = unsafe { opus_encode_float(self.state, pcm.as_ptr(), frame_size as i32, packet.as_mut_ptr(), packet.len() as i32) };
        if n < 0 { Err(OpusError::from_code(n)) } else { Ok(n as usize) }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        (api().free)(self.state as usize, self.state_len);
    }
}
