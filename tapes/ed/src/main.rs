#![no_std]
#![no_main]

//! A deliberately small, line-oriented text editor tape.
//!
//! `tape run ed NAME` loads one complete FOSFS file through the high-level
//! filesystem callbacks, edits it in a 1 MiB-granular allocator-backed
//! buffer (up to 4 MiB), and writes it back only when the user chooses `w` or
//! `x`. It uses the same
//! non-blocking `read_key` input path as the calculator tape, so local
//! keyboard input and remote `term net` input need no separate code paths.
//! Full-screen editing is intentionally out of scope: this tape demonstrates
//! persistent tape-owned file access without duplicating terminal machinery.

extern crate ed_tape;

use core::slice;
use ed_tape::TextBuffer;
use tape_rt::{Key, KeyDecoder, print_bytes, print_u32, set_raw_keys};

const TAPE_EVENT_ABORT: u32 = 1;
const TAPE_EVENT_BYTE_BASE: u32 = 0x100;
const FS_OK: u32 = 0;
const FS_NOT_FOUND: u32 = 1;
const FS_BUFFER_TOO_SMALL: u32 = 3;
const FS_DIRECTORY_FULL: u32 = 5;
const FS_DATA_FULL: u32 = 6;
const EDITOR_MIN_CAPACITY: u32 = 1024 * 1024;
const EDITOR_MAX_CAPACITY: u32 = 4 * 1024 * 1024;
const INPUT_CAPACITY: usize = 256;
const PAGE_LINES: usize = 20;
/// Longest line the cursor editor takes (longer ones: `cLINE/OLD/NEW`).
const EDIT_CAPACITY: usize = 1024;

// Small files must work on the default `make run` QEMU configuration, whose
// RAM ends at the big allocator's 128 MiB starting address. Keeping the
// common 1 MiB case in the tape's own fixed 4 MiB image region avoids making
// basic editing depend on extra machine RAM.
static mut EDITOR_BUFFER: [u8; EDITOR_MIN_CAPACITY as usize] =
    [0; EDITOR_MIN_CAPACITY as usize];

type Putc = extern "C" fn(u32);
type ReadKey = fn() -> u32;
type FsWrite = fn(usize, u32, usize, u32) -> u32;

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static tape_rt::TapeApi) {
    tape_rt::init(api);
    let putc: Putc = api.putc;
    let read_key: ReadKey = tape_rt::compat::read_key;
    let fs_file_length = tape_rt::compat::fs_file_length;
    let fs_read = tape_rt::compat::fs_read;
    let fs_write: FsWrite = tape_rt::compat::fs_write;
    let alloc = tape_rt::compat::alloc;
    let free = tape_rt::compat::free;
    let (args_ptr, args_len) = (api.args as usize, api.args_len as u32);
    let Some(name) = parse_filename(args_ptr, args_len) else {
        print_bytes(putc, b"Usage: tape run ed NAME (or diskN/NAME, @LABEL/NAME for another volume)\n");
        return;
    };
    let mut resolved = [0u8; 256];
    let name = tape_rt::resolve_path(name, &mut resolved);

    let mut file_length = 0u64;
    let length_status = fs_file_length(name.as_ptr() as usize, name.len() as u32, &mut file_length as *mut u64 as usize);
    if length_status == tape_rt::FS_BAD_NAME as u32 {
        print_bytes(putc, b"ed: bad file name (or volume): ");
        print_bytes(putc, name);
        print_bytes(putc, b"\n");
        return;
    }
    if length_status != FS_OK && length_status != FS_NOT_FOUND {
        print_bytes(putc, b"ed: cannot inspect file (status ");
        print_u32(putc, length_status);
        print_bytes(putc, b")\n");
        return;
    }
    if file_length > EDITOR_MAX_CAPACITY as u64 {
        print_bytes(putc, b"ed: file exceeds the 4 MiB editor buffer\n");
        return;
    }

    let capacity = file_length
        .max(u64::from(EDITOR_MIN_CAPACITY))
        .div_ceil(u64::from(EDITOR_MIN_CAPACITY))
        .saturating_mul(u64::from(EDITOR_MIN_CAPACITY)) as u32;
    let uses_static_buffer = capacity == EDITOR_MIN_CAPACITY;
    let address = if uses_static_buffer {
        core::ptr::addr_of_mut!(EDITOR_BUFFER) as *mut u8 as usize
    } else {
        alloc(capacity)
    };
    if address == 0 {
        print_bytes(putc, b"ed: unable to allocate the ");
        print_u32(putc, capacity / (1024 * 1024));
        print_bytes(putc, b" MiB editor buffer\n");
        return;
    }

    let result = unsafe {
        let storage = slice::from_raw_parts_mut(address as *mut u8, capacity as usize);
        let mut text = TextBuffer::new(storage);
        if length_status == FS_OK {
            let mut read_length = 0u32;
            let status = fs_read(
                name.as_ptr() as usize,
                name.len() as u32,
                address,
                capacity,
                &mut read_length as *mut u32 as usize,
            );
            if status != FS_OK {
                print_bytes(putc, b"ed: cannot read file (status ");
                print_u32(putc, status);
                print_bytes(putc, b")\n");
                if !uses_static_buffer {
                    free(address, capacity);
                }
                return;
            }
            if text
                .load(slice::from_raw_parts(address as *const u8, read_length as usize))
                .is_err()
            {
                print_bytes(putc, b"ed: file does not fit after line-ending normalization\n");
                if !uses_static_buffer {
                    free(address, capacity);
                }
                return;
            }
        }
        run_editor(
            putc,
            read_key,
            name,
            &mut text,
            fs_write,
        )
    };
    if !uses_static_buffer {
        free(address, capacity);
    }
    if result {
        print_bytes(putc, b"ed: saved and exited\n");
    } else {
        print_bytes(putc, b"ed: exited without saving\n");
    }
}

fn parse_filename<'a>(args_ptr: usize, args_len: u32) -> Option<&'a [u8]> {
    if args_ptr == 0 || args_len == 0 {
        return None;
    }
    let args: &'a [u8] = unsafe { slice::from_raw_parts(args_ptr as *const u8, args_len as usize) };
    let mut words = args
        .split(|byte| *byte == b' ')
        .map(trim_ascii)
        .filter(|word| !word.is_empty());
    // `tape run ed NAME` keeps its leading program word, while a bare-name run
    // (`ed NAME`, see `shell::run_external_command`) hands the tape only NAME.
    // Both work; a file literally named `ed` must be written `ed ed`.
    let first = words.next()?;
    let name = if first == b"ed" { words.next()? } else { first };
    // Only the shape of the command line is checked: which names are valid depends on the volume (FOSFS v3 is lowercase and
    // 16 characters, v4 and ext are not), so the kernel decides and `ed` reports its answer.
    if words.next().is_some() {
        return None;
    }
    Some(name)
}

fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = bytes.len();
    while start < end && bytes[start].is_ascii_whitespace() {
        start += 1;
    }
    while end > start && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    &bytes[start..end]
}

fn run_editor(
    putc: Putc,
    read_key: ReadKey,
    name: &[u8],
    text: &mut TextBuffer<'_>,
    fs_write: FsWrite,
) -> bool {
    print_bytes(putc, b"\ned: ");
    print_bytes(putc, name);
    if text.len() == 0 {
        print_bytes(putc, b" (new file)\n");
    } else {
        print_bytes(putc, b" (");
        print_u32(putc, text.line_count() as u32);
        print_bytes(putc, b" lines)\n");
    }
    print_bytes(putc, b"Type h for help.\n");

    let mut input = [0u8; INPUT_CAPACITY];
    loop {
        print_bytes(putc, b"ed> ");
        let Some(length) = read_line(putc, read_key, &mut input) else {
            return false;
        };
        if length == 0 {
            continue;
        }
        match dispatch(putc, read_key, name, text, fs_write, &input[..length]) {
            CommandResult::Continue => {}
            CommandResult::Saved => return true,
            CommandResult::Quit => return false,
        }
    }
}

enum CommandResult {
    Continue,
    Saved,
    Quit,
}

fn dispatch(
    putc: Putc,
    read_key: ReadKey,
    name: &[u8],
    text: &mut TextBuffer<'_>,
    fs_write: FsWrite,
    command: &[u8],
) -> CommandResult {
    match command {
        b"h" | b"help" => {
            print_bytes(
                putc,
                b"p print | m more-style print | a append | iN insert | dN delete\n\
                  sN/TEXT replace | eN edit | cN/OLD/NEW change | w write | x write+exit | q quit\n",
            );
        }
        b"p" => print_buffer(putc, text),
        b"m" => {
            if print_buffer_more(putc, read_key, text) {
                return CommandResult::Quit;
            }
        }
        b"a" => {
            if append_lines(putc, read_key, text, text.line_count() + 1) {
                return CommandResult::Quit;
            }
        }
        b"w" => {
            save(putc, name, text, fs_write);
        }
        b"x" => {
            if save(putc, name, text, fs_write) {
                return CommandResult::Saved;
            }
        }
        b"q" => return CommandResult::Quit,
        _ if command.first() == Some(&b'i') => {
            let Some(line) = parse_number(&command[1..]) else {
                print_bytes(putc, b"ed: use iLINE\n");
                return CommandResult::Continue;
            };
            if append_lines(putc, read_key, text, line) {
                return CommandResult::Quit;
            }
        }
        _ if command.first() == Some(&b'a') => {
            let Some(line) = parse_number(&command[1..]) else {
                print_bytes(putc, b"ed: use aLINE\n");
                return CommandResult::Continue;
            };
            let Some(insert_line) = line.checked_add(1) else {
                print_bytes(putc, b"ed: invalid line number\n");
                return CommandResult::Continue;
            };
            if append_lines(putc, read_key, text, insert_line) {
                return CommandResult::Quit;
            }
        }
        _ if command.first() == Some(&b'd') => {
            let Some(line) = parse_number(&command[1..]) else {
                print_bytes(putc, b"ed: use dLINE\n");
                return CommandResult::Continue;
            };
            if text.delete_line(line).is_err() {
                print_bytes(putc, b"ed: no such line\n");
            }
        }
        _ if command.first() == Some(&b'e') => {
            let Some(line) = parse_number(&command[1..]) else {
                print_bytes(putc, b"ed: use eLINE\n");
                return CommandResult::Continue;
            };
            if edit_line(putc, read_key, text, line) {
                return CommandResult::Quit;
            }
        }
        _ if command.first() == Some(&b'c') => change_in_line(putc, text, &command[1..]),
        _ if command.first() == Some(&b's') => {
            if substitute(putc, read_key, text, &command[1..]) {
                return CommandResult::Quit;
            }
        }
        _ => print_bytes(putc, b"ed: unknown command; use h for help\n"),
    }
    CommandResult::Continue
}

fn read_line(putc: Putc, read_key: ReadKey, output: &mut [u8]) -> Option<usize> {
    let mut length = 0;
    loop {
        let event = read_key();
        if event == 0 {
            tape_rt::idle();
            continue;
        }
        if event == TAPE_EVENT_ABORT {
            print_bytes(putc, b"\n");
            return None;
        }
        if event < TAPE_EVENT_BYTE_BASE {
            continue;
        }
        let byte = (event - TAPE_EVENT_BYTE_BASE) as u8;
        match byte {
            b'\r' | b'\n' => {
                putc(u32::from(b'\n'));
                return Some(length);
            }
            8 | 127 => {
                // v1's kernel erased the character on screen itself; on v2 the tape does.
                if length > 0 {
                    length -= 1;
                    print_bytes(putc, b"\x08 \x08");
                }
            }
            32..=126 => {
                if length < output.len() {
                    output[length] = byte;
                    length += 1;
                    putc(u32::from(byte));
                } else {
                    print_bytes(putc, b"\ninput too long\n");
                    return Some(0);
                }
            }
            _ => {}
        }
    }
}

/// Moves the terminal cursor with an ANSI sequence: `ESC [ n D` (left) or
/// `ESC [ n C` (right). The kernel's VGA console and a serial/`term net`
/// terminal both implement these.
fn cursor_move(putc: Putc, count: usize, direction: u8) {
    if count == 0 {
        return;
    }
    putc(0x1b);
    putc(u32::from(b'['));
    print_u32(putc, count as u32);
    putc(u32::from(direction));
}

/// Reads one line with a real cursor: `initial` is pre-filled and editable with
/// Left/Right, Home/End (or Ctrl-A/Ctrl-E), Backspace and Delete; typed
/// characters insert at the cursor. The tape asks the kernel for raw keys so
/// it controls every character on screen. After each key the one screen row
/// is redrawn from its start: a line longer than the row scrolls sideways
/// (`<` and `>` mark the hidden ends), because the cursor-left sequence does
/// not wrap to the row above.
fn read_line_cursor(
    putc: Putc,
    read_key: ReadKey,
    prompt: &[u8],
    output: &mut [u8],
    initial: &[u8],
) -> Option<usize> {
    let mut length = initial.len().min(output.len());
    output[..length].copy_from_slice(&initial[..length]);
    let mut cursor = length;
    let columns = tape_rt::console_size().0;
    // One column is left free so that a full row never makes the console wrap.
    let width = columns.saturating_sub(prompt.len() + 1).max(8);
    let mut first = 0usize;
    redraw_row(putc, prompt, &output[..length], cursor, width, &mut first);
    let mut decoder = KeyDecoder::new();
    set_raw_keys(true);
    let result = loop {
        let event = read_key();
        if event == 0 {
            tape_rt::idle();
            continue;
        }
        if event == TAPE_EVENT_ABORT {
            print_bytes(putc, b"\n");
            break None;
        }
        if event < TAPE_EVENT_BYTE_BASE {
            continue;
        }
        let Some(key) = decoder.feed((event - TAPE_EVENT_BYTE_BASE) as u8) else {
            continue;
        };
        match key {
            Key::Enter => {
                putc(u32::from(b'\n'));
                break Some(length);
            }
            Key::Left if cursor > 0 => cursor -= 1,
            Key::Right if cursor < length => cursor += 1,
            Key::Home | Key::Control(1) => cursor = 0,
            Key::End | Key::Control(5) => cursor = length,
            Key::Backspace if cursor > 0 => {
                output.copy_within(cursor..length, cursor - 1);
                cursor -= 1;
                length -= 1;
            }
            Key::Delete if cursor < length => {
                output.copy_within(cursor + 1..length, cursor);
                length -= 1;
            }
            Key::Char(byte) if length < output.len() => {
                output.copy_within(cursor..length, cursor + 1);
                output[cursor] = byte;
                length += 1;
                cursor += 1;
            }
            _ => continue,
        }
        redraw_row(putc, prompt, &output[..length], cursor, width, &mut first);
    };
    set_raw_keys(false);
    result
}

/// Redraws the prompt and the part of `line` that fits `width` columns, then
/// puts the cursor where `cursor` is. `first` is the leftmost visible byte; it
/// only moves when the cursor would leave the window.
fn redraw_row(putc: Putc, prompt: &[u8], line: &[u8], cursor: usize, width: usize, first: &mut usize) {
    if cursor < *first {
        *first = cursor;
    } else if cursor >= *first + width {
        *first = cursor + 1 - width;
    }
    let left_hidden = *first > 0;
    let right_hidden = line.len() > *first + width;
    // A marker takes the place of the edge character, so it is never the cursor's.
    let mut start = *first;
    let mut end = (*first + width).min(line.len());
    if left_hidden && cursor > start {
        start += 1;
    }
    if right_hidden && cursor < end {
        end -= 1;
    }
    putc(u32::from(b'\r'));
    print_bytes(putc, prompt);
    if left_hidden && cursor > *first {
        putc(u32::from(b'<'));
    }
    print_bytes(putc, &line[start..end]);
    if right_hidden && cursor < end + 1 {
        putc(u32::from(b'>'));
    }
    putc(0x1b);
    print_bytes(putc, b"[K");
    putc(u32::from(b'\r'));
    cursor_move(putc, prompt.len() + (cursor - *first), b'C');
}

fn append_lines(
    putc: Putc,
    read_key: ReadKey,
    text: &mut TextBuffer<'_>,
    mut line: usize,
) -> bool {
    print_bytes(putc, b"Enter lines; finish with a single '.'\n");
    let mut input = [0u8; INPUT_CAPACITY];
    loop {
        print_bytes(putc, b". ");
        let Some(length) = read_line(putc, read_key, &mut input) else {
            return true;
        };
        if input[..length] == *b"." {
            return false;
        }
        if text.insert_line(line, &input[..length]).is_err() {
            print_bytes(putc, b"ed: editor buffer full or invalid line\n");
            return false;
        }
        line += 1;
    }
}

fn edit_line(putc: Putc, read_key: ReadKey, text: &mut TextBuffer<'_>, line: usize) -> bool {
    let Some(current) = text.line_content(line) else {
        print_bytes(putc, b"ed: no such line\n");
        return false;
    };
    // The old text is pre-filled after the prompt and edited with the cursor
    // keys; Enter keeps whatever is on screen.
    let mut input = [0u8; EDIT_CAPACITY];
    if current.len() > EDIT_CAPACITY {
        print_bytes(putc, b"ed: line too long to edit; use cLINE/OLD/NEW\n");
        return false;
    }
    let Some(length) = read_line_cursor(putc, read_key, b"edit: ", &mut input, current) else {
        return true;
    };
    if text.replace_line(line, &input[..length]).is_err() {
        print_bytes(putc, b"ed: replacement failed\n");
    }
    false
}

/// `cLINE/OLD/NEW`: replaces the first occurrence of OLD in the line with NEW
/// (classic ed/sed substitution), for changing the middle of a line without
/// retyping the rest of it.
fn change_in_line(putc: Putc, text: &mut TextBuffer<'_>, argument: &[u8]) {
    static mut SCRATCH: [u8; 1024] = [0; 1024];
    let parsed = argument
        .iter()
        .position(|byte| *byte == b'/')
        .and_then(|first| {
            let rest = &argument[first + 1..];
            let second = rest.iter().position(|byte| *byte == b'/')?;
            Some((parse_number(&argument[..first])?, &rest[..second], &rest[second + 1..]))
        });
    let Some((line, old, new)) = parsed else {
        print_bytes(putc, b"ed: use cLINE/OLD/NEW\n");
        return;
    };
    let Some(current) = text.line_content(line) else {
        print_bytes(putc, b"ed: no such line\n");
        return;
    };
    let position = if old.is_empty() {
        None
    } else {
        current.windows(old.len()).position(|window| window == old)
    };
    let Some(position) = position else {
        print_bytes(putc, b"ed: text not found in that line\n");
        return;
    };
    let scratch = unsafe { &mut *core::ptr::addr_of_mut!(SCRATCH) };
    let total = current.len() - old.len() + new.len();
    if total > scratch.len() {
        print_bytes(putc, b"ed: resulting line too long\n");
        return;
    }
    scratch[..position].copy_from_slice(&current[..position]);
    scratch[position..position + new.len()].copy_from_slice(new);
    scratch[position + new.len()..total].copy_from_slice(&current[position + old.len()..]);
    if text.replace_line(line, &scratch[..total]).is_err() {
        print_bytes(putc, b"ed: replacement failed\n");
        return;
    }
    print_u32(putc, line as u32);
    print_bytes(putc, b": ");
    print_bytes(putc, &scratch[..total]);
    putc(u32::from(b'\n'));
}

fn substitute(
    putc: Putc,
    read_key: ReadKey,
    text: &mut TextBuffer<'_>,
    argument: &[u8],
) -> bool {
    let Some(line_end) = argument.iter().position(|byte| *byte == b'/') else {
        let Some(line) = parse_number(argument) else {
            print_bytes(putc, b"ed: use sLINE/TEXT or sLINE\n");
            return false;
        };
        return edit_line(putc, read_key, text, line);
    };
    let Some(line) = parse_number(&argument[..line_end]) else {
        print_bytes(putc, b"ed: invalid line number\n");
        return false;
    };
    let replacement = &argument[line_end + 1..];
    if text.replace_line(line, replacement).is_err() {
        print_bytes(putc, b"ed: replacement failed\n");
    }
    false
}

fn print_buffer(putc: Putc, text: &TextBuffer<'_>) {
    for line in 1..=text.line_count() {
        print_u32(putc, line as u32);
        print_bytes(putc, b": ");
        if let Some(content) = text.line_content(line) {
            print_bytes(putc, content);
        }
        putc(u32::from(b'\n'));
    }
}

fn print_buffer_more(putc: Putc, read_key: ReadKey, text: &TextBuffer<'_>) -> bool {
    let line_count = text.line_count();
    let mut printed = 0;
    for line in 1..=line_count {
        print_u32(putc, line as u32);
        print_bytes(putc, b": ");
        if let Some(content) = text.line_content(line) {
            print_bytes(putc, content);
        }
        putc(u32::from(b'\n'));
        printed += 1;
        if printed == PAGE_LINES && line < line_count {
            match wait_for_more(putc, read_key) {
                MoreAction::NextPage => printed = 0,
                MoreAction::Stop => return false,
                MoreAction::Abort => return true,
            }
        }
    }
    false
}

enum MoreAction {
    NextPage,
    Stop,
    Abort,
}

fn wait_for_more(putc: Putc, read_key: ReadKey) -> MoreAction {
    print_bytes(putc, b"-- more -- (space: next page, q: stop) ");
    loop {
        let event = read_key();
        if event == TAPE_EVENT_ABORT {
            putc(u32::from(b'\n'));
            return MoreAction::Abort;
        }
        if event < TAPE_EVENT_BYTE_BASE {
            tape_rt::idle();
            continue;
        }
        match (event - TAPE_EVENT_BYTE_BASE) as u8 {
            b'q' | b'Q' => {
                putc(u32::from(b'\n'));
                return MoreAction::Stop;
            }
            b' ' | b'\r' | b'\n' => {
                putc(u32::from(b'\n'));
                return MoreAction::NextPage;
            }
            _ => {}
        }
    }
}

fn save(putc: Putc, name: &[u8], text: &TextBuffer<'_>, fs_write: FsWrite) -> bool {
    let status = fs_write(
        name.as_ptr() as usize,
        name.len() as u32,
        text.as_bytes().as_ptr() as usize,
        text.len() as u32,
    );
    if status == FS_OK {
        print_bytes(putc, b"written ");
        print_bytes(putc, name);
        print_bytes(putc, b"\n");
        true
    } else {
        print_bytes(putc, b"ed: write failed (status ");
        print_u32(putc, status);
        match status {
            FS_BUFFER_TOO_SMALL => print_bytes(putc, b", buffer too small"),
            FS_DIRECTORY_FULL => print_bytes(putc, b", directory full"),
            FS_DATA_FULL => print_bytes(putc, b", data full"),
            _ => {}
        }
        print_bytes(putc, b")\n");
        false
    }
}

fn parse_number(bytes: &[u8]) -> Option<usize> {
    if bytes.is_empty() {
        return None;
    }
    let mut value = 0usize;
    for byte in bytes {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(usize::from(*byte - b'0'))?;
    }
    Some(value)
}

tape_rt::tape_help!(br#"ed: small line-oriented text editor (see `ebb` for the full-screen one).

  ed NAME           edit the file NAME (up to 4 MiB); w or x writes it back

Commands are read from the keyboard or the term net session.
"#);
