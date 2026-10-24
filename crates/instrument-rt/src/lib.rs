//! The tape-side glue shared by the instrument tapes (`organ`, and the string machine to come): everything that is not the sound
//! (`crates/organ`, `crates/fx`) and not the picture. `Output` runs the audio device (format, channel pair, latency, a scope
//! of what was played), `Touches` turns finger events into grabs on widgets, `TypedKeys` gives a terminal's key presses an
//! end (a terminal sends no key release), `poll_midi` drains the USB-MIDI queue, and `blit_banded` sends the picture in
//! bands with the audio topped up in between, because a full-screen framebuffer write takes longer than the queue holds.
#![cfg_attr(not(test), no_std)]

mod output;
mod touch;

pub use output::{CHUNK, MAX_FRAME_BYTES, Output, SCOPE_LEN, pack};
pub use touch::{TouchTarget, Touches, TypedKeys};

use audio_screen::Screen;

/// Rows of the picture sent between two audio top-ups.
pub const BAND_ROWS: u32 = 32;

/// Hands every pending USB-MIDI message (at most 64 per call) to `handle`, which says whether it was one the instrument
/// understands; returns how many were.
pub fn poll_midi(mut handle: impl FnMut(&[u8]) -> bool) -> u32 {
    let mut accepted = 0;
    for _ in 0..64 {
        let Some(message) = tape_rt::midi_event() else { break };
        if handle(&message.data[..message.len]) {
            accepted += 1;
        }
    }
    accepted
}

/// Sends the screen's last frame in bands of `BAND_ROWS` rows, calling `between` after each (pump the audio there).
pub fn blit_banded(screen: &Screen, mut between: impl FnMut()) {
    let width = screen.width;
    let mut row = 0;
    while row < screen.height {
        let rows = BAND_ROWS.min(screen.height - row);
        let pixels = &screen.pixels()[(row * width) as usize..((row + rows) * width) as usize];
        tape_rt::gfx_blit_on(screen.index, 0, row, width, rows, pixels, width);
        between();
        row += rows;
    }
}

#[cfg(test)]
mod tests;
