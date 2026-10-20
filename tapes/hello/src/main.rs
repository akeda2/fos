//! The smallest v2 tape: says hello, shows the arguments it was started with, counts
//! primes with a sieve in `alloc`ed memory, reports the clock, and returns.
#![no_std]
#![no_main]

use tape_rt::{TapeApi, println};

const LIMIT: usize = 100_000;

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    tape_rt::init(api);
    println!("hello from a v2 tape (API version {}, table of {} bytes).", api.version, api.size);
    let arguments = core::str::from_utf8(tape_rt::args()).unwrap_or("?");
    println!("arguments: [{arguments}]");
    let mut names = 0;
    while let Ok(Some(file)) = tape_rt::list_file(api, "", names) {
        names += 1;
        if names == 1 {
            println!("first file: {} ({} bytes)", core::str::from_utf8(file.name()).unwrap_or("?"), file.length);
        }
    }
    println!("files on the boot volume: {names}");

    // A sieve in memory from the kernel's allocator: no pools, no size classes.
    let bytes = LIMIT + 1;
    let memory = (api.alloc)(bytes);
    if memory == 0 {
        println!("alloc failed");
        return;
    }
    let sieve = unsafe { core::slice::from_raw_parts_mut(memory as *mut bool, bytes) };
    let started = tape_rt::micros();
    let mut primes = 0;
    for candidate in 2..=LIMIT {
        if sieve[candidate] {
            continue;
        }
        primes += 1;
        let mut multiple = candidate * candidate;
        while multiple <= LIMIT {
            sieve[multiple] = true;
            multiple += candidate;
        }
    }
    let elapsed = tape_rt::micros() - started;
    println!("primes up to {LIMIT}: {primes} (in {elapsed} us)");
    (api.free)(memory, bytes);
}
