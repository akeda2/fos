#![no_std]
#![no_main]

//! `tape run ebb NAME [tab=N]`: the full-screen text editor. See `lib.rs` for the editor itself;
//! this file loads the file, drives the console (cursor positioning and SGR attributes, `ESC [ ...`)
//! and turns key and pointer events into editor events.

extern crate ebb_tape;

use ebb_tape::{Action, BLANK, Cell, DIM, Decoder, Editor, Event, Grid, NORMAL, POINTER, REVERSE};
use edit_core::{Buffer, Doc};
use tape_rt::{FS_NOT_FOUND, KEY_ABORT, KEY_BASE, TapeApi, println};

tape_rt::tape_help!(
    br#"ebb - a small full-screen text editor

  tape run ebb NAME [tab=N]

NAME is a file on the boot volume, or diskN/NAME, @LABEL/NAME. A file that
does not exist yet is created on the first save. The file is read whole into
memory and written whole by Ctrl-S.

  Ctrl-S save        Ctrl-Q quit (twice to discard changes)   Ctrl-G help
  Ctrl-Z undo        Ctrl-Y redo
  Ctrl-C/X/V         copy/cut/paste; with no selection a whole line
  Ctrl-A select all  Ctrl-T select mode (cursor keys select)
  mouse: click, drag to select
"#
);

const TEXT_SPARE: usize = 256 * 1024;
const LOG_SIZE: usize = 256 * 1024;
const CLIP_SIZE: usize = 256 * 1024;
const MAX_FILE: usize = 32 * 1024 * 1024;

fn out(byte: u8) {
    (tape_rt::api().putc)(u32::from(byte));
}

fn out_str(text: &str) {
    text.bytes().for_each(out);
}

fn out_num(mut n: usize) {
    let mut digits = [0u8; 20];
    let mut count = 0;
    loop {
        digits[count] = b'0' + (n % 10) as u8;
        count += 1;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    digits[..count].iter().rev().for_each(|&d| out(d));
}

fn goto(col: usize, row: usize) {
    out_str("\x1b[");
    out_num(row + 1);
    out(b';');
    out_num(col + 1);
    out(b'H');
}

fn set_attr(attr: u8) {
    out_str(match attr {
        DIM => "\x1b[0;90m",
        REVERSE => "\x1b[0;7m",
        POINTER => "\x1b[0;30;46m",
        _ => "\x1b[0m",
    });
}

/// Sends only the cells that differ from what is already on screen.
fn flush(grid: &Grid, shown: &mut [Cell], caret: (usize, usize)) {
    let mut at: Option<(usize, usize)> = None;
    let mut attr = 255u8;
    out_str("\x1b[?25l");
    for row in 0..grid.rows {
        for col in 0..grid.cols {
            let i = row * grid.cols + col;
            let cell = grid.cells[i];
            if cell == shown[i] {
                continue;
            }
            shown[i] = cell;
            if at != Some((col, row)) {
                goto(col, row);
            }
            if cell.attr != attr {
                set_attr(cell.attr);
                attr = cell.attr;
            }
            let mut raw = [0u8; 4];
            cell.ch.encode_utf8(&mut raw).bytes().for_each(out);
            at = Some((col + 1, row));
        }
    }
    set_attr(NORMAL);
    goto(caret.0, caret.1);
    out_str("\x1b[?25h");
}

fn alloc_slice(api: &TapeApi, bytes: usize) -> Option<&'static mut [u8]> {
    tape_rt::alloc_bytes(api, bytes.max(4096))
}

fn alloc_cells(api: &TapeApi, count: usize) -> Option<&'static mut [Cell]> {
    let address = (api.alloc)(count * core::mem::size_of::<Cell>());
    if address == 0 {
        return None;
    }
    let cells = unsafe { core::slice::from_raw_parts_mut(address as *mut Cell, count) };
    cells.fill(BLANK);
    Some(cells)
}

/// Reads a whole file into freshly allocated memory with `TEXT_SPARE` or more room to edit in.
/// Returns the buffer and the length read; a file that does not exist is an empty buffer.
fn load(api: &'static TapeApi, name: &str) -> Result<(&'static mut [u8], usize, bool), &'static str> {
    let mut size = 1024 * 1024;
    loop {
        let probe = alloc_slice(api, size).ok_or("out of memory")?;
        let got = (api.fs_read)(name.as_ptr(), name.len(), probe.as_mut_ptr(), probe.len());
        if got == tape_rt::FS_TOO_BIG && size < MAX_FILE {
            (api.free)(probe.as_mut_ptr() as usize, probe.len());
            size *= 4;
            continue;
        }
        let (len, exists) = match got {
            n if n >= 0 => (n as usize, true),
            FS_NOT_FOUND => (0, false),
            tape_rt::FS_TOO_BIG => return Err("file too big"),
            tape_rt::FS_BAD_NAME => return Err("bad file name"),
            tape_rt::FS_NO_VOLUME => return Err("no such volume"),
            _ => return Err("cannot read the file"),
        };
        let capacity = len * 2 + TEXT_SPARE;
        let text = alloc_slice(api, capacity).ok_or("out of memory")?;
        text[..len].copy_from_slice(&probe[..len]);
        (api.free)(probe.as_mut_ptr() as usize, probe.len());
        return Ok((text, len, exists));
    }
}

/// If every line break is CRLF, drops the CRs (kept in mind, put back on save). Returns the new length.
fn strip_crlf(text: &mut [u8], len: usize) -> Option<usize> {
    let (mut crlf, mut lf) = (0, 0);
    for i in 0..len {
        if text[i] == b'\n' {
            if i > 0 && text[i - 1] == b'\r' {
                crlf += 1;
            } else {
                lf += 1;
            }
        }
    }
    if crlf == 0 || lf > 0 {
        return None;
    }
    let mut w = 0;
    for r in 0..len {
        if !(text[r] == b'\r' && r + 1 < len && text[r + 1] == b'\n') {
            text[w] = text[r];
            w += 1;
        }
    }
    Some(w)
}

fn save(api: &'static TapeApi, editor: &mut Editor) -> Result<usize, &'static str> {
    let name_len = editor.name_len;
    let mut name = [0u8; 128];
    name[..name_len].copy_from_slice(editor.name());
    let crlf = editor.crlf;
    let bytes = editor.doc.buf.contiguous();
    let status = if crlf {
        let extra = bytes.iter().filter(|&&b| b == b'\n').count();
        let Some(tmp) = alloc_slice(api, bytes.len() + extra) else { return Err("out of memory") };
        let mut w = 0;
        for &b in bytes {
            if b == b'\n' {
                tmp[w] = b'\r';
                w += 1;
            }
            tmp[w] = b;
            w += 1;
        }
        let status = (api.fs_write)(name.as_ptr(), name_len, tmp.as_ptr(), w);
        (api.free)(tmp.as_ptr() as usize, tmp.len());
        status
    } else {
        (api.fs_write)(name.as_ptr(), name_len, bytes.as_ptr(), bytes.len())
    };
    match status {
        0 => Ok(bytes.len()),
        tape_rt::FS_TOO_BIG => Err("no room on the volume"),
        tape_rt::FS_BAD_NAME => Err("bad file name"),
        tape_rt::FS_NO_VOLUME => Err("no such volume"),
        _ => Err("write failed"),
    }
}

fn pointer_cell(columns: usize, rows: usize) -> Option<(usize, usize, bool, bool)> {
    let api = tape_rt::api();
    let mut last = None;
    while let Some(event) = tape_rt::touch_event() {
        if api.version < 24 {
            break;
        }
        let extent = (api.touch_info)(event.panel);
        let (max_x, max_y) = ((extent >> 32) as i64 + 1, (extent & 0xffff_ffff) as i64 + 1);
        if extent == 0 {
            continue;
        }
        let col = (i64::from(event.x).max(0) * columns as i64 / max_x) as usize;
        let row = (i64::from(event.y).max(0) * rows as i64 / max_y) as usize;
        last = Some((col.min(columns - 1), row.min(rows - 1), event.down, true));
    }
    last
}

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    tape_rt::init(api);
    let args = core::str::from_utf8(tape_rt::args()).unwrap_or("");
    let mut name = "";
    let mut tab = 4usize;
    for word in args.split_whitespace() {
        if let Some(value) = word.strip_prefix("tab=") {
            tab = value.parse().unwrap_or(4).clamp(1, 16);
        } else if name.is_empty() {
            name = word;
        }
    }
    if name.is_empty() {
        println!("usage: tape run ebb NAME [tab=N]   (help: ebb --help)");
        return;
    }
    let (columns, rows) = tape_rt::console_size();
    let (columns, rows) = (columns.clamp(20, 256), rows.clamp(4, 100));

    let (text, len, exists) = match load(api, name) {
        Ok(loaded) => loaded,
        Err(why) => {
            println!("ebb: {name}: {why}");
            return;
        }
    };
    let crlf_len = strip_crlf(text, len);
    let len = crlf_len.unwrap_or(len);
    let (Some(log), Some(clip), Some(cells), Some(shown)) =
        (alloc_slice(api, LOG_SIZE), alloc_slice(api, CLIP_SIZE), alloc_cells(api, columns * rows), alloc_cells(api, columns * rows))
    else {
        println!("ebb: out of memory");
        return;
    };
    let mut doc = Doc::new(Buffer::adopt(text, len), log, clip);
    doc.tab = tab;
    let mut editor = Editor::new(doc, columns, rows);
    editor.set_name(name.as_bytes());
    editor.crlf = crlf_len.is_some();
    if !exists {
        editor.say("new file");
    }

    out_str("\x1b[0m\x1b[2J");
    let mut grid = Grid { cols: columns, rows, cells };
    // The screen is blank, so what is "shown" starts as blank cells; a screen draw only sends changes.
    let mut decoder = Decoder::new();
    let mut dirty = true;
    'run: loop {
        let mut event = None;
        let key = (api.read_key)();
        if key == KEY_ABORT {
            event = Some(Event::Abort);
        } else if key >= KEY_BASE {
            if let Some(k) = decoder.feed((key - KEY_BASE) as u8) {
                event = Some(Event::Key(k));
            }
        } else if let Some((col, row, down, _)) = pointer_cell(columns, rows) {
            event = Some(Event::Mouse { col, row, down });
        }
        if let Some(event) = event {
            dirty = true;
            match editor.handle(event) {
                Action::Quit => break 'run,
                Action::Save => {
                    let result = save(api, &mut editor);
                    editor.saved(result);
                }
                Action::None => {}
            }
        } else if !dirty {
            tape_rt::idle();
        }
        if dirty {
            let caret = editor.render(&mut grid);
            flush(&grid, shown, caret);
            dirty = false;
        }
    }
    out_str("\x1b[0m\x1b[2J\x1b[H\x1b[?25h");
    println!("ebb: {}", core::str::from_utf8(editor.name()).unwrap_or("?"));
}
