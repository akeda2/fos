//! httpd: a very small web server, a bit like `python3 -m http.server`. GET and HEAD only, files from a folder (on the
//! boot volume or on another one, such as a stick), an optional directory listing, `index.html` for a folder, and a
//! redirect from `/dir` to `/dir/`. Every connection is answered once and closed (HTTP/1.0 style); there is no TLS and no
//! authentication, so it belongs on a trusted network. Quiet unless `log` is given; meant to run as a service
//! (`run httpd @HTTPD.CFG &2`), which then owns that NIC.
#![no_std]
#![no_main]

use tape_http::{Out, Parse, clean_path, is_blocked, listing_end, listing_entry, listing_start, message_page, mime, parse_request, response_head};
use tape_ip::net::{self, NetConfig};
use tape_ip::tcp::Server;
use tape_net::MacAddress;
use tape_rt::config::Config;
use tape_rt::{TapeApi, arg_value as value, println};

tape_rt::tape_help!(
    br#"httpd [@NAME] [root=DIR] [port=80] [listing=on|off] [index=index.html]
      [nic=N] [ip=A.B.C.D/N] [gateway=A.B.C.D] [log]
A tiny web server: GET and HEAD, files from DIR, nothing else. It runs as a
service: run httpd @HTTPD.CFG &2 (the service owns NIC 2; tape run httpd takes
the foreground and Escape stops it). Settings are key=value words on the
command line or in the file (the same words, one per line, # for comments).
root=   the folder served (default www on the boot volume). A stick or another
        volume works: root=disk2/www, root=@LABEL/site, root=@LABEL (all of it).
port=   TCP port (default 80).
listing=on   a folder without an index file shows its contents (default off:
        404). index= names the index file (default index.html).
ip=     a fixed address like 192.168.1.20/24 (gateway= for the router);
        without it DHCP. nic= is the NIC number from 1 (default: nic use).
log     print one line per request (quiet otherwise).
/dir redirects to /dir/. The machine's own files (synckey, settings, start,
hostname, debug, config/) are never served. One request per connection, no
TLS, no authentication: use it on a network you trust.
"#
);

const SLOTS: usize = 4;
/// Each connection's receive buffer and window: a request head must fit in it.
const RX: usize = 4096;
/// Room in front of the body buffer for the response head, so both leave in the same segments (a head alone would wait
/// for the peer's delayed acknowledgement).
const HEAD_ROOM: usize = 512;
const BODY: usize = 32768;
const SEND_TIMEOUT: u64 = 5_000_000;

static mut SERVER: Server<RX, SLOTS> = Server::new();
static mut BUFFER: [u8; HEAD_ROOM + BODY] = [0; HEAD_ROOM + BODY];

struct Settings<'a> {
    root: &'a str,
    index: &'a str,
    listing: bool,
    log: bool,
}

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    tape_rt::init(api);
    let config = match Config::load(api, "HTTPD", core::str::from_utf8(tape_rt::args()).unwrap_or("")) {
        Ok(config) => config,
        Err(reason) => return println!("httpd: {reason}."),
    };
    let text = config.text();
    let settings = Settings {
        root: value(text, "root").unwrap_or("www").trim_end_matches('/'),
        index: value(text, "index").unwrap_or("index.html"),
        listing: value(text, "listing").is_some_and(|word| matches!(word, "on" | "yes" | "1")),
        log: text.split_ascii_whitespace().any(|word| word == "log" || word == "log=on"),
    };
    let Some(port) = value(text, "port").map_or(Some(80), |word| word.parse::<u16>().ok().filter(|&port| port != 0)) else {
        return println!("httpd: port= takes a number from 1 to 65535.");
    };
    let nic = value(text, "nic").and_then(|word| word.parse::<usize>().ok()).map_or(tape_rt::active_nic(), |n| n.saturating_sub(1));
    if (api.nic_count)() <= nic {
        return println!("httpd: this machine has no NIC {}.", nic + 1);
    }
    let mut mac = [0u8; 6];
    (api.nic_mac)(nic, mac.as_mut_ptr());
    let mac = MacAddress(mac);
    let network: NetConfig = match value(text, "ip") {
        Some(ip) => match net::static_config(mac, ip, value(text, "gateway").unwrap_or("")) {
            Ok(network) => network,
            Err(reason) => return println!("httpd: {reason}."),
        },
        None => match net::dhcp(api, nic, mac) {
            Some(network) => network,
            None => return println!("httpd: no DHCP lease (give ip=A.B.C.D/N for a fixed address)."),
        },
    };
    #[allow(static_mut_refs)]
    let (server, buffer) = unsafe { (&mut *core::ptr::addr_of_mut!(SERVER), &mut *core::ptr::addr_of_mut!(BUFFER)) };
    server.start(nic, &network, port);
    println!("httpd: NIC {} at {}:{port}, serving {}.", nic + 1, format_args!("{}.{}.{}.{}", network.ip.0[0], network.ip.0[1], network.ip.0[2], network.ip.0[3]), if settings.root.is_empty() { "the boot volume" } else { settings.root });
    loop {
        if (api.should_abort)() != 0 {
            break;
        }
        let busy = server.poll(api, 16);
        for slot in 0..SLOTS {
            handle(api, server, buffer, slot, &settings);
        }
        if !busy {
            tape_rt::idle();
        }
    }
}

/// Answers the request waiting on `slot`, if a complete one has arrived.
fn handle(api: &TapeApi, server: &mut Server<RX, SLOTS>, buffer: &mut [u8], slot: usize, settings: &Settings) {
    if !server.is_open(slot) || server.received(slot).is_empty() {
        return;
    }
    // The request is copied out of the connection, which the response needs for itself.
    let mut head = [0u8; RX];
    let waiting = server.received(slot).len().min(RX);
    head[..waiting].copy_from_slice(&server.received(slot)[..waiting]);
    let (method, target) = match parse_request(&head[..waiting]) {
        Parse::Incomplete if waiting + 64 < RX => return,
        Parse::Incomplete => return refuse(api, server, buffer, slot, settings, b"-", b"-", 414),
        Parse::Bad => return refuse(api, server, buffer, slot, settings, b"-", b"-", 400),
        Parse::Done(request) => (request.method, request.target),
    };
    if method != b"GET" && method != b"HEAD" {
        return refuse(api, server, buffer, slot, settings, &method[..method.len().min(8)], &target[..target.len().min(200)], 405);
    }
    // Whatever else the peer sent (a body, pipelined requests) is not wanted.
    let mut sink = [0u8; 256];
    while !server.received(slot).is_empty() {
        server.take(slot, &mut sink);
    }
    serve(api, server, buffer, slot, settings, method == b"HEAD", method, target);
}

fn refuse(api: &TapeApi, server: &mut Server<RX, SLOTS>, buffer: &mut [u8], slot: usize, settings: &Settings, method: &[u8], target: &[u8], status: u16) {
    let mut sink = [0u8; 256];
    while !server.received(slot).is_empty() {
        server.take(slot, &mut sink);
    }
    reply(api, server, buffer, slot, settings, method, target, status);
}

/// Where a request path lives: `root`, `path` and `extra` joined with slashes (empty parts leave no slash).
fn join<'a>(out: &'a mut [u8], root: &str, path: &[u8], extra: &str) -> Option<&'a str> {
    use core::fmt::Write;
    let mut writer = Out::new(out);
    writer.write_str(root).ok()?;
    if !path.is_empty() {
        if !writer.is_empty() {
            writer.write_str("/").ok()?;
        }
        writer.write_str(core::str::from_utf8(path).ok()?).ok()?;
    }
    if !extra.is_empty() {
        if !writer.is_empty() {
            writer.write_str("/").ok()?;
        }
        writer.write_str(extra).ok()?;
    }
    let length = writer.len();
    core::str::from_utf8(&out[..length]).ok()
}

/// What is at `reference` (a file reference or a folder): (is a folder, length).
fn stat(api: &TapeApi, reference: &str) -> Option<(bool, u64)> {
    let (parent, name) = match reference.rfind('/') {
        Some(slash) => (&reference[..slash], &reference[slash + 1..]),
        None => ("", reference),
    };
    for index in 0..8192 {
        match tape_rt::list_file(api, parent, index) {
            Ok(Some(info)) if info.name() == name.as_bytes() => return Some((info.flags & tape_rt::FILE_DIRECTORY != 0, info.length)),
            Ok(Some(_)) => {}
            _ => return None,
        }
    }
    None
}

fn is_folder(api: &TapeApi, reference: &str) -> bool {
    matches!(tape_rt::list_file(api, reference, 0), Ok(_))
}

fn serve(api: &TapeApi, server: &mut Server<RX, SLOTS>, buffer: &mut [u8], slot: usize, settings: &Settings, head_only: bool, method: &[u8], target: &[u8]) {
    let mut path_buffer = [0u8; 256];
    let Some((path_length, trailing)) = clean_path(target, &mut path_buffer) else { return reply(api, server, buffer, slot, settings, method, target, 400) };
    let path = &path_buffer[..path_length];
    if is_blocked(path) {
        return reply(api, server, buffer, slot, settings, method, target, 404);
    }
    let mut reference_buffer = [0u8; 400];
    let Some(reference) = join(&mut reference_buffer, settings.root, path, "") else { return reply(api, server, buffer, slot, settings, method, target, 414) };
    let found = if path.is_empty() { is_folder(api, reference).then_some((true, 0)) } else { stat(api, reference) };
    let Some((folder, length)) = found else { return reply(api, server, buffer, slot, settings, method, target, 404) };
    if folder {
        if !trailing {
            let mut location = [0u8; 400];
            let cut = target.iter().position(|&b| b == b'?' || b == b'#').unwrap_or(target.len());
            if cut >= location.len() - 2 {
                return reply(api, server, buffer, slot, settings, method, target, 414);
            }
            location[..cut].copy_from_slice(&target[..cut]);
            location[cut] = b'/';
            return redirect(api, server, buffer, slot, settings, method, target, &location[..cut + 1]);
        }
        let mut index_buffer = [0u8; 400];
        if let Some(index) = join(&mut index_buffer, settings.root, path, settings.index) {
            if let Some((false, length)) = stat(api, index) {
                return send_file(api, server, buffer, slot, settings, head_only, method, target, index, settings.index.as_bytes(), length);
            }
        }
        if settings.listing {
            return send_listing(api, server, buffer, slot, settings, head_only, method, target, reference, path);
        }
        return reply(api, server, buffer, slot, settings, method, target, 404);
    }
    if trailing {
        return reply(api, server, buffer, slot, settings, method, target, 404);
    }
    let name = path.rsplit(|&b| b == b'/').next().unwrap_or(path);
    send_file(api, server, buffer, slot, settings, head_only, method, target, reference, name, length);
}

fn finish(api: &TapeApi, server: &mut Server<RX, SLOTS>, slot: usize, settings: &Settings, method: &[u8], target: &[u8], status: u16, sent: u64) {
    server.close(api, slot);
    if settings.log {
        let (ip, _) = server.peer(slot);
        println!("httpd: {}.{}.{}.{} {} {} {status} {sent}", ip.0[0], ip.0[1], ip.0[2], ip.0[3], core::str::from_utf8(method).unwrap_or("?"), core::str::from_utf8(target).unwrap_or("?"));
    }
}

/// A short page for an error status.
fn reply(api: &TapeApi, server: &mut Server<RX, SLOTS>, buffer: &mut [u8], slot: usize, settings: &Settings, method: &[u8], target: &[u8], status: u16) {
    let mut page = [0u8; 200];
    let mut body = Out::new(&mut page);
    let _ = message_page(&mut body, status);
    let body_length = body.len();
    let mut head = Out::new(&mut buffer[..HEAD_ROOM]);
    let _ = response_head(&mut head, status, "text/html", body_length as u64, None);
    let head_length = head.len();
    buffer[head_length..head_length + body_length].copy_from_slice(&page[..body_length]);
    let total = head_length + body_length;
    let _ = server.send(api, slot, &buffer[..total], SEND_TIMEOUT);
    finish(api, server, slot, settings, method, target, status, 0);
}

fn redirect(api: &TapeApi, server: &mut Server<RX, SLOTS>, buffer: &mut [u8], slot: usize, settings: &Settings, method: &[u8], target: &[u8], location: &[u8]) {
    let (head_area, _) = buffer.split_at_mut(HEAD_ROOM);
    let mut head = Out::new(head_area);
    let _ = response_head(&mut head, 301, "text/html", 0, Some(location));
    let total = head.len();
    let _ = server.send(api, slot, &buffer[..total], SEND_TIMEOUT);
    finish(api, server, slot, settings, method, target, 301, 0);
}

fn send_listing(api: &TapeApi, server: &mut Server<RX, SLOTS>, buffer: &mut [u8], slot: usize, settings: &Settings, head_only: bool, method: &[u8], target: &[u8], reference: &str, path: &[u8]) {
    let (head_area, body_area) = buffer.split_at_mut(HEAD_ROOM);
    let mut body = Out::new(body_area);
    let _ = listing_start(&mut body, path);
    for index in 0..8192 {
        let Ok(Some(info)) = tape_rt::list_file(api, reference, index) else { break };
        if is_blocked_entry(path, info.name()) {
            continue;
        }
        if listing_entry(&mut body, info.name(), info.flags & tape_rt::FILE_DIRECTORY != 0, info.length).is_err() {
            break;
        }
    }
    let _ = listing_end(&mut body);
    let body_length = body.len();
    let mut head = [0u8; 256];
    let mut writer = Out::new(&mut head);
    let _ = response_head(&mut writer, 200, "text/html; charset=utf-8", body_length as u64, None);
    let head_length = writer.len();
    head_area[HEAD_ROOM - head_length..].copy_from_slice(&head[..head_length]);
    let end = if head_only { HEAD_ROOM } else { HEAD_ROOM + body_length };
    let _ = server.send(api, slot, &buffer[HEAD_ROOM - head_length..end], SEND_TIMEOUT);
    finish(api, server, slot, settings, method, target, 200, body_length as u64);
}

/// Blocked names stay out of a listing too.
fn is_blocked_entry(folder: &[u8], name: &[u8]) -> bool {
    let mut joined = [0u8; 320];
    let length = folder.len().min(150);
    joined[..length].copy_from_slice(&folder[..length]);
    let mut at = length;
    if at > 0 {
        joined[at] = b'/';
        at += 1;
    }
    let name = &name[..name.len().min(joined.len() - at)];
    joined[at..at + name.len()].copy_from_slice(name);
    is_blocked(&joined[..at + name.len()])
}

fn send_file(api: &TapeApi, server: &mut Server<RX, SLOTS>, buffer: &mut [u8], slot: usize, settings: &Settings, head_only: bool, method: &[u8], target: &[u8], reference: &str, name: &[u8], length: u64) {
    let mut head = [0u8; 256];
    let mut writer = Out::new(&mut head);
    let _ = response_head(&mut writer, 200, mime(name), length, None);
    let head_length = writer.len();
    buffer[HEAD_ROOM - head_length..HEAD_ROOM].copy_from_slice(&head[..head_length]);
    if head_only {
        let _ = server.send(api, slot, &buffer[HEAD_ROOM - head_length..HEAD_ROOM], SEND_TIMEOUT);
        return finish(api, server, slot, settings, method, target, 200, 0);
    }
    let mut offset = 0u64;
    let mut first = true;
    while offset < length {
        let (_, body) = buffer.split_at_mut(HEAD_ROOM);
        let read = match tape_rt::read_file_at(reference, offset, body) {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        let start = if first { HEAD_ROOM - head_length } else { HEAD_ROOM };
        if server.send(api, slot, &buffer[start..HEAD_ROOM + read], SEND_TIMEOUT).is_err() {
            break;
        }
        first = false;
        offset += read as u64;
    }
    if first {
        // Nothing could be read after all (the file went away): the head promised a body, so just hang up.
        let _ = server.send(api, slot, &buffer[HEAD_ROOM - head_length..HEAD_ROOM], SEND_TIMEOUT);
    }
    finish(api, server, slot, settings, method, target, 200, offset);
}
