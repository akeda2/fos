//! Checks the file-range call (tape ABI 29, `fs_read_at`): `tape run fscheck NAME OFFSET LENGTH` reads that range of a
//! file without loading the rest and prints it. `fscheck streams NAME1 NAME2` (ABI 30) writes two files through two stream
//! handles at once and reads them back. The tape for `make test-fs-read-at` (`tools/test_fs_read_at.py`).
#![no_std]
#![no_main]

use tape_rt::{TapeApi, println};

tape_rt::tape_help!(br#"fscheck: read a range of a file with fs_read_at (ABI 29).
Usage: tape run fscheck NAME OFFSET LENGTH   (NAME: NAME, diskN/NAME or @LABEL/NAME)
Prints the bytes read; a count of 0 means the offset is at or past the end.
       tape run fscheck streams NAME1 NAME2   (two open streams, ABI 30; the files are 20000 and 19900 bytes)
       tape run fscheck vfs PATH              (ABI 32: stat PATH, list it if a folder, else read its first 40 bytes)
       tape run fscheck remove NAME           (create NAME, delete it with fs_remove, check it is gone)
"#);

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    tape_rt::init(api);
    let arguments = core::str::from_utf8(tape_rt::args()).unwrap_or("");
    let mut words = arguments.split_whitespace();
    if arguments.starts_with("streams ") {
        words.next();
        return match (words.next(), words.next()) {
            (Some(first), Some(second)) => streams(first, second),
            _ => println!("Usage: fscheck streams NAME1 NAME2"),
        };
    }
    if arguments.starts_with("vfs ") {
        words.next();
        return match words.next() {
            Some(path) => vfs(path),
            None => println!("Usage: fscheck vfs PATH"),
        };
    }
    if arguments.starts_with("remove ") {
        words.next();
        return match words.next() {
            Some(name) => remove(api, name),
            None => println!("Usage: fscheck remove NAME"),
        };
    }
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

/// Creates NAME, deletes it with fs_remove, and checks it is gone and that a second delete fails.
fn remove(api: &'static TapeApi, name: &str) {
    let write = (api.fs_write)(name.as_ptr(), name.len(), b"gone".as_ptr(), 4);
    let mut byte = [0u8; 4];
    let before = tape_rt::read_file_at(name, 0, &mut byte);
    let first = (api.fs_remove)(name.as_ptr(), name.len());
    let after = tape_rt::read_file_at(name, 0, &mut byte);
    let second = (api.fs_remove)(name.as_ptr(), name.len());
    let ok = write == 0 && before == Ok(4) && first == 0 && after.is_err() && second < 0;
    println!("fscheck: remove {}: write {write}, before {before:?}, remove {first}, after {after:?}, again {second}", if ok { "ok" } else { "FAILED" });
}

const BLOCK: usize = 2048;
const LENGTHS: [usize; 2] = [20000, 19900];

fn pattern(which: usize, index: usize) -> u8 {
    if which == 0 { (index % 251) as u8 } else { (index * 7 + 3) as u8 }
}

/// Opens both files at once, writes them in turn a block at a time, closes both and reads them back.
fn streams(first: &str, second: &str) {
    let Some(api) = tape_rt::multi_stream_table() else { return println!("fscheck: streams: this kernel has no stream handles") };
    let names = [first, second];
    let mut handles = [-1isize; 2];
    for (n, name) in names.iter().enumerate() {
        let mut capacity = 0u64;
        handles[n] = (api.fs_stream_open2)(name.as_ptr(), name.len(), 64 * 1024, &mut capacity);
        println!("fscheck: stream {n} {name}: handle {} capacity {capacity}", handles[n]);
        if handles[n] < 0 {
            return println!("fscheck: streams: open failed");
        }
    }
    let mut block = [0u8; BLOCK];
    for offset in (0..LENGTHS[0]).step_by(BLOCK) {
        for n in 0..2 {
            let count = LENGTHS[n].saturating_sub(offset).min(BLOCK);
            if count == 0 {
                continue;
            }
            for (i, byte) in block[..count].iter_mut().enumerate() {
                *byte = pattern(n, offset + i);
            }
            let status = (api.fs_stream_write2)(handles[n], offset as u64, block.as_ptr(), count, (offset + count) as u64);
            if status != 0 {
                return println!("fscheck: streams: write {n} at {offset} failed ({status})");
            }
        }
    }
    for n in 0..2 {
        let status = (api.fs_stream_close2)(handles[n], LENGTHS[n] as u64);
        if status != 0 {
            return println!("fscheck: streams: close {n} failed ({status})");
        }
    }
    for n in 0..2 {
        let mut bad = 0;
        let mut total = 0;
        loop {
            match tape_rt::read_file_at(names[n], total as u64, &mut block) {
                Ok(0) => break,
                Ok(count) => {
                    bad += block[..count].iter().enumerate().filter(|&(i, &byte)| byte != pattern(n, total + i)).count();
                    total += count;
                }
                Err(code) => return println!("fscheck: streams: read {n} error {code}"),
            }
        }
        println!("fscheck: streams {n}: {total} bytes, {bad} wrong");
    }
}

/// ABI 32: `vfs_stat`, then `vfs_read_dir` for every entry of a folder or `vfs_read` of the start of a file.
fn vfs(path: &str) {
    let stat = match tape_rt::vfs_stat(path) {
        Ok(stat) => stat,
        Err(code) => return println!("fscheck: vfs {path}: error {code}"),
    };
    println!("fscheck: vfs {path}: kind {} mode {:o} size {} uid {} gid {} mtime {}", stat.kind, stat.mode, stat.size, stat.uid, stat.gid, stat.mtime);
    if stat.kind == 2 {
        let mut index = 0;
        while let Ok(Some(entry)) = tape_rt::vfs_read_dir(path, index) {
            let name = core::str::from_utf8(entry.name()).unwrap_or("?");
            println!("fscheck: entry {index} [{name}] kind {} size {} uid {}", entry.stat.kind, entry.stat.size, entry.stat.uid);
            index += 1;
        }
        println!("fscheck: {index} entries");
    } else {
        let mut buffer = [0u8; 40];
        match tape_rt::vfs_read(path, 0, &mut buffer) {
            Ok(count) => println!("fscheck: read {count} bytes [{}]", core::str::from_utf8(&buffer[..count]).unwrap_or("?")),
            Err(code) => println!("fscheck: read error {code}"),
        }
    }
}
