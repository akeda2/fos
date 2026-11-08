//! ssh: a terminal on another machine, or one command run there. `ssh USER@HOST` opens a login shell on the console,
//! `ssh USER@HOST COMMAND...` runs the command and prints its output.
//!
//! The login is an ed25519 key (`settings/ssh_key`, made by `ssh keygen`, shared with `sshmount`) and the server's key is
//! checked against `settings/known_hosts`, trust on first use: both are in the `ssh-tape` crate. The protocol is
//! `ssh::client` (`connect_shell`, `write_input`, `read_output`).
//!
//! Keys go to the server byte for byte (the kernel turns arrows and the like into ANSI sequences) and what the server prints
//! goes to the console as it is. Esc is the kernel's abort key, so it ends the session instead of being sent.
#![no_std]
#![no_main]

use ssh::client::{CLIENT_ARENA, Client, Error, Login, SHELL_CHUNK, Start};
use ssh_tape::{Trust, Wire, load_seed, parse_target};
use tape_ip::tcp::Socket;
use tape_rt::*;

tape_rt::tape_help!(
    br#"ssh USER@HOST[:PORT] [key=NAME | password|pw] [COMMAND...]
ssh keygen [key=NAME]
Opens a login shell on another machine over SSH, on this console, or runs
COMMAND there and prints its output (then the exit status, if it is not 0).
Esc ends the session. Everything else you type goes to the server, which does
the echo and the line editing; the console understands ANSI colours and
cursor movement, so a full-screen program may not look right.
Login is by key: `ssh keygen` makes settings/ssh_key (key=NAME: another file,
shared with sshmount) and prints its line; put that line in
~/.ssh/authorized_keys on the server. With `password` (or `pw`) the tape asks
for the account's password instead (typed blind, Esc cancels): for a first
login on a machine that does not have the key yet.
The server's key is remembered in settings/known_hosts the first time and
checked afterwards: a changed key is refused (edit the file to accept it).
The tape takes its address from DHCP on the active NIC (`nic use N`).
OpenSSH, fos's sshd (with the `shell` option), any SSH-2 server with ed25519.
"#
);

/// How long one wait for server data lasts while a session runs: the delay a typed key can see before it is sent.
const POLL_MICROS: u64 = 2_000;
const LOGIN_WAIT_MICROS: u64 = 20_000_000;
/// Typed bytes held until the server's window takes them.
const TYPED: usize = 512;

static mut ARENA: [u8; CLIENT_ARENA] = [0; CLIENT_ARENA];
static mut OUTPUT: [u8; SHELL_CHUNK] = [0; SHELL_CHUNK];
static mut SOCKET: Socket<16384> = Socket::new();

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    tape_rt::init(api);
    let mut rest = core::str::from_utf8(tape_rt::args()).unwrap_or("").trim();
    let (mut key_file, mut keygen, mut password, mut target, mut command) = ("ssh_key", false, false, None, "");
    while !rest.is_empty() {
        let whole = rest;
        let (word, after) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        rest = after.trim_start();
        if let Some(value) = word.strip_prefix("key=") {
            key_file = value;
        } else if word == "password" || word == "pw" {
            password = true;
        } else if word == "keygen" && target.is_none() {
            keygen = true;
        } else if target.is_none() {
            target = Some(word);
        } else {
            // The first word after the target that is not an option starts the command.
            command = whole.trim_end();
            break;
        }
    }
    let seed = if password { Some([0u8; 32]) } else { load_seed("ssh", api, key_file, keygen) };
    let Some(seed) = seed else { return };
    if keygen {
        let mut line = [0u8; 80];
        let length = ssh::authorized::public_line(&seed, &mut line);
        println!("ssh: add this line to ~/.ssh/authorized_keys on the server:");
        return println!("{}", core::str::from_utf8(&line[..length]).unwrap_or(""));
    }
    let Some((user, host, port)) = target.and_then(|target| parse_target("ssh", target)) else {
        return println!("usage: ssh USER@HOST[:PORT] [key=NAME] [COMMAND...]  (help ssh)");
    };

    let mut typed_password = [0u8; 128];
    let password_length = if password {
        match ask_password(api, user, host, &mut typed_password) {
            Some(length) => length,
            None => return println!("ssh: cancelled."),
        }
    } else {
        0
    };
    let login = if password { Login::Password(&typed_password[..password_length]) } else { Login::Key(&seed) };

    let start = if command.is_empty() {
        let (cols, rows) = tape_rt::console_size();
        Start::Shell { cols: cols as u32, rows: rows as u32 }
    } else {
        Start::Exec(command)
    };
    #[allow(static_mut_refs)]
    let (arena, socket) = unsafe { (&mut *core::ptr::addr_of_mut!(ARENA), core::ptr::replace(core::ptr::addr_of_mut!(SOCKET), Socket::new())) };
    let Some(wire) = ssh_tape::dial("ssh", api, socket, host, port, LOGIN_WAIT_MICROS) else { return };
    let mut trust = Trust { tool: "ssh", api, host, port, refused: false };
    let mut client = match Client::connect_as(wire, arena, user, login, &mut |public| trust.check(public), start) {
        Ok(client) => client,
        Err(error) => {
            if !trust.refused {
                println!("ssh: {user}@{host}: {}.", error.describe());
                if password && matches!(error, Error::Auth) {
                    println!("ssh: wrong password, or the server does not allow password login.");
                }
            }
            return;
        }
    };
    {
        let wire = client.link();
        wire.wait_micros = POLL_MICROS;
        wire.polling = true;
    }
    let ended = run(api, &mut client, matches!(start, Start::Shell { .. }));
    if let Some(status) = client.exit_status().filter(|&status| status != 0) {
        println!("\nssh: exit status {status}.");
    }
    match ended {
        Ended::Left => println!("\nssh: left {host}."),
        Ended::Lost(error) => println!("\nssh: {host}: {}.", error.describe()),
        Ended::Done => {}
    }
    client.hang_up();
    client.link().socket.close(api);
}

/// Reads a password from the keyboard without showing it; None when Esc cancels.
fn ask_password(api: &TapeApi, user: &str, host: &str, out: &mut [u8]) -> Option<usize> {
    print!("{user}@{host}'s password: ");
    let mut length = 0;
    loop {
        match (api.read_key)() {
            KEY_NONE => tape_rt::idle(),
            KEY_ABORT => {
                println!();
                return None;
            }
            key if key >= KEY_BASE => match (key - KEY_BASE) as u8 {
                b'\r' | b'\n' => {
                    println!();
                    return Some(length);
                }
                8 | 127 => length = length.saturating_sub(1),
                byte if length < out.len() => {
                    out[length] = byte;
                    length += 1;
                }
                _ => {}
            },
            _ => {}
        }
    }
}

enum Ended {
    /// The server ended the session.
    Done,
    /// Esc.
    Left,
    Lost(Error),
}

/// Keys to the server and its output to the console until one side ends the session.
fn run(api: &'static TapeApi, client: &mut Client<'static, Wire>, interactive: bool) -> Ended {
    #[allow(static_mut_refs)]
    let output = unsafe { &mut *core::ptr::addr_of_mut!(OUTPUT) };
    let mut typed = [0u8; TYPED];
    let mut held = 0;
    loop {
        // The abort key ends a command run too; only a shell session forwards what is typed.
        for _ in 0..64 {
            match (api.read_key)() {
                KEY_NONE => break,
                KEY_ABORT => return Ended::Left,
                key if key >= KEY_BASE && interactive && held < TYPED => {
                    typed[held] = (key - KEY_BASE) as u8;
                    held += 1;
                }
                _ => {}
            }
        }
        if held > 0 {
            match client.write_input(&typed[..held]) {
                Ok(taken) => {
                    typed.copy_within(taken..held, 0);
                    held -= taken;
                }
                Err(Error::Closed) => return Ended::Done,
                Err(error) => return Ended::Lost(error),
            }
        }
        match client.read_output(&mut output[..]) {
            Ok(Some(count)) => {
                (api.puts)(output.as_ptr(), count);
            }
            Ok(None) => {}
            Err(Error::Closed) => return Ended::Done,
            Err(error) => return Ended::Lost(error),
        }
    }
}
