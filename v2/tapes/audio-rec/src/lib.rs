//! The parts of a recorder that need no hardware, so they can be tested on the host:
//!
//! - `wav`: the file header (plain PCM, or WAVE_FORMAT_EXTENSIBLE for more than two channels or 24 bits, and RF64
//!   for takes over 4 GiB);
//! - `preroll`: a ring that always holds the last few seconds, so a take can begin before the moment it was asked for;
//! - `silence`: the trigger that starts a take on sound and ends it on silence, with configurable timing;
//! - `peaks`: feeding the meters from the device's own sample format.
#![cfg_attr(not(test), no_std)]

pub mod peaks;
pub mod preroll;
pub mod recorder;
pub mod select;
pub mod silence;
pub mod stream;
pub mod wav;
