//! web: a very small web browser. `tape run web URL` opens a lynx-style text screen (links are selected with the cursor keys);
//! `tape run web -c URL` is the curl-like command line mode: it prints the page and exits. HTTP/1.0 over plain TCP, GET and
//! HEAD only, no TLS, no cookies, no images; HTML is turned into text by `tape-http`. Host names go through the DNS server DHCP
//! names (or `dns=`).
#![no_std]
#![no_main]

use core::fmt::Write;
use tape_http::{LINK_END, LINK_START, Out, Page, parse_head, parse_url, resolve_link};
use tape_ip::net::{self, NetConfig};
use tape_ip::tcp::Socket;
use tape_net::{ArpCache, Ipv4Address, MacAddress};
use tape_rt::{KEY_ABORT, KEY_BASE, TapeApi, arg_value as value, println};

tape_rt::tape_help!(
    br#"web [URL] [ip=A.B.C.D/N gateway=A.B.C.D] [dns=A.B.C.D] [nic=N]
web -c[iILd] URL [...]
A very small web browser: http only (no TLS, cookies or images), GET only.
Without -c it is a full-screen text browser; with -c it prints and exits.
Addresses come from DHCP unless ip= is given. nic= is the NIC from 1 (default
nic use). The URL may leave out http:// and may be a name or an IP address.

Text mode:  Down/Up next/previous link (or scroll)   Right/Enter follow
  Left/Backspace back   Space/PgDn, PgUp scroll   Home/End   g or Ctrl-G go
  to an address   r reload   q or Ctrl-Q or Esc quit
Command line: -c print the page as it came; -i head and page; -I head only;
  -L follow redirects; -d print the page as text with a list of its links.
"#
);

const RESPONSE: usize = 1024 * 1024;
const MAX_LINES: usize = 8192;
const MAX_URL: usize = 512;
const HISTORY: usize = 16;
const NONE: usize = usize::MAX;

static mut SOCKET: Socket<16384> = Socket::new();
static mut PAGE: Page<131072, 16384, 512> = Page::new();
static mut LINES: [Line; MAX_LINES] = [Line { start: 0, len: 0, base: 0, open: false }; MAX_LINES];
static mut LINK_LINE: [u16; 512] = [0; 512];

#[allow(static_mut_refs)]
fn page() -> &'static mut Page<131072, 16384, 512> {
    unsafe { &mut *core::ptr::addr_of_mut!(PAGE) }
}

#[allow(static_mut_refs)]
fn lines() -> &'static mut [Line; MAX_LINES] {
    unsafe { &mut *core::ptr::addr_of_mut!(LINES) }
}

#[allow(static_mut_refs)]
fn link_line() -> &'static mut [u16; 512] {
    unsafe { &mut *core::ptr::addr_of_mut!(LINK_LINE) }
}

struct Net {
    nic: usize,
    config: NetConfig,
    dns: Ipv4Address,
    arp: ArpCache<8>,
}

fn bring_up(api: &TapeApi, text: &str) -> Result<Net, &'static str> {
    let nic = value(text, "nic").and_then(|word| word.parse::<usize>().ok()).map_or(tape_rt::active_nic(), |n| n.saturating_sub(1));
    if (api.nic_count)() <= nic {
        return Err("this machine has no such NIC");
    }
    let mut mac = [0u8; 6];
    (api.nic_mac)(nic, mac.as_mut_ptr());
    let mac = MacAddress(mac);
    let gateway = value(text, "gateway").unwrap_or("");
    let (config, mut dns) = match value(text, "ip") {
        Some(ip) => {
            let config = net::static_config(mac, ip, gateway)?;
            (config, config.gateway)
        }
        None => match net::dhcp_with_servers(api, nic, mac) {
            Some((config, _, dns)) => (config, dns.unwrap_or(config.gateway)),
            None => return Err("no DHCP lease (give ip=A.B.C.D/N for a fixed address)"),
        },
    };
    if let Some(server) = value(text, "dns").and_then(net::parse_ip) {
        dns = server;
    }
    Ok(Net { nic, config, dns, arp: ArpCache::new() })
}

/// One request and its whole answer (until the server closes) into `resp`; the length and whether it was cut off.
fn fetch(api: &TapeApi, network: &mut Net, url: &str, head_only: bool, resp: &mut [u8]) -> Result<(usize, bool), &'static str> {
    let target = parse_url(url)?;
    let ip = tape_ip::dns::lookup(api, network.nic, &network.config, &mut network.arp, network.dns, tape_net::dns::PORT, target.host)?;
    #[allow(static_mut_refs)]
    let socket = unsafe { &mut *core::ptr::addr_of_mut!(SOCKET) };
    socket.connect(api, network.nic, &network.config, ip, target.port, 5_000_000)?;
    let mut request = [0u8; 1024];
    let mut out = Out::new(&mut request);
    let method = if head_only { "HEAD" } else { "GET" };
    let written = if target.port == 80 {
        write!(out, "{method} {} HTTP/1.0\r\nHost: {}\r\n", target.path, target.host)
    } else {
        write!(out, "{method} {} HTTP/1.0\r\nHost: {}:{}\r\n", target.path, target.host, target.port)
    };
    written
        .and_then(|_| out.write_str("User-Agent: fos-web\r\nAccept: text/html, text/plain\r\nConnection: close\r\n\r\n"))
        .map_err(|_| "the address is too long")?;
    let length = out.len();
    socket.send(api, &request[..length], 3_000_000)?;
    let (mut filled, mut truncated, mut quiet) = (0, false, 0);
    loop {
        if (api.should_abort)() != 0 {
            socket.close(api);
            return Err("stopped");
        }
        if filled == resp.len() {
            truncated = true;
            break;
        }
        match socket.recv(api, &mut resp[filled..], 500_000) {
            Ok(0) => break,
            Ok(count) => {
                filled += count;
                quiet = 0;
            }
            Err(_) => {
                quiet += 1;
                if quiet >= 20 {
                    socket.close(api);
                    return Err("timed out waiting for the server");
                }
            }
        }
    }
    socket.close(api);
    Ok((filled, truncated))
}

/// `fetch` that follows redirects when `follow` is set; `url` ends up as the address of the answer.
fn load(api: &TapeApi, network: &mut Net, url: &mut [u8; MAX_URL], url_len: &mut usize, head_only: bool, follow: bool, resp: &mut [u8]) -> Result<(usize, bool), &'static str> {
    for _ in 0..6 {
        let current = core::str::from_utf8(&url[..*url_len]).map_err(|_| "bad address")?;
        let (count, truncated) = fetch(api, network, current, head_only, resp)?;
        let head = parse_head(&resp[..count]).ok_or("the server did not answer with HTTP")?;
        let redirect = matches!(head.status, 301 | 302 | 303 | 307 | 308);
        let (Some(location), true) = (head.location, follow && redirect) else {
            return Ok((count, truncated));
        };
        let location = core::str::from_utf8(location).map_err(|_| "bad redirect")?;
        let mut next = [0u8; MAX_URL];
        let length = {
            let base = parse_url(current)?;
            let mut out = Out::new(&mut next);
            resolve_link(&base, location, &mut out).map_err(|_| "the redirect address is too long")?;
            out.len()
        };
        url[..length].copy_from_slice(&next[..length]);
        *url_len = length;
    }
    Err("too many redirects")
}

fn out_byte(api: &TapeApi, byte: u8) {
    (api.putc)(u32::from(byte));
}

fn is_html(content_type: Option<&[u8]>) -> Option<bool> {
    match content_type {
        None => Some(false),
        Some(kind) if kind.len() >= 9 && kind[..9].eq_ignore_ascii_case(b"text/html") => Some(true),
        Some(kind) if kind.len() >= 5 && kind[..5].eq_ignore_ascii_case(b"text/") => Some(false),
        Some(_) => None,
    }
}

// ---------------------------------------------------------------- command line mode

struct Flags {
    include: bool,
    head_only: bool,
    follow: bool,
    dump: bool,
}

fn command_line(api: &'static TapeApi, text: &str, flags: &Flags, url: &str) {
    let mut network = match bring_up(api, text) {
        Ok(network) => network,
        Err(reason) => return println!("web: {reason}."),
    };
    let Some(resp) = tape_rt::alloc_bytes(api, RESPONSE) else {
        return println!("web: out of memory.");
    };
    let mut buffer = [0u8; MAX_URL];
    if url.len() > MAX_URL {
        return println!("web: the address is too long.");
    }
    buffer[..url.len()].copy_from_slice(url.as_bytes());
    let mut length = url.len();
    let (count, truncated) = match load(api, &mut network, &mut buffer, &mut length, flags.head_only, flags.follow, resp) {
        Ok(result) => result,
        Err(reason) => return println!("web: {url}: {reason}."),
    };
    let Some(head) = parse_head(&resp[..count]) else {
        return println!("web: not an HTTP answer.");
    };
    if flags.include || flags.head_only {
        resp[..head.header_len].iter().for_each(|&byte| out_byte(api, byte));
    }
    if flags.head_only {
        return;
    }
    let body = &resp[head.header_len..count];
    if flags.dump {
        let Some(html) = is_html(head.content_type) else {
            return println!("web: not text ({}).", core::str::from_utf8(head.content_type.unwrap_or(b"?")).unwrap_or("?"));
        };
        let page = page();
        if html { page.set_html(body) } else { page.set_plain(body) }
        page.text().iter().filter(|&&byte| byte != LINK_START && byte != LINK_END).for_each(|&byte| out_byte(api, byte));
        out_byte(api, b'\n');
        if page.link_count() > 0 {
            println!("\nLinks:");
            let final_url = core::str::from_utf8(&buffer[..length]).unwrap_or("");
            for index in 0..page.link_count() {
                let href = core::str::from_utf8(page.link(index).unwrap_or(b"")).unwrap_or("?");
                let mut absolute = [0u8; MAX_URL];
                let mut out = Out::new(&mut absolute);
                let shown = match parse_url(final_url) {
                    Ok(base) if resolve_link(&base, href, &mut out).is_ok() => core::str::from_utf8(out.bytes()).unwrap_or(href),
                    _ => href,
                };
                println!("{:3}. {shown}", index + 1);
            }
        }
    } else {
        body.iter().for_each(|&byte| out_byte(api, byte));
    }
    if truncated {
        println!("\nweb: the page was cut off at {} KiB.", RESPONSE / 1024);
    }
}

// ---------------------------------------------------------------- text screen

const A_NORMAL: u8 = 0;
const A_LINK: u8 = 1;
const A_SELECTED: u8 = 2;
const A_BAR: u8 = 3;

#[derive(Clone, Copy, PartialEq, Eq)]
struct Cell {
    ch: char,
    attr: u8,
}

const BLANK: Cell = Cell { ch: ' ', attr: A_NORMAL };

/// One wrapped screen line of `PAGE.text()`: its bytes, how many links began before it, and whether it starts inside a link.
#[derive(Clone, Copy)]
struct Line {
    start: u32,
    len: u32,
    base: u16,
    open: bool,
}

fn next_char(text: &[u8], at: usize) -> (char, usize) {
    let lead = text[at];
    let length = match lead {
        0..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => 0,
    };
    if length > 1 && at + length <= text.len() {
        if let Ok(piece) = core::str::from_utf8(&text[at..at + length]) {
            return (piece.chars().next().unwrap_or('?'), length);
        }
    }
    // Not UTF-8: take the byte as Latin-1, as old pages are.
    (char::from(lead), 1)
}

/// Breaks the page text into lines of at most `cols` characters at spaces; fills `lines()` and `link_line()`.
fn wrap(cols: usize) -> usize {
    let text = page().text();
    let (lines, link_line) = (lines(), link_line());
    let (mut count, mut starts, mut open) = (0usize, 0usize, false);
    let mut push = |start: usize, end: usize, count: &mut usize, starts: &mut usize, open: &mut bool| {
        if *count >= MAX_LINES {
            return;
        }
        lines[*count] = Line { start: start as u32, len: (end - start) as u32, base: *starts as u16, open: *open };
        for &byte in &text[start..end] {
            match byte {
                LINK_START => {
                    if *starts < link_line.len() {
                        link_line[*starts] = *count as u16;
                    }
                    *starts += 1;
                    *open = true;
                }
                LINK_END => *open = false,
                _ => {}
            }
        }
        *count += 1;
    };
    let mut at = 0;
    while at < text.len() {
        let end = text[at..].iter().position(|&byte| byte == b'\n').map_or(text.len(), |p| at + p);
        if end == at {
            push(at, at, &mut count, &mut starts, &mut open);
        }
        let mut start = at;
        while start < end {
            let (mut column, mut cursor, mut space) = (0, start, None);
            while cursor < end && column < cols {
                let byte = text[cursor];
                if byte == LINK_START || byte == LINK_END {
                    cursor += 1;
                    continue;
                }
                if byte == b' ' {
                    space = Some(cursor);
                }
                cursor += next_char(text, cursor).1;
                column += 1;
            }
            if cursor < end {
                match space {
                    Some(gap) if gap > start => {
                        push(start, gap, &mut count, &mut starts, &mut open);
                        start = gap + 1;
                    }
                    _ => {
                        push(start, cursor, &mut count, &mut starts, &mut open);
                        start = cursor;
                    }
                }
            } else {
                push(start, end, &mut count, &mut starts, &mut open);
                start = end;
            }
        }
        at = end + 1;
    }
    count
}

struct History {
    url: [u8; MAX_URL],
    len: usize,
    top: usize,
    sel: usize,
}

enum Key {
    Char(char),
    Enter,
    Backspace,
    Tab,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Ctrl(u8),
}

/// The bytes `read_key` delivers as keys: plain characters, Ctrl letters and the cursor sequences (`ESC [ A`, `ESC O A`, `ESC [ 5 ~`).
struct Decoder {
    state: u8,
    number: u16,
}

impl Decoder {
    fn feed(&mut self, byte: u8) -> Option<Key> {
        match self.state {
            0 => match byte {
                0x1b => {
                    self.state = 1;
                    None
                }
                b'\r' | b'\n' => Some(Key::Enter),
                8 | 127 => Some(Key::Backspace),
                b'\t' => Some(Key::Tab),
                1..=26 => Some(Key::Ctrl(byte)),
                32..=126 => Some(Key::Char(char::from(byte))),
                _ => None,
            },
            1 => {
                self.state = match byte {
                    b'[' => {
                        self.number = 0;
                        2
                    }
                    b'O' => 3,
                    _ => 0,
                };
                None
            }
            2 => match byte {
                b'0'..=b'9' => {
                    self.number = self.number.saturating_mul(10).saturating_add(u16::from(byte - b'0'));
                    None
                }
                b';' => {
                    self.number = 0;
                    None
                }
                _ => {
                    self.state = 0;
                    match byte {
                        b'~' => match self.number {
                            1 | 7 => Some(Key::Home),
                            4 | 8 => Some(Key::End),
                            5 => Some(Key::PageUp),
                            6 => Some(Key::PageDown),
                            _ => None,
                        },
                        other => Self::arrow(other),
                    }
                }
            },
            _ => {
                self.state = 0;
                Self::arrow(byte)
            }
        }
    }

    fn arrow(byte: u8) -> Option<Key> {
        match byte {
            b'A' => Some(Key::Up),
            b'B' => Some(Key::Down),
            b'C' => Some(Key::Right),
            b'D' => Some(Key::Left),
            b'H' => Some(Key::Home),
            b'F' => Some(Key::End),
            _ => None,
        }
    }
}

struct Browser {
    api: &'static TapeApi,
    network: Net,
    resp: &'static mut [u8],
    cols: usize,
    rows: usize,
    cells: &'static mut [Cell],
    shown: &'static mut [Cell],
    url: [u8; MAX_URL],
    url_len: usize,
    nlines: usize,
    top: usize,
    sel: usize,
    message: [u8; 160],
    message_len: usize,
    history: [History; HISTORY],
    history_len: usize,
    prompt: Option<([u8; MAX_URL], usize)>,
}

impl Browser {
    fn body_rows(&self) -> usize {
        self.rows - 2
    }

    fn say(&mut self, text: &str) {
        let count = text.len().min(self.message.len());
        self.message[..count].copy_from_slice(&text.as_bytes()[..count]);
        self.message_len = count;
    }

    fn set(&mut self, col: usize, row: usize, ch: char, attr: u8) {
        if col < self.cols && row < self.rows {
            self.cells[row * self.cols + col] = Cell { ch, attr };
        }
    }

    fn text(&mut self, col: usize, row: usize, text: &str, attr: u8) -> usize {
        let mut at = col;
        for ch in text.chars() {
            self.set(at, row, ch, attr);
            at += 1;
        }
        at
    }

    fn link_visible(&self, index: usize) -> bool {
        let line = usize::from(link_line()[index]);
        line >= self.top && line < self.top + self.body_rows()
    }

    fn max_top(&self) -> usize {
        self.nlines.saturating_sub(self.body_rows())
    }

    fn select(&mut self, index: usize) {
        self.sel = index;
        let line = usize::from(link_line()[index]);
        if line < self.top {
            self.top = line;
        } else if line >= self.top + self.body_rows() {
            self.top = (line + 1 - self.body_rows()).min(self.max_top());
        }
    }

    /// After the view moved: keep the selected link only while it is on the screen, else take the first one that is.
    fn fix_selection(&mut self) {
        let count = page().link_count().min(512);
        if self.sel != NONE && self.sel < count && self.link_visible(self.sel) {
            return;
        }
        self.sel = (0..count).find(|&index| self.link_visible(index)).unwrap_or(NONE);
    }

    fn scroll(&mut self, delta: isize) {
        let top = (self.top as isize + delta).clamp(0, self.max_top() as isize);
        self.top = top as usize;
        self.fix_selection();
    }

    fn next_link(&mut self) {
        let count = page().link_count().min(512);
        let next = if self.sel == NONE { (0..count).find(|&i| usize::from(link_line()[i]) >= self.top) } else if self.sel + 1 < count { Some(self.sel + 1) } else { None };
        match next {
            Some(index) => self.select(index),
            None => self.scroll(1),
        }
    }

    fn previous_link(&mut self) {
        let count = page().link_count().min(512);
        let last = self.top + self.body_rows();
        let previous = if self.sel == NONE { (0..count).rev().find(|&i| usize::from(link_line()[i]) < last) } else if self.sel > 0 { Some(self.sel - 1) } else { None };
        match previous {
            Some(index) => self.select(index),
            None => self.scroll(-1),
        }
    }

    fn draw(&mut self) {
        self.cells.fill(BLANK);
        let cols = self.cols;
        let mut bar = [0u8; 256];
        let title;
        {
            let mut out = Out::new(&mut bar);
            let name = core::str::from_utf8(page().title()).unwrap_or("");
            let url = core::str::from_utf8(&self.url[..self.url_len]).unwrap_or("");
            let _ = if name.is_empty() { write!(out, " {url}") } else { write!(out, " {name}  {url}") };
            title = out.len();
        }
        let bar_text = core::str::from_utf8(&bar[..title]).unwrap_or("");
        for col in 0..cols {
            self.set(col, 0, ' ', A_BAR);
        }
        let mut col = 0;
        for ch in bar_text.chars().take(cols) {
            self.set(col, 0, ch, A_BAR);
            col += 1;
        }
        let text = page().text();
        for row in 0..self.body_rows() {
            let Some(line) = lines().get(self.top + row).filter(|_| self.top + row < self.nlines) else { break };
            let (mut at, mut in_link, mut number) = (line.start as usize, line.open, usize::from(line.base));
            let end = at + line.len as usize;
            let mut col = 0;
            while at < end {
                match text[at] {
                    LINK_START => {
                        number += 1;
                        in_link = true;
                        at += 1;
                    }
                    LINK_END => {
                        in_link = false;
                        at += 1;
                    }
                    _ => {
                        let (ch, used) = next_char(text, at);
                        let attr = if !in_link { A_NORMAL } else if number - 1 == self.sel { A_SELECTED } else { A_LINK };
                        self.set(col, row + 1, ch, attr);
                        col += 1;
                        at += used;
                    }
                }
            }
        }
        let status_row = self.rows - 1;
        let mut caret = (0, status_row);
        if let Some((entry, length)) = self.prompt {
            let shown = core::str::from_utf8(&entry[..length]).unwrap_or("");
            let after = self.text(0, status_row, "Go to: ", A_BAR);
            let end = self.text(after, status_row, shown, A_NORMAL);
            caret = (end.min(cols - 1), status_row);
        } else if self.message_len > 0 {
            let message = self.message;
            let shown = core::str::from_utf8(&message[..self.message_len]).unwrap_or("");
            self.text(0, status_row, shown, A_NORMAL);
        } else if self.sel != NONE {
            let target = core::str::from_utf8(page().link(self.sel).unwrap_or(b"")).unwrap_or("");
            self.text(0, status_row, target, A_LINK);
        } else {
            self.text(0, status_row, "Up/Down links  Right follow  Left back  g address  q quit", A_NORMAL);
        }
        self.flush(caret);
    }

    fn goto(&self, col: usize, row: usize) {
        out_str(self.api, "\x1b[");
        out_number(self.api, row + 1);
        out_byte(self.api, b';');
        out_number(self.api, col + 1);
        out_byte(self.api, b'H');
    }

    /// Sends only the cells that differ from the screen.
    fn flush(&mut self, caret: (usize, usize)) {
        let mut at: Option<(usize, usize)> = None;
        let mut attr = 255u8;
        out_str(self.api, "\x1b[?25l");
        for row in 0..self.rows {
            for col in 0..self.cols {
                let index = row * self.cols + col;
                let cell = self.cells[index];
                if cell == self.shown[index] {
                    continue;
                }
                self.shown[index] = cell;
                if at != Some((col, row)) {
                    self.goto(col, row);
                }
                if cell.attr != attr {
                    out_str(
                        self.api,
                        match cell.attr {
                            A_LINK => "\x1b[0;36m",
                            A_SELECTED => "\x1b[0;30;46m",
                            A_BAR => "\x1b[0;7m",
                            _ => "\x1b[0m",
                        },
                    );
                    attr = cell.attr;
                }
                let mut raw = [0u8; 4];
                cell.ch.encode_utf8(&mut raw).bytes().for_each(|byte| out_byte(self.api, byte));
                at = Some((col + 1, row));
            }
        }
        out_str(self.api, "\x1b[0m");
        self.goto(caret.0, caret.1);
        if self.prompt.is_some() {
            out_str(self.api, "\x1b[?25h");
        }
    }

    /// Fetches `target` and, when it is something to show, replaces the page. `push` keeps the old page for Back.
    fn visit(&mut self, target: &[u8], push: bool) -> bool {
        let mut url = [0u8; MAX_URL];
        if target.len() > MAX_URL {
            self.say("the address is too long");
            return false;
        }
        url[..target.len()].copy_from_slice(target);
        let mut length = target.len();
        self.say("Loading...");
        self.draw();
        let (count, truncated) = match load(self.api, &mut self.network, &mut url, &mut length, false, true, self.resp) {
            Ok(result) => result,
            Err(reason) => {
                self.say(reason);
                return false;
            }
        };
        let Some(head) = parse_head(&self.resp[..count]) else {
            self.say("not an HTTP answer");
            return false;
        };
        let Some(html) = is_html(head.content_type) else {
            let mut note = [0u8; 160];
            let mut out = Out::new(&mut note);
            let kind = core::str::from_utf8(head.content_type.unwrap_or(b"?")).unwrap_or("?");
            let _ = write!(out, "not text: {kind}, {} bytes (use web -c to print it)", count - head.header_len);
            let length = out.len();
            self.say(core::str::from_utf8(&note[..length]).unwrap_or("not text"));
            return false;
        };
        let (status, header_len) = (head.status, head.header_len);
        if push && self.url_len > 0 {
            self.remember();
        }
        let body = &self.resp[header_len..count];
        let page = page();
        if html { page.set_html(body) } else { page.set_plain(body) }
        self.url = url;
        self.url_len = length;
        self.nlines = wrap(self.cols);
        self.top = 0;
        self.sel = NONE;
        self.fix_selection();
        self.message_len = 0;
        if status != 200 || truncated {
            let mut note = [0u8; 64];
            let mut out = Out::new(&mut note);
            let _ = write!(out, "HTTP {status}{}", if truncated { " (page cut off)" } else { "" });
            let used = out.len();
            self.say(core::str::from_utf8(&note[..used]).unwrap_or(""));
        }
        true
    }

    fn remember(&mut self) {
        if self.history_len == HISTORY {
            self.history.rotate_left(1);
            self.history_len -= 1;
        }
        let entry = &mut self.history[self.history_len];
        entry.url = self.url;
        entry.len = self.url_len;
        entry.top = self.top;
        entry.sel = self.sel;
        self.history_len += 1;
    }

    fn back(&mut self) {
        if self.history_len == 0 {
            self.say("no earlier page");
            return;
        }
        let entry = &self.history[self.history_len - 1];
        let (url, len, top, sel) = (entry.url, entry.len, entry.top, entry.sel);
        if self.visit(&url[..len], false) {
            self.history_len -= 1;
            self.top = top.min(self.max_top());
            self.sel = NONE;
            if sel != NONE && sel < page().link_count().min(512) && self.link_visible(sel) {
                self.sel = sel;
            } else {
                self.fix_selection();
            }
        }
    }

    fn follow(&mut self) {
        if self.sel == NONE {
            self.say("no link selected (Up/Down choose one)");
            return;
        }
        let href = core::str::from_utf8(page().link(self.sel).unwrap_or(b"")).unwrap_or("");
        let mut absolute = [0u8; MAX_URL];
        let length = {
            let current = core::str::from_utf8(&self.url[..self.url_len]).unwrap_or("");
            let Ok(base) = parse_url(current) else {
                return self.say("the current address is not a web address");
            };
            let mut out = Out::new(&mut absolute);
            if resolve_link(&base, href, &mut out).is_err() {
                return self.say("the link address is too long");
            }
            out.len()
        };
        self.visit(&absolute[..length], true);
    }
}

fn out_str(api: &TapeApi, text: &str) {
    text.bytes().for_each(|byte| out_byte(api, byte));
}

fn out_number(api: &TapeApi, mut number: usize) {
    let mut digits = [0u8; 20];
    let mut count = 0;
    loop {
        digits[count] = b'0' + (number % 10) as u8;
        count += 1;
        number /= 10;
        if number == 0 {
            break;
        }
    }
    digits[..count].iter().rev().for_each(|&digit| out_byte(api, digit));
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

fn screen(api: &'static TapeApi, text: &str, start: &str) {
    let network = match bring_up(api, text) {
        Ok(network) => network,
        Err(reason) => return println!("web: {reason}."),
    };
    let (cols, rows) = tape_rt::console_size();
    let (cols, rows) = (cols.clamp(20, 256), rows.clamp(5, 100));
    let (Some(resp), Some(cells), Some(shown)) = (tape_rt::alloc_bytes(api, RESPONSE), alloc_cells(api, cols * rows), alloc_cells(api, cols * rows)) else {
        return println!("web: out of memory.");
    };
    let history = core::array::from_fn(|_| History { url: [0; MAX_URL], len: 0, top: 0, sel: NONE });
    let mut browser = Browser {
        api,
        network,
        resp,
        cols,
        rows,
        cells,
        shown,
        url: [0; MAX_URL],
        url_len: 0,
        nlines: 0,
        top: 0,
        sel: NONE,
        message: [0; 160],
        message_len: 0,
        history,
        history_len: 0,
        prompt: None,
    };
    page().clear();
    out_str(api, "\x1b[0m\x1b[2J");
    if start.is_empty() {
        browser.prompt = Some(([0; MAX_URL], 0));
        browser.say("Type an address and press Enter");
    } else if !browser.visit(start.as_bytes(), false) {
        browser.url[..start.len().min(MAX_URL)].copy_from_slice(&start.as_bytes()[..start.len().min(MAX_URL)]);
        browser.url_len = start.len().min(MAX_URL);
    }
    let mut decoder = Decoder { state: 0, number: 0 };
    let mut dirty = true;
    loop {
        let key = (api.read_key)();
        let mut event = None;
        if key == KEY_ABORT {
            if browser.prompt.take().is_none() {
                break;
            }
            browser.message_len = 0;
            dirty = true;
        } else if key >= KEY_BASE {
            event = decoder.feed((key - KEY_BASE) as u8);
        } else if !dirty {
            tape_rt::idle();
        }
        if let Some(event) = event {
            dirty = true;
            if browser.prompt.is_some() {
                browser.edit(event);
            } else {
                browser.message_len = 0;
                if browser.command(event) {
                    break;
                }
            }
        }
        if dirty {
            browser.draw();
            dirty = false;
        }
    }
    out_str(api, "\x1b[0m\x1b[2J\x1b[H\x1b[?25h");
}

impl Browser {
    /// A key in normal mode. `true` quits.
    fn command(&mut self, key: Key) -> bool {
        let page_rows = self.body_rows().saturating_sub(1).max(1) as isize;
        match key {
            Key::Char('q') | Key::Ctrl(17) => return true,
            Key::Char('g') | Key::Ctrl(7) => {
                let mut entry = [0u8; MAX_URL];
                entry[..self.url_len].copy_from_slice(&self.url[..self.url_len]);
                self.prompt = Some((entry, self.url_len));
            }
            Key::Down | Key::Tab => self.next_link(),
            Key::Up => self.previous_link(),
            Key::Right | Key::Enter => self.follow(),
            Key::Left | Key::Backspace => self.back(),
            Key::Char(' ') | Key::PageDown | Key::Char('f') => self.scroll(page_rows),
            Key::PageUp | Key::Char('b') => self.scroll(-page_rows),
            Key::Home => {
                self.top = 0;
                self.fix_selection();
            }
            Key::End => {
                self.top = self.max_top();
                self.fix_selection();
            }
            Key::Char('r') | Key::Ctrl(18) => {
                let (url, len) = (self.url, self.url_len);
                if len > 0 {
                    self.visit(&url[..len], false);
                }
            }
            _ => {}
        }
        false
    }

    /// A key while the address is being typed.
    fn edit(&mut self, key: Key) {
        let Some((mut entry, mut length)) = self.prompt else { return };
        match key {
            Key::Enter => {
                self.prompt = None;
                let text = entry;
                let length = text[..length].iter().position(|&b| b == 0).unwrap_or(length);
                if length > 0 {
                    self.visit(&text[..length], true);
                }
                return;
            }
            Key::Backspace => length = length.saturating_sub(1),
            Key::Ctrl(21) => length = 0,
            Key::Char(ch) if length < MAX_URL => {
                entry[length] = ch as u8;
                length += 1;
            }
            _ => {}
        }
        self.prompt = Some((entry, length));
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    tape_rt::init(api);
    let text = core::str::from_utf8(tape_rt::args()).unwrap_or("");
    let mut flags = Flags { include: false, head_only: false, follow: false, dump: false };
    let (mut command, mut url) = (false, "");
    for word in text.split_ascii_whitespace() {
        if let Some(letters) = word.strip_prefix('-').filter(|letters| !letters.is_empty()) {
            for letter in letters.chars() {
                match letter {
                    'c' => command = true,
                    'i' => (command, flags.include) = (true, true),
                    'I' => (command, flags.head_only) = (true, true),
                    'L' => (command, flags.follow) = (true, true),
                    'd' => (command, flags.dump) = (true, true),
                    _ => return println!("web: unknown option -{letter} (web --help)"),
                }
            }
        } else if !word.contains('=') || word.contains("://") || word.contains('/') {
            if url.is_empty() {
                url = word;
            }
        }
    }
    if command {
        if url.is_empty() {
            return println!("usage: tape run web -c[iILd] URL   (help: web --help)");
        }
        command_line(api, text, &flags, url);
    } else {
        screen(api, text, url);
    }
}
