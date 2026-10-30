//! Checks the file-range call (tape ABI 29, `fs_read_at`): `tape run fscheck NAME OFFSET LENGTH` reads that range of a
//! file without loading the rest and prints it. The tape for `make test-fs-read-at` (`tools/test_fs_read_at.py`).
#![no_std]
#![no_main]

use tape_rt::{TapeApi, println};

tape_rt::tape_help!(br#"fscheck: read a range of a file with fs_read_at (ABI 29).
Usage: tape run fscheck NAME OFFSET LENGTH   (NAME: NAME, diskN/NAME or @LABEL/NAME)
Prints the bytes read; a count of 0 means the offset is at or past the end.
"#);

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    tape_rt::init(api);
    let arguments = core::str::from_utf8(tape_rt::args()).unwrap_or("");
    let mut words = arguments.split_whitespace();
    let (Some(name), Some(offset), Some(length)) = (words.next(), words.next().and_then(|word| word.parse::<u64>().ok()), words.next().and_then(|word| word.parse::<usize>().ok())) else {
        return println!("Usage: fscheck NAME OFFSET LENGTH");
    };
    let mut buffer = [0u8; 4096];
    let length = length.min(buffer.len());
    match tape_rt::read_file_at(name, offset, &mut buffer[..length]) {
        Ok(count) => {
            let text: &[u8] = &buffer[..count];
            println!("fscheck: {count} bytes [{}]", core::str::from_utf8(text).unwrap_or("(not text)"));
        }
        Err(code) => println!("fscheck: error {code}"),
    }
}
