//! Rust FFI wrapper around a vendored, fixed-point-only libopus codec.
//!
//! `vendor/opus/` is libopus 1.5.2 (BSD-3-Clause, see `vendor/opus/COPYING`),
//! trimmed to exactly the files this `FIXED_POINT`/`DISABLE_FLOAT_API`/
//! `CUSTOM_MODES`-off build needs -- no ARM/x86 SIMD, no multistream/
//! ambisonics, no libm, no libc `malloc`. See `../config.h` and
//! `../custom_support.h` for the exact build configuration, and the root
//! Makefile's `opus-codec-lib` recipe for how it is compiled and archived
//! (`libopus_fixed.a`) and linked into a tape via `RUSTFLAGS`.
//!
//! [`Decoder`] wraps only `opus_decoder_get_size`/`opus_decoder_init`/
//! `opus_decode`; [`Encoder`] wraps the matching `opus_encoder_*` triple
//! plus `opus_encoder_ctl` (used only to fix complexity and bitrate, see
//! [`Encoder::new`]). Neither ever calls the corresponding
//! `opus_{decoder,encoder}_create` (which would use libopus's own
//! libc-`malloc`-backed `opus_alloc`), instead sizing and owning their
//! state buffer through the tape ABI's own `alloc`/`free` callbacks.
#![no_std]

use core::ffi::c_void;

unsafe extern "C" {
    fn opus_decoder_get_size(channels: i32) -> i32;
    fn opus_decoder_init(state: *mut c_void, sample_rate: i32, channels: i32) -> i32;
    fn opus_decode(
        state: *mut c_void,
        data: *const u8,
        len: i32,
        pcm: *mut i16,
        frame_size: i32,
        decode_fec: i32,
    ) -> i32;

    fn opus_encoder_get_size(channels: i32) -> i32;
    fn opus_encoder_init(state: *mut c_void, sample_rate: i32, channels: i32, application: i32) -> i32;
    fn opus_encode(
        state: *mut c_void,
        pcm: *const i16,
        frame_size: i32,
        data: *mut u8,
        max_data_bytes: i32,
    ) -> i32;
    /// Variadic C function; ordinary Rust FFI supports *calling* one, and
    /// every call site here passes exactly one trailing `i32`, matching
    /// `include/opus_defines.h`'s `OPUS_SET_COMPLEXITY`/`OPUS_SET_BITRATE`
    /// macros' own `__opus_check_int(x)` shape.
    fn opus_encoder_ctl(state: *mut c_void, request: i32, value: i32) -> i32;
}

/// `include/opus_defines.h`'s `OPUS_APPLICATION_AUDIO` -- tuned for
/// general music/audio rather than speech (`OPUS_APPLICATION_VOIP`) or
/// minimal algorithmic delay (`OPUS_APPLICATION_RESTRICTED_LOWDELAY`).
/// Fixed, not exposed as a choice: this project has no reason to encode
/// anything other than general audio.
const OPUS_APPLICATION_AUDIO: i32 = 2049;
/// `include/opus_defines.h`'s `OPUS_SET_COMPLEXITY_REQUEST`/
/// `OPUS_SET_BITRATE_REQUEST`.
const OPUS_SET_COMPLEXITY_REQUEST: i32 = 4010;
const OPUS_SET_BITRATE_REQUEST: i32 = 4002;
/// Maximum encoder complexity (0-10 scale), hardcoded rather than exposed
/// as a tunable: this project's real-hardware floor is Pentium 4, and
/// Opus's CPU cost even at max complexity is trivial for that era of
/// hardware -- the only reason it would ever look expensive is QEMU/TCG
/// software emulation, not a real target-hardware constraint (see
/// `tapes/test_audio_rtp_opus_e2e.py`'s `kvm_available()`).
const OPUS_MAX_COMPLEXITY: i32 = 10;
/// Default target bitrate for 48 kHz stereo: within Opus's conventional
/// 96-128 kbps "transparent" range. Overridable by the caller; this is
/// only the fallback when no bitrate is given.
pub const DEFAULT_BITRATE_BPS: i32 = 96_000;

/// Errors libopus's `int` return codes map to (see `include/opus_defines.h`'s
/// `OPUS_BAD_ARG`..`OPUS_UNIMPLEMENTED` in the vendored tree).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpusError {
    BadArgument,
    BufferTooSmall,
    InternalError,
    InvalidPacket,
    Unimplemented,
    InvalidState,
    AllocFail,
    /// The caller's PCM output buffer cannot hold `frame_size * channels`
    /// samples; not a libopus error code, checked before calling `opus_decode`.
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

/// One tape's currently-installed allocator, backing the vendored decoder's
/// `opus_alloc`/`opus_free` calls (see `custom_support.h`). A `static mut`
/// is safe here for the same reason every other single-threaded, no-
/// concurrency mutable global in this codebase is (e.g. `shell.rs`'s
/// `ACTIVE_TERM_PEER`): a tape runs cooperatively, with no interrupts or
/// second thread that could observe this mid-update.
static mut CURRENT_ALLOC: Option<extern "C" fn(u32) -> u32> = None;
static mut CURRENT_FREE: Option<extern "C" fn(u32, u32)> = None;

/// Bytes reserved before the pointer libopus sees, storing the total
/// allocation size so `opus_free` can pass it back to the tape ABI's
/// `free(address, bytes)`, which -- unlike libc's `free(ptr)` -- requires
/// the caller to remember the size.
const ALLOC_HEADER_LEN: u32 = 4;

/// Plain libc `abs()`, referenced by a few vendored files (e.g. `cwrs.c`,
/// `pitch.c`). GCC normally substitutes its own `__builtin_abs` for this
/// automatically, but `-fno-builtin` (part of this freestanding build's
/// flags, see the root Makefile's `OPUS_CODEC_CFLAGS`) disables that,
/// leaving a real undefined symbol -- caught only once the freestanding
/// i686-fos build actually linked (a host build always has libc's own
/// `abs` to satisfy it, which is why this did not show up until then).
#[unsafe(no_mangle)]
extern "C" fn abs(value: i32) -> i32 {
    value.wrapping_abs()
}

#[unsafe(no_mangle)]
extern "C" fn opus_alloc(size: usize) -> *mut c_void {
    let alloc = match unsafe { CURRENT_ALLOC } {
        Some(f) => f,
        None => return core::ptr::null_mut(),
    };
    let total = size as u32 + ALLOC_HEADER_LEN;
    let base = alloc(total);
    if base == 0 {
        return core::ptr::null_mut();
    }
    unsafe { (base as *mut u32).write(total) };
    (base + ALLOC_HEADER_LEN) as *mut c_void
}

#[unsafe(no_mangle)]
extern "C" fn opus_free(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let free = match unsafe { CURRENT_FREE } {
        Some(f) => f,
        None => return,
    };
    let base = ptr as u32 - ALLOC_HEADER_LEN;
    let total = unsafe { (base as *const u32).read() };
    free(base, total);
}

/// A single Opus decoder instance, owning its decoder-state buffer through
/// the tape ABI's `alloc`/`free` callbacks rather than libopus's own
/// libc-`malloc`-backed `opus_decoder_create`.
pub struct Decoder {
    state: *mut c_void,
    state_len: u32,
    free: extern "C" fn(u32, u32),
    channels: i32,
}

impl Decoder {
    /// `channels` is 1 or 2. `alloc`/`free` are the tape ABI's own
    /// callbacks (`kernel/src/shell.rs`'s `tape_alloc`/`tape_free`); they
    /// are also installed as the vendored decoder's global allocator (see
    /// [`opus_alloc`]/[`opus_free`]) for the lifetime of the process, since
    /// a fos tape never runs more than one decoder concurrently.
    pub fn new(
        sample_rate: i32,
        channels: i32,
        alloc: extern "C" fn(u32) -> u32,
        free: extern "C" fn(u32, u32),
    ) -> Result<Self> {
        unsafe {
            CURRENT_ALLOC = Some(alloc);
            CURRENT_FREE = Some(free);
        }

        let size = unsafe { opus_decoder_get_size(channels) };
        if size <= 0 {
            return Err(OpusError::BadArgument);
        }
        let state_len = size as u32;

        let address = alloc(state_len);
        if address == 0 {
            return Err(OpusError::AllocFail);
        }
        let state = address as *mut c_void;

        let ret = unsafe { opus_decoder_init(state, sample_rate, channels) };
        if ret != 0 {
            free(address, state_len);
            return Err(OpusError::from_code(ret));
        }

        Ok(Self {
            state,
            state_len,
            free,
            channels,
        })
    }

    /// Decodes one Opus packet into `pcm_out` (interleaved `i16` samples,
    /// `channels` wide), requesting up to `frame_size` samples per channel.
    /// Returns the number of samples actually decoded per channel.
    pub fn decode(&mut self, packet: &[u8], pcm_out: &mut [i16], frame_size: usize) -> Result<usize> {
        let needed = frame_size
            .checked_mul(self.channels as usize)
            .ok_or(OpusError::OutputTooSmall)?;
        if pcm_out.len() < needed {
            return Err(OpusError::OutputTooSmall);
        }
        let n = unsafe {
            opus_decode(
                self.state,
                packet.as_ptr(),
                packet.len() as i32,
                pcm_out.as_mut_ptr(),
                frame_size as i32,
                0,
            )
        };
        if n < 0 {
            Err(OpusError::from_code(n))
        } else {
            Ok(n as usize)
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        (self.free)(self.state as u32, self.state_len);
    }
}

/// A single Opus encoder instance, owning its encoder-state buffer through
/// the tape ABI's `alloc`/`free` callbacks rather than libopus's own
/// libc-`malloc`-backed `opus_encoder_create`. Complexity is fixed at
/// [`OPUS_MAX_COMPLEXITY`] and the application at
/// [`OPUS_APPLICATION_AUDIO`] -- neither is a per-instance choice.
pub struct Encoder {
    state: *mut c_void,
    state_len: u32,
    free: extern "C" fn(u32, u32),
    channels: i32,
}

impl Encoder {
    /// `channels` is 1 or 2; `sample_rate` must be one of libopus's five
    /// accepted rates (8/12/16/24/48 kHz). `alloc`/`free` are the tape
    /// ABI's own callbacks, also installed as the vendored codec's global
    /// allocator (see [`opus_alloc`]/[`opus_free`]) -- shared with any
    /// [`Decoder`] the same tape also constructs, since a fos tape never
    /// runs more than one codec direction pair concurrently.
    pub fn new(
        sample_rate: i32,
        channels: i32,
        bitrate_bps: i32,
        alloc: extern "C" fn(u32) -> u32,
        free: extern "C" fn(u32, u32),
    ) -> Result<Self> {
        unsafe {
            CURRENT_ALLOC = Some(alloc);
            CURRENT_FREE = Some(free);
        }

        let size = unsafe { opus_encoder_get_size(channels) };
        if size <= 0 {
            return Err(OpusError::BadArgument);
        }
        let state_len = size as u32;

        let address = alloc(state_len);
        if address == 0 {
            return Err(OpusError::AllocFail);
        }
        let state = address as *mut c_void;

        let ret = unsafe { opus_encoder_init(state, sample_rate, channels, OPUS_APPLICATION_AUDIO) };
        if ret != 0 {
            free(address, state_len);
            return Err(OpusError::from_code(ret));
        }

        let complexity_ret = unsafe { opus_encoder_ctl(state, OPUS_SET_COMPLEXITY_REQUEST, OPUS_MAX_COMPLEXITY) };
        let bitrate_ret = unsafe { opus_encoder_ctl(state, OPUS_SET_BITRATE_REQUEST, bitrate_bps) };
        if complexity_ret != 0 || bitrate_ret != 0 {
            free(address, state_len);
            return Err(OpusError::from_code(complexity_ret.min(bitrate_ret)));
        }

        Ok(Self {
            state,
            state_len,
            free,
            channels,
        })
    }

    /// Encodes exactly one frame (`frame_size` samples per channel, one of
    /// libopus's accepted frame durations at this encoder's sample rate --
    /// e.g. 960 for 20 ms @ 48 kHz) from `pcm` (interleaved `i16`,
    /// `channels` wide) into `packet_out`. Returns the number of bytes
    /// written.
    pub fn encode(&mut self, pcm: &[i16], frame_size: usize, packet_out: &mut [u8]) -> Result<usize> {
        let needed = frame_size
            .checked_mul(self.channels as usize)
            .ok_or(OpusError::OutputTooSmall)?;
        if pcm.len() < needed {
            return Err(OpusError::OutputTooSmall);
        }
        let n = unsafe {
            opus_encode(
                self.state,
                pcm.as_ptr(),
                frame_size as i32,
                packet_out.as_mut_ptr(),
                packet_out.len() as i32,
            )
        };
        if n < 0 {
            Err(OpusError::from_code(n))
        } else {
            Ok(n as usize)
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        (self.free)(self.state as u32, self.state_len);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // [`opus_alloc`]/[`opus_free`] treat the tape ABI's `u32` "address" as a
    // real, dereferenceable pointer -- correct on the actual 32-bit i686-fos
    // target this crate ships on, where every address genuinely fits in
    // `u32`, but not safely exercisable here: a host test's real pointers
    // are 64-bit and truncating one to `u32` (as `opus_alloc` would then
    // dereference) is not a valid address, only ever an accident away from
    // a segfault. That header-bookkeeping path is instead proven by the
    // real freestanding i686-fos build once linked into a tape (see the
    // root Makefile's `opus-codec-lib` and `tape-audio-rtp` recipes) rather
    // than a host unit test. Only the pure, pointer-free error mapping is
    // tested here.

    #[test]
    fn error_code_mapping() {
        assert_eq!(OpusError::from_code(-1), OpusError::BadArgument);
        assert_eq!(OpusError::from_code(-4), OpusError::InvalidPacket);
        assert_eq!(OpusError::from_code(-7), OpusError::AllocFail);
        assert_eq!(OpusError::from_code(-99), OpusError::Unknown(-99));
    }

    #[test]
    fn opus_alloc_without_installed_allocator_returns_null() {
        unsafe {
            CURRENT_ALLOC = None;
            CURRENT_FREE = None;
        }
        assert!(opus_alloc(16).is_null());
        opus_free(core::ptr::null_mut()); // must not panic on a null pointer
    }
}
