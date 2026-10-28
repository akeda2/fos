#![no_std]
#![no_main]

//! `fterm`: a terminal for serial ports, Telnet servers and other fos machines (`term net`). Keys go out over the link,
//! what comes back is shown on the console (the kernel console understands the usual ANSI sequences, so a remote shell or a
//! full-screen editor works). It works from the local keyboard or a `term net` session.
//!
//! The transports are `Link`s (`link.rs`): `serial.rs` (the kernel's `serial_*` calls: COM2-COM4 today, USB adapters later
//! behind the same calls; COM1 is the console and is never listed), `telnet.rs` and `termnet.rs`. The command line and the key
//! handling live in `lib.rs` (host-tested); this file is the loop around them.
//!
//! Every key is sent as typed, Ctrl-C included; Escape too (the kernel reports it as an abort request, which `Keys` turns
//! back into ESC). Commands start with Ctrl-A and then a plain letter: nothing needs AltGr, so a Nordic keyboard works.
//! A bare `run fterm` shows a settings panel (`ui.rs`, form rules in `form.rs`) that builds one of the command lines above.
//! The loop never blocks: it hands the link what it takes, shows what the link has, and sleeps with `wait_for_event` (which
//! also polls the UART every 100 us while a port is open) when neither had anything to do.

mod link;
mod serial;
mod telnet;
mod termnet;
mod ui;

use fterm::{Action, Config, Keys, Peer, Target, parse_args};
use link::{Link, Nic};
use serial::SerialLink;
use tape_rt::{PeerInfo, SerialInfo, TapeApi, println};
use telnet::TelnetLink;
use termnet::TermNetLink;

const TX_CAPACITY: usize = 1024;

static mut TX: [u8; TX_CAPACITY] = [0; TX_CAPACITY];
static mut TX_HEAD: usize = 0;
static mut TX_TAIL: usize = 0;

fn tx_used() -> usize {
    unsafe { (TX_HEAD + TX_CAPACITY - TX_TAIL) % TX_CAPACITY }
}

fn tx_push(byte: u8) {
    unsafe {
        let next = (TX_HEAD + 1) % TX_CAPACITY;
        if next != TX_TAIL {
            (*(&raw mut TX))[TX_HEAD] = byte;
            TX_HEAD = next;
        }
    }
}

/// Offers the link what is queued; true if it took any.
fn tx_pump(link: &mut dyn Link) -> bool {
    unsafe {
        if TX_TAIL == TX_HEAD {
            return false;
        }
        let mut chunk = [0u8; 64];
        let mut count = 0;
        let mut index = TX_TAIL;
        while index != TX_HEAD && count < chunk.len() {
            chunk[count] = (*(&raw const TX))[index];
            index = (index + 1) % TX_CAPACITY;
            count += 1;
        }
        let taken = link.write(&chunk[..count]);
        TX_TAIL = (TX_TAIL + taken) % TX_CAPACITY;
        taken > 0
    }
}

fn usable(api: &TapeApi) -> bool {
    api.version >= 7 && (api.size as usize) >= core::mem::offset_of!(TapeApi, serial_break) + core::mem::size_of::<usize>()
}

fn list(api: &TapeApi) {
    let count = (api.serial_count)();
    if count == 0 {
        println!("No serial ports besides the console (COM1).");
        return;
    }
    for number in 1..=count {
        let mut info = SerialInfo::default();
        if (api.serial_info)(number, &mut info) != 0 {
            let name = core::str::from_utf8(&info.name).unwrap_or("?").trim_end_matches('\0');
            println!("{number}: {name} at {:#x}", info.io_base);
        }
    }
}

fn help(lead: u8) {
    let key = (b'A' + lead - 1) as char;
    println!("\n[Ctrl-{key} then: x exit, s status, e local echo, l CR sends CRLF, c clear screen,");
    println!(" a send Ctrl-{key}, h help; serial only: b break, d toggle DTR, r toggle RTS]");
    println!(" Inside another fterm: Ctrl-{key} a x leaves the inner one, or start it with prefix=b]");
}

/// A peer's MAC and the interface it was heard on, by announced name (case does not matter).
fn find_peer(api: &TapeApi, name: &str) -> Option<([u8; 6], u32)> {
    if api.version < 5 {
        return None;
    }
    let count = (api.peer_info)(0, core::ptr::null_mut());
    for number in 1..=count {
        let mut info = PeerInfo::default();
        (api.peer_info)(number, &mut info);
        if info.name().eq_ignore_ascii_case(name) {
            return Some((info.mac, u32::from(info.nic)));
        }
    }
    None
}

fn prefix_name(config: &Config) -> char {
    (b'A' + config.prefix - 1) as char
}

/// Runs the terminal until the user leaves or the link closes.
fn run(api: &TapeApi, link: &mut dyn Link, config: &Config) {
    let (mut echo, mut crlf) = (config.echo, config.crlf);
    let lead = config.prefix;
    let mut keys = Keys::with_prefix(lead);
    let mut buffer = [0u8; 256];
    'run: loop {
        let mut busy = link.poll();
        while tx_used() + 3 < TX_CAPACITY {
            let key = (api.read_key)();
            if key == tape_rt::KEY_NONE {
                break;
            }
            busy = true;
            match keys.feed(key) {
                Action::Nothing => {}
                Action::Send(byte) => {
                    tx_push(byte);
                    if byte == b'\r' && crlf {
                        tx_push(b'\n');
                    }
                    if echo {
                        let shown: &[u8] = if byte == b'\r' { b"\r\n" } else { &[byte] };
                        (api.puts)(shown.as_ptr(), shown.len());
                    }
                }
                Action::Exit => break 'run,
                Action::Help => help(lead),
                Action::Status => {
                    link.status();
                    println!("[echo {}, crlf {}]", if echo { "on" } else { "off" }, if crlf { "on" } else { "off" });
                }
                Action::ToggleEcho => {
                    echo = !echo;
                    println!("\n[echo {}]", if echo { "on" } else { "off" });
                }
                Action::ToggleCrlf => {
                    crlf = !crlf;
                    println!("\n[crlf {}]", if crlf { "on" } else { "off" });
                }
                Action::Clear => (api.puts)(b"\x1b[2J\x1b[H".as_ptr(), 7),
                Action::Unknown => println!("\n[unknown command; Ctrl-{} h lists them]", (b'A' + lead - 1) as char),
                action @ (Action::Break | Action::ToggleDtr | Action::ToggleRts) => {
                    if !link.command(&action) {
                        println!("\n[that command is for serial ports]");
                    }
                }
            }
        }
        if tx_pump(link) {
            busy = true;
        }
        let received = link.read(&mut buffer);
        if received > 0 {
            busy = true;
            (api.puts)(buffer.as_ptr(), received);
        }
        if !link.is_open() {
            break;
        }
        if !busy {
            (api.wait_for_event)();
        }
    }
    link.close();
    println!("\nfterm: closed.");
}

static mut EXPANDED: [u8; 320] = [0; 320];

/// `@NAME` first on the command line stands for that file's words (what the panel's W saves); more words may follow it.
fn expand<'a>(api: &TapeApi, text: &'a str) -> Result<&'a str, &'static str> {
    let trimmed = text.trim_start();
    let Some(rest) = trimmed.strip_prefix('@') else { return Ok(text) };
    let (name, after) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let buffer: &'static mut [u8; 320] = unsafe { &mut *(&raw mut EXPANDED) };
    let length = (api.fs_read)(name.as_ptr(), name.len(), buffer.as_mut_ptr(), 200);
    if length <= 0 {
        return Err("cannot read that configuration file");
    }
    let mut used = length as usize;
    while used > 0 && buffer[used - 1].is_ascii_whitespace() {
        used -= 1;
    }
    let after = after.trim();
    if used + 1 + after.len() > buffer.len() {
        return Err("the configuration and the command line are too long");
    }
    buffer[used] = b' ';
    buffer[used + 1..used + 1 + after.len()].copy_from_slice(after.as_bytes());
    core::str::from_utf8(&buffer[..used + 1 + after.len()]).map_err(|_| "the configuration file is not text")
}

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    tape_rt::init(api);
    if !usable(api) {
        println!("fterm needs a kernel with the serial calls (tape ABI 7).");
        return;
    }
    let text = core::str::from_utf8(tape_rt::args()).unwrap_or("");
    if matches!(text.trim(), "" | "ui") {
        ui::run(api);
        return;
    }
    let text = match expand(api, text) {
        Ok(text) => text,
        Err(message) => {
            println!("fterm: {message}.");
            return;
        }
    };
    let config = match parse_args(text) {
        Ok(config) => config,
        Err(message) => {
            println!("fterm: {message}. Try `help fterm`.");
            return;
        }
    };
    if config.list {
        list(api);
        return;
    }
    connect(api, &config);
}

/// Opens the link a configuration names and runs the terminal on it; failures are printed, not returned.
fn connect(api: &'static TapeApi, config: &Config) {
    match &config.target {
        Target::Serial => {
            let mut link = match SerialLink::open(api, config.port, config.baud, config.format) {
                Ok(link) => link,
                Err(tape_rt::SERIAL_NO_PORT) => {
                    println!("fterm: no serial port {}. `run fterm list` shows them.", config.port);
                    return;
                }
                Err(tape_rt::SERIAL_BUSY) => {
                    println!("fterm: port {} carries this machine's shell (`serial-shell` in settings).", config.port);
                    return;
                }
                Err(_) => {
                    println!("fterm: the port refused those settings.");
                    return;
                }
            };
            let mut format = [0u8; 3];
            fterm::format_name(config.format, &mut format);
            println!(
                "fterm: port {}, {} baud {}. Ctrl-{} x exits, Ctrl-{} h helps.",
                config.port,
                config.baud,
                core::str::from_utf8(&format).unwrap_or(""),
                prefix_name(config),
                prefix_name(config)
            );
            run(api, &mut link, config);
        }
        Target::Telnet(target) => {
            let Some(nic) = Nic::open(tape_rt::active_nic() as u32) else {
                println!("fterm: no network interface (`nic use` picks one).");
                return;
            };
            let mut link = TelnetLink::open(nic, target);
            run(api, &mut link, config);
        }
        Target::Net(peer) => {
            let (mac, index) = match peer {
                Peer::Mac(mac) => (*mac, tape_rt::active_nic() as u32),
                Peer::Name(name) => match find_peer(api, name) {
                    Some(found) => found,
                    None => {
                        println!("fterm: no machine called {name} has announced itself (`nic peers`). Use its MAC address.");
                        return;
                    }
                },
            };
            let Some(nic) = Nic::open(index) else {
                println!("fterm: no network interface (`nic use` picks one).");
                return;
            };
            let Some(mut link) = TermNetLink::open(nic, mac) else {
                println!("fterm: the network interface refused the frame.");
                return;
            };
            println!("fterm: term net. Ctrl-{} x leaves, Ctrl-{} h helps.", prefix_name(config), prefix_name(config));
            run(api, &mut link, config);
        }
    }
}

tape_rt::tape_help!(br#"fterm: a terminal for serial ports, Telnet and other fos machines.

  run fterm                       the settings panel (also `run fterm ui`):
                                  pick a transport, C connects, W saves
                                  fterm.cfg, which `run fterm @fterm.cfg` runs
  run fterm list                  show the serial ports (COM2-COM4)
  run fterm [PORT] [BAUD] [8n1] [echo] [crlf]
                                  serial: PORT 1 = first listed, BAUD 115200,
                                  format 8n1 (7e1, 8o2, ... also work)
  run fterm telnet IP|NAME [PORT] [static LOCAL/PREFIX GATEWAY] [dns IP]
                                  Telnet (port 23); DHCP unless static is given.
                                  A NAME goes to the DNS server DHCP names,
                                  `dns`, or else the gateway
  run fterm net NAME|MAC          another fos machine's shell (term net)
  Any form takes prefix=LETTER: Ctrl-LETTER starts the commands instead of
  Ctrl-A (not c, h, i, j, m). Use it for an fterm run inside another fterm.

  Keys go out as typed (Ctrl-C and Esc too); incoming text is shown.
  Commands start with Ctrl-A, then a plain letter:
    x exit   s status   e echo   l CR->CRLF   c clear screen   a send Ctrl-A
    h help   serial only: b break   d DTR   r RTS
  Nested: the outer fterm eats Ctrl-A x. Type Ctrl-A a x to leave the inner
  one, or start the inner with prefix=b (then Ctrl-B x).
  Telnet and net use the interface chosen with `nic use`.
"#);
