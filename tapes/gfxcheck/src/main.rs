//! Checks the tape graphics calls (`gfx_*`). `tape run gfxcheck [secs=N] [screen=N]` takes the screen, draws three colour bars, a red ramp
//! blitted from a wider buffer (the stride case) and a block that runs off the right and bottom edges (the clipping case),
//! prints `gfxcheck: drawn`, holds the picture for N seconds (default 10, Escape ends it) and returns; the kernel then gives the
//! screen back to the console. The tape for `make test-gfx` (`tools/test_gfx.py`), which reads the screen back from QEMU.
#![no_std]
#![no_main]

use tape_rt::{TapeApi, println};

const BARS_Y: u32 = 0;
const RAMP_Y: u32 = 100;
const RAMP_WIDTH: u32 = 256;
const RAMP_HEIGHT: u32 = 4;

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    tape_rt::init(api);
    let secs = core::str::from_utf8(tape_rt::args())
        .ok()
        .and_then(|text| text.split_whitespace().find_map(|word| word.strip_prefix("secs=")))
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(10);
    let screen = core::str::from_utf8(tape_rt::args())
        .ok()
        .and_then(|text| text.split_whitespace().find_map(|word| word.strip_prefix("screen=")))
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0);
    println!("gfxcheck: {} screen(s)", tape_rt::gfx_screens());
    let Some((width, height)) = tape_rt::gfx_info_on(screen) else {
        println!("gfxcheck: no pixel screen {screen}");
        return;
    };
    println!("gfxcheck: screen {width}x{height}");
    if !tape_rt::gfx_fill_on(screen, 0, 0, 8, 8, 0xffffff) {
        println!("gfxcheck: a fill before gfx_open is refused");
    }
    if !tape_rt::gfx_open_on(screen) {
        println!("gfxcheck: gfx_open failed");
        return;
    }
    tape_rt::gfx_fill_on(screen, 0, BARS_Y, 64, 64, 0xff0000);
    tape_rt::gfx_fill_on(screen, 80, BARS_Y, 64, 64, 0x00ff00);
    tape_rt::gfx_fill_on(screen, 160, BARS_Y, 64, 64, 0x0000ff);

    // 16 pixels per source row, 8 of them used: the pixel after each row's 8th must not be drawn.
    let mut wide = [0xffff_ffffu32; 16 * 4];
    for row in 0..4 {
        for column in 0..8 {
            wide[row * 16 + column] = 0x00ff00;
        }
    }
    tape_rt::gfx_blit_on(screen, 0, 70, 8, 4, &wide, 16);

    let mut ramp = [0u32; (RAMP_WIDTH * RAMP_HEIGHT) as usize];
    for row in 0..RAMP_HEIGHT as usize {
        for column in 0..RAMP_WIDTH as usize {
            ramp[row * RAMP_WIDTH as usize + column] = (column as u32) << 16;
        }
    }
    tape_rt::gfx_blit_on(screen, 0, RAMP_Y, RAMP_WIDTH, RAMP_HEIGHT, &ramp, RAMP_WIDTH);

    // Runs off the right and bottom edges: clipped, not wrapped onto the next row or written past the screen.
    let block = [0x00ff_00ffu32; 40 * 40];
    tape_rt::gfx_blit_on(screen, width - 10, height - 10, 40, 40, &block, 40);
    println!("gfxcheck: drawn");

    let until = tape_rt::micros() + secs * 1_000_000;
    while tape_rt::micros() < until && (tape_rt::api().should_abort)() == 0 {
        tape_rt::idle();
    }
    println!("gfxcheck: done");
}
