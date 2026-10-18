//! Audio DSP building blocks for hard-float tapes: `math` (sin, log10, ... for `no_std`), `fft` (radix-2 FFT in f32 with an
//! AVX2/FMA build of the same code, chosen by the caller from `fp_level`) and `pcm` (device frames to f32), `level` (sample peak, peak hold), `stereo` (correlation, balance), `window` (analysis windows with the
//! normalisation constants a spectrum needs). No allocation: tables and buffers belong to the caller, usually a `static`.
//! Only for tapes built for `x86_64-fos-tape-fp` (the soft-float target has no SSE and `sqrt` here is an SSE instruction).
#![cfg_attr(not(test), no_std)]

pub mod fft;
pub mod level;
pub mod math;
pub mod pcm;
pub mod stereo;
pub mod window;
