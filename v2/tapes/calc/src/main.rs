//! A reverse-Polish-notation integer calculator. `tape run 3 4 +` evaluates its
//! arguments once and returns; plain `tape run` starts a line-at-a-time REPL on
//! `read_key` (from the local keyboard or the term net session) until Escape.
//!
//! RPN needs no precedence table or parentheses: a stack and one left-to-right pass over
//! whitespace-separated tokens, each a signed integer (pushed) or one of `+ - * / %`
//! (pops two, pushes one). Arithmetic is 64-bit and checked: overflow is an error, never
//! a wrap.
#![no_std]
#![no_main]

use tape_rt::{KEY_ABORT, KEY_BASE, KEY_NONE, TapeApi, print, println};

const STACK: usize = 32;
const LINE: usize = 120;

#[derive(Debug, PartialEq, Eq)]
enum Error {
    Empty,
    Underflow,
    DivideByZero,
    Overflow,
    BadToken,
    StackFull,
    Leftover(usize),
}

fn evaluate(line: &str) -> Result<i64, Error> {
    let mut stack = [0i64; STACK];
    let mut depth = 0;
    let mut tokens = 0;
    for token in line.split_ascii_whitespace() {
        tokens += 1;
        let operator = match token {
            "+" | "-" | "*" | "/" | "%" => token.as_bytes()[0],
            _ => {
                let value: i64 = token.parse().map_err(|_| if token.bytes().all(|b| b.is_ascii_digit() || b == b'-') && token.len() > 1 { Error::Overflow } else { Error::BadToken })?;
                if depth == STACK {
                    return Err(Error::StackFull);
                }
                stack[depth] = value;
                depth += 1;
                continue;
            }
        };
        if depth < 2 {
            return Err(Error::Underflow);
        }
        let (left, right) = (stack[depth - 2], stack[depth - 1]);
        let result = match operator {
            b'+' => left.checked_add(right),
            b'-' => left.checked_sub(right),
            b'*' => left.checked_mul(right),
            b'/' | b'%' if right == 0 => return Err(Error::DivideByZero),
            b'/' => left.checked_div(right),
            _ => left.checked_rem(right),
        }
        .ok_or(Error::Overflow)?;
        depth -= 1;
        stack[depth - 1] = result;
    }
    match (tokens, depth) {
        (0, _) => Err(Error::Empty),
        (_, 1) => Ok(stack[0]),
        (_, 0) => Err(Error::Underflow),
        (_, extra) => Err(Error::Leftover(extra)),
    }
}

fn report(line: &str) {
    match evaluate(line) {
        Ok(value) => println!("= {value}"),
        Err(Error::Empty) => {}
        Err(Error::Underflow) => println!("error: an operator needs two operands"),
        Err(Error::DivideByZero) => println!("error: division by zero"),
        Err(Error::Overflow) => println!("error: the result does not fit in 64 bits"),
        Err(Error::BadToken) => println!("error: not a number or operator"),
        Err(Error::StackFull) => println!("error: more than {STACK} pending operands"),
        Err(Error::Leftover(count)) => println!("error: {count} operands left on the stack (missing an operator?)"),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    tape_rt::init(api);
    let arguments = core::str::from_utf8(tape_rt::args()).unwrap_or("").trim();
    if !arguments.is_empty() {
        report(arguments);
        return;
    }
    println!("RPN calculator: type e.g. `3 4 + 2 *` and Enter; Escape quits.");
    let mut line = [0u8; LINE];
    let mut length = 0;
    print!("> ");
    loop {
        match (api.read_key)() {
            KEY_NONE => tape_rt::idle(),
            KEY_ABORT => {
                println!();
                return;
            }
            key => match (key - KEY_BASE) as u8 {
                b'\r' | b'\n' => {
                    println!();
                    report(core::str::from_utf8(&line[..length]).unwrap_or(""));
                    length = 0;
                    print!("> ");
                }
                8 | 0x7f => {
                    // Without raw mode the kernel erases the character on screen itself.
                    if length > 0 {
                        length -= 1;
                        print!("\u{8} \u{8}");
                    }
                }
                byte @ 0x20..=0x7e if length < LINE => {
                    line[length] = byte;
                    length += 1;
                    print!("{}", byte as char);
                }
                _ => {}
            },
        }
    }
}
