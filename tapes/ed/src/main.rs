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
use tape_rt::{print_bytes, print_u32};

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

// Small files must work on the default `make run` QEMU configuration, whose
// RAM ends at the big allocator's 128 MiB starting address. Keeping the
// common 1 MiB case in the tape's own fixed 4 MiB image region avoids making
// basic editing depend on extra machine RAM.
static mut EDITOR_BUFFER: [u8; EDITOR_MIN_CAPACITY as usize] =
    [0; EDITOR_MIN_CAPACITY as usize];

type Putc = extern "C" fn(u32);
type ReadKey = extern "C" fn() -> u32;
type Alloc = extern "C" fn(u32) -> u32;
type Free = extern "C" fn(u32, u32);
type FsFileLength = extern "C" fn(u32, u32, u32) -> u32;
type FsRead = extern "C" fn(u32, u32, u32, u32, u32) -> u32;
type FsWrite = extern "C" fn(u32, u32, u32, u32) -> u32;

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(
    putc: Putc,
    _should_abort: extern "C" fn() -> u32,
    alloc: Alloc,
    free: Free,
    read_key: ReadKey,
    args_ptr: u32,
    args_len: u32,
    _nic_mac: extern "C" fn(u32) -> u32,
    _nic_send: extern "C" fn(u32, u32) -> u32,
    _nic_receive: extern "C" fn(u32, u32) -> u32,
    _nic_max_frame_len: extern "C" fn() -> u32,
    _ticks: extern "C" fn() -> u64,
    _nic_count: extern "C" fn() -> u32,
    _nic_mac_at: extern "C" fn(u32, u32) -> u32,
    _nic_send_at: extern "C" fn(u32, u32, u32) -> u32,
    _nic_receive_at: extern "C" fn(u32, u32, u32) -> u32,
    _nic_set_promiscuous_at: extern "C" fn(u32, u32) -> u32,
    _terminal_position: extern "C" fn() -> u32,
    fs_file_length: FsFileLength,
    fs_read: FsRead,
    fs_write: FsWrite,
    _fs_remove: extern "C" fn(u32, u32) -> u32,
) {
    let Some(name) = parse_filename(args_ptr, args_len) else {
        print_bytes(putc, b"Usage: tape run ed NAME\n");
        return;
    };

    let mut file_length = 0u64;
    let length_status = fs_file_length(name.as_ptr() as u32, name.len() as u32, &mut file_length as *mut u64 as u32);
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
        core::ptr::addr_of_mut!(EDITOR_BUFFER) as *mut u8 as u32
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
                name.as_ptr() as u32,
                name.len() as u32,
                address,
                capacity,
                &mut read_length as *mut u32 as u32,
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

fn parse_filename<'a>(args_ptr: u32, args_len: u32) -> Option<&'a [u8]> {
    if args_ptr == 0 || args_len == 0 {
        return None;
    }
    let args: &'a [u8] = unsafe { slice::from_raw_parts(args_ptr as *const u8, args_len as usize) };
    let mut words = args.split(|byte| *byte == b' ');
    let command = trim_ascii(words.next()?);
    let name = trim_ascii(words.next()?);
    if command != b"ed" || name.is_empty() || words.next().is_some() || !valid_name(name) {
        return None;
    }
    Some(name)
}

fn valid_name(name: &[u8]) -> bool {
    !name.is_empty()
        && name.len() <= 16
        && name.iter().all(|byte| {
            matches!(*byte, b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'_' | b'-')
        })
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
                b"p print | a append | iN insert | dN delete | sN/TEXT substitute\n\
                  eN edit | w write | x write+exit | q quit\n",
            );
        }
        b"p" => print_buffer(putc, text),
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
            8 => {
                length = length.saturating_sub(1);
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
    print_bytes(putc, b"old: ");
    print_bytes(putc, current);
    print_bytes(putc, b"\nnew: ");
    let mut input = [0u8; INPUT_CAPACITY];
    let Some(length) = read_line(putc, read_key, &mut input) else {
        return true;
    };
    if text.replace_line(line, &input[..length]).is_err() {
        print_bytes(putc, b"ed: replacement failed\n");
    }
    false
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

fn save(putc: Putc, name: &[u8], text: &TextBuffer<'_>, fs_write: FsWrite) -> bool {
    let status = fs_write(
        name.as_ptr() as u32,
        name.len() as u32,
        text.as_bytes().as_ptr() as u32,
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
