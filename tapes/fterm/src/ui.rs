//! The panel shown by a bare `run fterm` (or `run fterm ui`): pick the transport and its settings, press C to connect. Like the
//! `audio-net` and recorder panels it draws with plain ANSI (so it works on the local screen and over `term net`) and reads
//! `read_key`. All keys are plain or arrows: Up/Down or Tab select, Left/Right, Space/Backspace or Enter change, C connects, W saves
//! `fterm.cfg` (which `run fterm @fterm.cfg` repeats), P lists the machines that announced themselves and Enter picks one, Q or Esc quits. The form's
//! rules live in `form.rs`.

use crate::connect;
use fterm::form::{Form, NET, ROWS, Row, TRANSPORTS};
use fterm::{format_name, parse_args};
use tape_panel::{Text, at, begin_batch, draw, end_batch};
use tape_rt::{Key, KeyDecoder, PeerInfo, TapeApi};

const CONFIG_FILE: &str = "fterm.cfg";
const FIRST_ROW: usize = 4;

fn label(row: Row) -> &'static str {
    match row {
        Row::Transport => "Connect to",
        Row::Port => "Serial port",
        Row::Baud => "Baud rate",
        Row::Format => "Data format",
        Row::Server => "Telnet server (IP or name)",
        Row::TcpPort => "TCP port",
        Row::Addressing => "Addressing",
        Row::Local => "Our address/prefix",
        Row::Gateway => "Gateway",
        Row::Dns => "DNS server (optional)",
        Row::Peer => "Machine (name or MAC)",
        Row::Echo => "Local echo",
        Row::Crlf => "Enter sends CR LF",
    }
}

fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

fn port_name(api: &TapeApi, number: usize) -> Text<24> {
    let mut info = tape_rt::SerialInfo::default();
    let mut text = Text::new();
    if (api.serial_info)(number, &mut info) != 0 {
        let name = core::str::from_utf8(&info.name).unwrap_or("?").trim_end_matches('\0');
        let _ = core::fmt::Write::write_fmt(&mut text, format_args!("{number}: {name}"));
    } else {
        let _ = core::fmt::Write::write_fmt(&mut text, format_args!("{number}: not present"));
    }
    text
}

fn value(api: &TapeApi, form: &Form, row: Row) -> Text<40> {
    let mut text = Text::new();
    let write = core::fmt::Write::write_fmt;
    let _ = match row {
        Row::Transport => write(&mut text, format_args!("{}", TRANSPORTS[form.transport])),
        Row::Port => write(&mut text, format_args!("{}", port_name(api, form.port).as_str())),
        Row::Baud => write(&mut text, format_args!("{}", form.baud)),
        Row::Format => {
            let mut name = [0u8; 3];
            format_name(form.format, &mut name);
            write(&mut text, format_args!("{}", core::str::from_utf8(&name).unwrap_or("?")))
        }
        Row::Server => write(&mut text, format_args!("{}", form.server.as_str())),
        Row::TcpPort => write(&mut text, format_args!("{}", form.tcp_port.as_str())),
        Row::Addressing => write(&mut text, format_args!("{}", if form.dhcp { "DHCP" } else { "static" })),
        Row::Local => write(&mut text, format_args!("{}", form.local.as_str())),
        Row::Gateway => write(&mut text, format_args!("{}", form.gateway.as_str())),
        Row::Dns => write(&mut text, format_args!("{}", form.dns.as_str())),
        Row::Peer => write(&mut text, format_args!("{}", form.peer.as_str())),
        Row::Echo => write(&mut text, format_args!("{}", on_off(form.echo))),
        Row::Crlf => write(&mut text, format_args!("{}", on_off(form.crlf))),
    };
    text
}

fn draw_row(api: &TapeApi, form: &Form, index: usize, selected: usize, editing: bool) {
    let row = ROWS[index];
    at(FIRST_ROW + index, 1);
    if !row.applies(form) {
        draw!("\x1b[K");
        return;
    }
    let marker = if index == selected { '>' } else { ' ' };
    let cursor = if editing && index == selected { "_" } else { "" };
    draw!(" {marker} {:<28}{}{cursor}\x1b[K", label(row), value(api, form, row).as_str());
}

fn draw_message(message: &str) {
    at(FIRST_ROW + ROWS.len() + 1, 1);
    draw!(" {message}\x1b[K");
}

fn draw_all(api: &TapeApi, form: &Form, selected: usize, editing: bool, message: &str) {
    begin_batch();
    draw!("\x1b[2J");
    at(1, 1);
    draw!(" fos fterm: a terminal for serial ports, Telnet and other fos machines\x1b[K");
    at(2, 1);
    draw!(" {:-<78}", "");
    for index in 0..ROWS.len() {
        draw_row(api, form, index, selected, editing);
    }
    draw_message(message);
    at(FIRST_ROW + ROWS.len() + 3, 1);
    draw!(" Up/Down or Tab: select   Enter or Space/Backspace: change   C: connect\x1b[K");
    at(FIRST_ROW + ROWS.len() + 4, 1);
    draw!(" P: pick a machine from a list   W: save {CONFIG_FILE}   Q or Esc: quit\x1b[K");
    at(FIRST_ROW + ROWS.len() + 5, 1);
    draw!(" Once connected: Ctrl-A x leaves, Ctrl-A h lists the other commands\x1b[K");
    end_batch();
}

fn next_key(decoder: &mut KeyDecoder) -> Option<Key> {
    match (tape_rt::api().read_key)() {
        tape_rt::KEY_NONE => None,
        tape_rt::KEY_ABORT => Some(Key::Control(0x1b)),
        other => decoder.feed((other & 0xff) as u8),
    }
}

fn load() -> Form {
    let api = tape_rt::api();
    let mut buffer = [0u8; 200];
    let length = (api.fs_read)(CONFIG_FILE.as_ptr(), CONFIG_FILE.len(), buffer.as_mut_ptr(), buffer.len());
    if length > 0 {
        if let Ok(text) = core::str::from_utf8(&buffer[..length as usize]) {
            if let Some(form) = Form::from_args(text) {
                return form;
            }
        }
    }
    Form::default()
}

fn save(form: &Form) -> &'static str {
    if let Some(problem) = form.problem() {
        return problem;
    }
    let mut text = form.args();
    text.push(b'\n');
    let api = tape_rt::api();
    if (api.fs_write)(CONFIG_FILE.as_ptr(), CONFIG_FILE.len(), text.as_str().as_ptr(), text.len()) == 0 {
        "Saved fterm.cfg. `run fterm @fterm.cfg` connects exactly like this."
    } else {
        "Could not write fterm.cfg (is the volume full?)."
    }
}

const PICK_ROWS: usize = 14;

fn draw_picker(api: &TapeApi, count: usize, selected: usize, top: usize) {
    begin_batch();
    draw!("\x1b[2J");
    at(1, 1);
    draw!(" Pick a machine ({count} announced on the network)\x1b[K");
    at(2, 1);
    draw!(" {:-<78}", "");
    for line in 0..PICK_ROWS.min(count - top) {
        let number = top + line + 1;
        let mut info = PeerInfo::default();
        (api.peer_info)(number, &mut info);
        let marker = if number == selected + 1 { '>' } else { ' ' };
        let mac = info.mac;
        at(3 + line, 1);
        draw!(
            " {marker} {:<20} {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}  NIC {}\x1b[K",
            info.name(), mac[0], mac[1], mac[2], mac[3], mac[4], mac[5], info.nic
        );
    }
    at(4 + PICK_ROWS, 1);
    draw!(" Up/Down: select   Enter: choose   Esc or Q: cancel\x1b[K");
    end_batch();
}

/// P: a list of the machines the kernel has heard announce themselves (`peer_info`); Enter puts the chosen name in the form.
fn pick_peer(api: &TapeApi, decoder: &mut KeyDecoder, form: &mut Form) -> &'static str {
    if api.version < 5 {
        return "This kernel has no peer table.";
    }
    let count = (api.peer_info)(0, core::ptr::null_mut());
    if count == 0 {
        return "No machine has announced itself yet (`nic peers` lists them).";
    }
    let mut selected = 0;
    for number in 1..=count {
        let mut info = PeerInfo::default();
        (api.peer_info)(number, &mut info);
        if info.name().eq_ignore_ascii_case(form.peer.as_str()) {
            selected = number - 1;
        }
    }
    let mut top = 0;
    loop {
        if selected < top {
            top = selected;
        } else if selected >= top + PICK_ROWS {
            top = selected + 1 - PICK_ROWS;
        }
        draw_picker(api, count, selected, top);
        let key = loop {
            match next_key(decoder) {
                Some(key) => break key,
                None => (api.wait_for_event)(),
            }
        };
        match key {
            Key::Up => selected = (selected + count - 1) % count,
            Key::Down | Key::Tab => selected = (selected + 1) % count,
            Key::Enter => {
                let mut info = PeerInfo::default();
                (api.peer_info)(selected + 1, &mut info);
                form.peer = Text::from(info.name());
                return "";
            }
            Key::Char(b'q' | b'Q') | Key::Control(0x1b) => return "",
            _ => {}
        }
    }
}

/// Shows the panel until the user quits; each connection returns to it.
pub fn run(api: &'static TapeApi) {
    let mut form = load();
    let mut decoder = KeyDecoder::new();
    let mut selected = 0;
    let mut editing = false;
    let mut backup = form;
    let mut message = "";
    draw_all(api, &form, selected, editing, message);
    loop {
        let Some(key) = next_key(&mut decoder) else {
            (api.wait_for_event)();
            continue;
        };
        message = "";
        let row = ROWS[selected];
        if editing {
            let mut text = form.text_mut(row);
            match key {
                Key::Enter => editing = false,
                Key::Control(0x1b) => {
                    form = backup;
                    editing = false;
                }
                Key::Backspace => {
                    if let Some(text) = text.as_mut() {
                        text.pop();
                    }
                }
                Key::Char(byte) if row.accepts(byte) => {
                    if let Some(text) = text.as_mut() {
                        text.push(byte);
                    }
                }
                _ => {}
            }
        } else {
            match key {
                Key::Up => selected = form.move_row(selected, -1),
                Key::Down | Key::Tab => selected = form.move_row(selected, 1),
                Key::Left | Key::Backspace => form.cycle(row, -1, (api.serial_count)()),
                Key::Right | Key::Char(b' ') => form.cycle(row, 1, (api.serial_count)()),
                Key::Enter if row.is_text() => {
                    // A fresh value replaces the old one, as in the other panels.
                    backup = form;
                    if let Some(mut text) = form.text_mut(row) {
                        text.clear();
                    }
                    editing = true;
                }
                Key::Enter => form.cycle(row, 1, (api.serial_count)()),
                Key::Char(b'p' | b'P') => {
                    if form.transport != NET {
                        form.transport = NET;
                        selected = ROWS.iter().position(|&row| row == Row::Peer).unwrap_or(selected);
                    }
                    message = pick_peer(api, &mut decoder, &mut form);
                }
                Key::Char(b'w' | b'W') => message = save(&form),
                Key::Char(b'c' | b'C') => match form.problem() {
                    Some(problem) => message = problem,
                    None => {
                        let args = form.args();
                        if let Ok(config) = parse_args(args.as_str()) {
                            draw!("\x1b[2J\x1b[H");
                            connect(api, &config);
                            draw!("\n\nPress a key to return to the panel.");
                            while next_key(&mut decoder).is_none() {
                                (api.wait_for_event)();
                            }
                        }
                    }
                },
                Key::Char(b'q' | b'Q') | Key::Control(0x1b) => {
                    draw!("\x1b[2J\x1b[H");
                    return;
                }
                _ => {}
            }
        }
        draw_all(api, &form, selected, editing, message);
    }
}
