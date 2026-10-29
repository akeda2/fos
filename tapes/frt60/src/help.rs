//! The help screen: plain-language text on what is measured, what the numbers mean and how to read the four plots.
//! It is word-wrapped to the plot area and split into pages by the lines that fit, so it follows the screen size.

use core::fmt::Write;

use gfx::Canvas;
use gfx::theme::theme;
use tape_rt::Line;

use crate::view::Layout;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Style {
    Head,
    Body,
}

use Style::{Body, Head};

/// The help is set in the small font on every screen: it is a lot of text, and the small font keeps it to a few pages.
const LINE_H: i32 = 20;

const TEXT: &[(Style, &str)] = &[
    (Head, "What this measures"),
    (Body, "When a sound stops in a room, it does not stop at once: it dies away as it bounces off walls, floor and ceiling. How long that takes is the reverberation time. A bathroom or a church rings for a long time, a room with curtains, carpet and sofas for a short time. The tape records the room's reply to one sharp sound and works out how fast it fades, separately for bass, mid and treble."),
    (Head, "How to measure"),
    (Body, "1. Put the microphone where people will listen, away from walls. 2. Press Space: the tape is now armed and waits for a loud sound. 3. Pop a balloon (or clap hard) at the place the sound normally comes from, then keep quiet until the recording ends. 4. Read the plots; press s to save the results. With a loudspeaker connected, m switches to the sweep method: the tape plays a sound itself, which is more exact and needs no balloon."),
    (Body, "Up and Down set the trigger level: lower it if nothing happens when you pop, raise it if noise sets it off. The status line shows the input level while armed. If it says the input clipped, the pop was too loud for the microphone: move further away or lower the gain, or the numbers will be wrong. A quiet room gives much better results."),
    (Head, "The numbers on the third line"),
    (Body, "T20 and T30: the reverberation time in seconds, the time the sound needs to fade by 60 dB (to one millionth of its power). It is worked out from the first 20 or 30 dB of the fade, so T30 needs a cleaner recording than T20. This is the number to quote. EDT (early decay time): the same, but from only the first 10 dB; it is what the ear notices most. If EDT is much shorter than T20 the room has a lot of direct sound and few late echoes."),
    (Body, "C50 and C80: clarity in dB, how much louder the early sound (the first 50 or 80 ms) is than the late reverb. Above 0 dB, speech (C50) or music (C80) is clear; far below 0, it is muddy. D50: the share, 0 to 1, of the energy that arrives in the first 50 ms; 0.5 or more means good speech clarity."),
    (Body, "DR: dynamic range, how far above the background noise the pop was, in dB. T20 needs at least 35 dB and T30 at least 45 dB; with less, the value is left blank (--) instead of showing a wrong one. To raise DR: pop closer, louder or in a quieter room."),
    (Head, "Typical reverberation times (mid frequencies)"),
    (Body, "Recording studio or dry living room: 0.2 to 0.4 s. Ordinary living room, office: 0.4 to 0.6 s. Classroom or lecture room for speech: 0.4 to 0.8 s. Big hall for speech: up to 1.2 s. Concert hall for music: 1.5 to 2.2 s. Large church: 2 to 6 s. Too long for speech makes words run together; too short makes music dull."),
    (Head, "Bands and frequencies"),
    (Body, "The sound is split into frequency bands and each is measured by itself, because rooms treat bass and treble differently. 63 Hz is deep bass, 125 to 250 Hz the body of voices, 500 Hz to 2 kHz speech, 4 to 8 kHz the sparkle of s and cymbals. b switches between octave bands (8 of them) and third-octave bands (more detail, more noise). The numbers under each plot are the band centres in Hz; 1k means 1000 Hz."),
    (Body, "The band you have selected (Left and Right, or tap the plot) is the one the third line shows; 'all' is the whole sound together."),
    (Head, "View: summary"),
    (Body, "Reverberation time (up) against frequency (across), one dot per band, three lines: EDT, T20 and T30. A flat line means an even room. Rising to the left means a boomy room: bass lingers more than treble (typical with hard walls and few soft things). Falling to the right is normal in rooms with air and soft furnishings. Lines that disagree widely or jump around mean a noisy or too short recording: measure again."),
    (Head, "View: waterfall"),
    (Body, "A mountain range: frequency runs along the front, and each line further back is a later moment, with the impulse at the very back and the end of the recording in front. Height is level, from 0 dB down to -60 dB. A fast-dying band collapses quickly toward the front; a band that stays high on the way forward is ringing. Ridges that stand out in one place are room resonances, where bass notes will boom."),
    (Head, "View: heat"),
    (Body, "The same data as a coloured map seen from above: frequency across, time downward, from the impulse at the top to the end of the recording at the bottom. Bright is loud, dark is quiet. A band whose colour stays bright far down decays slowly. Vertical bright stripes are resonances. Everything turning dark at the same time down the page means an even room."),
    (Head, "View: decay"),
    (Body, "One band's own decay curve: level (down) against time (across). The jagged curve is the measured fade and the straight line is the fit the T20 value comes from. If the curve is nearly straight along the line the number can be trusted. A curve that bends, or falls off a cliff and then flattens, means a low dynamic range or a noisy room. Use Left and Right to look at each band."),
    (Head, "Keys and buttons"),
    (Body, "Space arm, start or stop. m balloon or sweep. b octave or third bands. v next view. Left and Right pick a band, or tap the plot. Up and Down trigger or output level. n recording length. , or . input channel. c clear. s save: NAME.csv (the numbers), NAME.wav (the room's impulse response) and NAME.png (this screen) on the first USB stick. k colour theme. h this help, press again for the next page. Esc quits."),
];

/// Lays the text out for this screen and calls `visit(page, row, style, text)` for every wrapped line; returns the page count.
fn walk(layout: &Layout, mut visit: impl FnMut(usize, usize, Style, &str)) -> (usize, usize) {
    let columns = ((layout.plot_w as i32 - 16) / 8).max(20) as usize;
    // the last row of a page is left for the footer
    let per_page = ((layout.plot_h as i32 - 8) / LINE_H - 1).max(6) as usize;
    let mut line = 0usize;
    for (index, &(style, text)) in TEXT.iter().enumerate() {
        if style == Head && index > 0 && line % per_page != 0 {
            // a blank line before a heading, and no heading alone at the foot of a page
            line += if per_page - line % per_page < 4 { per_page - line % per_page } else { 1 };
        }
        let mut rest = text;
        while !rest.is_empty() {
            let mut end = rest.len().min(columns);
            if end < rest.len() {
                end = rest[..=end].rfind(' ').unwrap_or(end);
            }
            visit(line / per_page, line % per_page, style, rest[..end].trim_end());
            line += 1;
            rest = rest[end..].trim_start();
        }
    }
    ((line + per_page - 1) / per_page, per_page)
}

/// How many pages the help takes at this screen size.
pub fn pages(layout: &Layout) -> usize {
    walk(layout, |_, _, _, _| {}).0
}

/// Draws page `page` (0-based) in the plot area.
pub fn draw(canvas: &mut Canvas, layout: &Layout, page: usize) {
    let x0 = layout.left + 8;
    canvas.rect(layout.left, layout.top, layout.plot_w, layout.plot_h, theme().grid_major);
    let (pages, per_page) = walk(layout, |at, row, style, text| {
        if at == page {
            let colour = if style == Head { theme().green } else { theme().text };
            canvas.text_scaled(x0, layout.top + 4 + row as i32 * LINE_H, text, 1, colour);
        }
    });
    let mut footer = Line::<48>::new();
    let _ = write!(footer, "help {}/{}   h: next page, any other button: back", page + 1, pages);
    canvas.text_scaled(x0, layout.top + 4 + per_page as i32 * LINE_H, footer.as_str(), 1, theme().dim);
}
