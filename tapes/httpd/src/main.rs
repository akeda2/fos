//! httpd: a small web server for big files. GET and HEAD, files from a folder on any volume (FOSFS, FAT, ext4: it reads
//! through the `vfs_*` calls), an optional directory listing, `index.html` for a folder, and a redirect from `/dir` to
//! `/dir/`. HTTP/1.1 with keep-alive, `Range` (a download resumes, a player seeks), `If-Range` and conditional requests
//! (`ETag`, `Last-Modified`, 304 where the volume keeps times: ext). There is no TLS and no authentication: put a
//! reverse proxy in front for those. Quiet unless `log` is given; meant to run as a service (`run httpd @HTTPD.CFG &2`),
//! which then owns that NIC.
//!
//! The network thread owns the NIC and every connection, which is a small state machine (request, head, body, drain), so a
//! slow client never holds up another. A body leaves in 128 KiB chunks. On the server kernel (tape threads) worker threads
//! read the next chunk of every download while the network thread sends the current one (what `smbd` does for SMB); where
//! there are no threads the same code reads each chunk in the loop.
#![no_std]
#![no_main]

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use tape_http::{Out, Parse, RangeOutcome, Request, Response, clean_path, is_blocked, listing_end, listing_entry, listing_start, message_page, mime, not_modified, parse_range, parse_request, range_applies, write_response};
use tape_ip::net::{self, NetConfig};
use tape_ip::tcp::Server;
use tape_net::MacAddress;
use tape_rt::config::Config;
use tape_rt::{TapeApi, VFS_DIR, VFS_FILE, VfsEntry, VfsStat, arg_value as value, println};

tape_rt::tape_help!(
    br#"httpd [@NAME] [root=DIR] [port=80] [listing=on|off] [index=index.html]
      [slots=N] [nic=N] [ip=A.B.C.D/N] [gateway=A.B.C.D] [log]
A small web server for big files: GET and HEAD, files from DIR, nothing else.
It runs as a service: run httpd @HTTPD.CFG &2 (the service owns NIC 2; tape
run httpd takes the foreground and Escape stops it). Settings are key=value
words on the command line or in the file (one per line, # for comments).
root=   the folder served (default www on the boot volume). A stick or another
        volume works: root=disk2/www, root=@LABEL/site, root=@LABEL (all of it).
        An ext4 volume works too.
port=   TCP port (default 80).
listing=on   a folder without an index file shows its contents (default off:
        404). index= names the index file (default index.html).
slots=  how many connections at once: 4, 8 or 16 (default 16 where the tape
        has threads, else 8).
ip=     a fixed address like 192.168.1.20/24 (gateway= for the router);
        without it DHCP. nic= is the NIC number from 1 (default: nic use).
log     print one line per request (quiet otherwise).
HTTP/1.1 keep-alive, Range and conditional requests (ETag/Last-Modified on ext
volumes). /dir redirects to /dir/. The machine's own files (synckey, settings,
start, hostname, debug, config/) are never served. No TLS and no
authentication: put a reverse proxy in front for those.
"#
);

/// Each connection's receive buffer and window: a request head must fit in it.
const RX: usize = 4096;
/// Room for a response head, and for the small page of an error answer behind it.
const HEAD: usize = 1024;
const PATH: usize = 400;
/// One read, one chunk of a body; a connection holds two of them with workers (one being sent, one being read), else one.
const CHUNK: usize = 128 * 1024;
const MAX_BUFS: usize = 32;
const WORKERS: usize = 4;
/// A keep-alive connection may sit idle this long; a SYN that finds every slot taken still evicts the quietest one.
const KEEP_ALIVE_US: u64 = 120_000_000;
/// A peer that takes no byte of an answer for this long is dropped.
const STALL_US: u64 = 30_000_000;
/// Poll this long after the last frame before sleeping (the server kernel's idle sleeps a whole tick).
const SPIN_US: u64 = 2_000;

const REQUEST: u8 = 0;
const SEND_HEAD: u8 = 1;
const SEND_BODY: u8 = 2;
const DRAIN: u8 = 3;
const CLOSING: u8 = 4;

const EMPTY: u8 = 0;
const PENDING: u8 = 1;
const READY: u8 = 2;

const IDLE: u32 = 0;
const JOB: u32 = 1;
const DONE: u32 = 2;
const FAILED: usize = usize::MAX;

struct Settings<'a> {
    root: &'a str,
    index: &'a str,
    listing: bool,
    log: bool,
}

/// One connection's place in its answer. All-zero is a valid, idle connection.
struct Conn {
    phase: u8,
    keep_alive: bool,
    head_only: bool,
    ahead_state: u8,
    peer_ip: [u8; 4],
    peer_port: u16,
    status: u16,
    head: [u8; HEAD],
    head_len: usize,
    head_sent: usize,
    /// The file being sent: next byte to read, end of the range (exclusive), bytes handed to TCP so far.
    path: [u8; PATH],
    path_len: usize,
    next: u64,
    end: u64,
    sent: u64,
    /// Buffers of the shared pool plus one (0: none): `cur` is being sent, `ahead` is being (or has been) read.
    cur: u8,
    ahead: u8,
    cur_pos: usize,
    ahead_want: usize,
    progress: u64,
    method_len: u8,
    method: [u8; 8],
    target_len: u8,
    target: [u8; 96],
}

struct Buf {
    data: usize,
    len: usize,
    in_use: bool,
    /// Its connection is gone but a worker still reads into it: handed back when the worker is done.
    orphan: bool,
    /// The worker (plus one) that reads into it, 0 if none.
    worker: u8,
}

/// A read job, shared between the network thread and one worker; all-zero is valid. `state`: IDLE -> JOB (network thread
/// filled the rest in) -> DONE (worker stored `length`, `FAILED` for an error) -> IDLE (network thread took the answer).
struct Worker {
    state: AtomicU32,
    length: AtomicUsize,
    path: [u8; PATH],
    path_len: usize,
    offset: u64,
    data: usize,
    want: usize,
}

struct Shared<'a> {
    settings: Settings<'a>,
    bufs: [Buf; MAX_BUFS],
    buf_count: usize,
    workers: [usize; WORKERS],
    worker_count: usize,
}

/// Everything per connection, allocated from the kernel (all-zero is valid).
struct Http<const N: usize> {
    server: Server<RX, N>,
    conns: [Conn; N],
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
    let threads = tape_rt::threads_supported();
    let slots = match value(text, "slots") {
        None => if threads { 16 } else { 8 },
        Some("4") => 4,
        Some("8") => 8,
        Some("16") => 16,
        Some(_) => return println!("httpd: slots= is 4, 8 or 16."),
    };
    let nic = value(text, "nic").and_then(|word| word.parse::<usize>().ok()).map_or(tape_rt::active_nic(), |n| n.saturating_sub(1));
    if (api.nic_count)() <= nic {
        return println!("httpd: this machine has no NIC {}.", nic + 1);
    }
    if api.version < 32 {
        return println!("httpd: this kernel has no vfs calls (tape ABI 32).");
    }
    let mut mac = [0u8; 6];
    (api.nic_mac)(nic, mac.as_mut_ptr());
    let mac = MacAddress(mac);
    let mut lease = None;
    let network: NetConfig = match value(text, "ip") {
        Some(ip) => match net::static_config(mac, ip, value(text, "gateway").unwrap_or("")) {
            Ok(network) => network,
            Err(reason) => return println!("httpd: {reason}."),
        },
        None => match net::DhcpTask::bind(api, nic, mac) {
            Some((task, network)) => {
                lease = Some(task);
                network
            }
            None => return println!("httpd: no DHCP lease (give ip=A.B.C.D/N for a fixed address)."),
        },
    };
    let mut shared = Shared { settings, bufs: [const { Buf { data: 0, len: 0, in_use: false, orphan: false, worker: 0 } }; MAX_BUFS], buf_count: 0, workers: [0; WORKERS], worker_count: 0 };
    if threads {
        start_workers(api, &mut shared);
    }
    let per_conn = if shared.worker_count > 0 { 2 } else { 1 };
    // The biggest table that fits, down from the one asked for.
    let started = match slots {
        16 => start::<16>(api, &mut shared, &mut lease, nic, &network, port, per_conn) || start::<8>(api, &mut shared, &mut lease, nic, &network, port, per_conn) || start::<4>(api, &mut shared, &mut lease, nic, &network, port, per_conn),
        8 => start::<8>(api, &mut shared, &mut lease, nic, &network, port, per_conn) || start::<4>(api, &mut shared, &mut lease, nic, &network, port, per_conn),
        _ => start::<4>(api, &mut shared, &mut lease, nic, &network, port, per_conn),
    };
    if !started {
        println!("httpd: out of memory.");
    }
}

/// Allocates `N` connections and their buffers, announces and serves until aborted; false if the memory is not there.
fn start<const N: usize>(api: &'static TapeApi, shared: &mut Shared, lease: &mut Option<net::DhcpTask>, nic: usize, network: &NetConfig, port: u16, per_conn: usize) -> bool {
    let Some(http) = tape_rt::alloc_zeroed::<Http<N>>(api) else { return false };
    let wanted = (N * per_conn).min(MAX_BUFS);
    while shared.buf_count < wanted {
        let address = (api.alloc)(CHUNK);
        if address == 0 {
            break;
        }
        shared.bufs[shared.buf_count].data = address;
        shared.buf_count += 1;
    }
    if shared.buf_count < per_conn {
        return false;
    }
    http.server.set_idle(KEEP_ALIVE_US);
    http.server.start(nic, network, port);
    let ip = network.ip.0;
    println!(
        "httpd: NIC {} at {}.{}.{}.{}:{port}, serving {}, {N} connections, {}.",
        nic + 1,
        ip[0],
        ip[1],
        ip[2],
        ip[3],
        if shared.settings.root.is_empty() { "the boot volume" } else { shared.settings.root },
        if shared.worker_count > 0 { "worker threads" } else { "no threads" }
    );
    run(api, http, shared, lease, nic, port);
    true
}

fn run<const N: usize>(api: &TapeApi, http: &mut Http<N>, shared: &mut Shared, lease: &mut Option<net::DhcpTask>, nic: usize, port: u16) {
    let Http { server, conns } = http;
    let mut last_busy = tape_rt::micros();
    loop {
        if (api.should_abort)() != 0 {
            break;
        }
        let frames = server.poll(api, 16);
        let now = tape_rt::micros();
        if let Some(task) = lease.as_mut() {
            match task.maintain(api, server.take_dhcp()) {
                net::Lease::Held => {}
                net::Lease::Renewed => println!("httpd: DHCP lease renewed."),
                net::Lease::Replaced(Some(network)) => {
                    // Open connections die with the old address; listen again on the new one.
                    server.start(nic, &network, port);
                    let ip = network.ip.0;
                    println!("httpd: DHCP gave another address, now serving on {}.{}.{}.{}:{port}.", ip[0], ip[1], ip[2], ip[3]);
                }
                net::Lease::Replaced(None) => println!("httpd: DHCP lease lost, asking again."),
            }
        }
        shared.reap();
        let mut work = false;
        for slot in 0..N {
            work |= step(api, server, &mut conns[slot], shared, slot, now);
        }
        let reading = conns.iter().any(|conn| conn.ahead_state == PENDING);
        if frames || work || reading {
            last_busy = now;
        } else if now - last_busy > SPIN_US {
            tape_rt::idle();
        }
    }
}

/// Moves one connection along as far as it goes without waiting; true if anything happened.
fn step<const N: usize>(api: &TapeApi, server: &mut Server<RX, N>, conn: &mut Conn, shared: &mut Shared, slot: usize, now: u64) -> bool {
    if !server.is_open(slot) {
        conn.release(shared);
        conn.peer_port = 0;
        return false;
    }
    let (ip, port) = server.peer(slot);
    if conn.peer_port != port || conn.peer_ip != ip.0 {
        // A new connection took the slot.
        conn.release(shared);
        conn.peer_ip = ip.0;
        conn.peer_port = port;
    }
    let mut work = false;
    if conn.phase == REQUEST {
        work |= request(api, server, conn, shared, slot, now);
    }
    if conn.phase == SEND_HEAD {
        if conn.head_sent < conn.head_len {
            if let Ok(count) = server.send_some(api, slot, &conn.head[conn.head_sent..conn.head_len]) {
                if count > 0 {
                    conn.head_sent += count;
                    conn.progress = now;
                    work = true;
                }
            }
        }
        if conn.head_sent >= conn.head_len {
            conn.phase = if conn.cur != 0 { SEND_BODY } else { DRAIN };
            conn.progress = now;
        }
    }
    if conn.phase == SEND_BODY {
        work |= body(api, server, conn, shared, slot, now);
    }
    if conn.phase == DRAIN && server.in_flight(slot) == 0 {
        conn.log(&shared.settings);
        conn.release(shared);
        if conn.keep_alive && !server.peer_closed(slot) {
            conn.phase = REQUEST;
        } else {
            server.finish(api, slot);
            conn.phase = CLOSING;
        }
        work = true;
    }
    if conn.phase != REQUEST && conn.phase != CLOSING && now - conn.progress > STALL_US {
        server.recycle(slot);
        work = true;
    }
    work
}

/// Reads and answers the request waiting on the slot, if a whole one has arrived. True if it did something.
fn request<const N: usize>(api: &TapeApi, server: &mut Server<RX, N>, conn: &mut Conn, shared: &mut Shared, slot: usize, now: u64) -> bool {
    let waiting = server.received(slot).len().min(RX);
    if waiting == 0 || (waiting + 64 < RX && !head_complete(server.received(slot))) {
        if server.peer_closed(slot) {
            server.finish(api, slot);
            conn.phase = CLOSING;
            return true;
        }
        return false;
    }
    // The request is copied out of the connection, which the answer needs for itself.
    let mut head = [0u8; RX];
    head[..waiting].copy_from_slice(&server.received(slot)[..waiting]);
    let consumed = match parse_request(&head[..waiting]) {
        Parse::Incomplete => {
            conn.head_only = false;
            conn.error(414, false, None, b"-", b"-");
            Some(usize::MAX)
        }
        Parse::Bad => {
            conn.head_only = false;
            conn.error(400, false, None, b"-", b"-");
            Some(usize::MAX)
        }
        Parse::Done(request) => {
            conn.status = 0;
            conn.keep_alive = request.keep_alive;
            conn.head_only = request.method == b"HEAD";
            conn.set_log(request.method, request.target);
            if respond(conn, shared, &request) {
                Some(if conn.keep_alive { request.head_len } else { usize::MAX })
            } else {
                None
            }
        }
    };
    let Some(mut left) = consumed else { return false };
    let mut sink = [0u8; 256];
    while left > 0 && !server.received(slot).is_empty() {
        let size = left.min(sink.len());
        let count = server.take(api, slot, &mut sink[..size]);
        if count == 0 {
            break;
        }
        left -= count;
    }
    conn.phase = SEND_HEAD;
    conn.progress = now;
    true
}

fn head_complete(received: &[u8]) -> bool {
    received.windows(4).any(|window| window == b"\r\n\r\n")
}

/// Sets up the answer to `request` in `conn`. False means "not now" (no chunk buffer free): the request stays unread.
fn respond(conn: &mut Conn, shared: &mut Shared, request: &Request) -> bool {
    let (method, target) = (request.method, request.target);
    if method != b"GET" && method != b"HEAD" {
        conn.error(405, false, None, method, target);
        return true;
    }
    let mut path_buffer = [0u8; 256];
    let Some((path_length, trailing)) = clean_path(target, &mut path_buffer) else { return conn.error(400, conn.keep_alive, None, method, target) };
    let path = &path_buffer[..path_length];
    if is_blocked(path) {
        return conn.error(404, conn.keep_alive, None, method, target);
    }
    let (root, index_name, listing) = (shared.settings.root, shared.settings.index, shared.settings.listing);
    let mut reference_buffer = [0u8; PATH];
    let Some(reference) = join(&mut reference_buffer, root, path, "") else { return conn.error(414, conn.keep_alive, None, method, target) };
    let Some(found) = lookup(reference) else { return conn.error(404, conn.keep_alive, None, method, target) };
    if found.kind == VFS_DIR {
        if !trailing {
            let mut location = [0u8; PATH];
            let cut = target.iter().position(|&b| b == b'?' || b == b'#').unwrap_or(target.len());
            if cut >= location.len() - 2 {
                return conn.error(414, conn.keep_alive, None, method, target);
            }
            location[..cut].copy_from_slice(&target[..cut]);
            location[cut] = b'/';
            let mut response = Response::new(301, "text/html", 0, conn.keep_alive);
            response.location = Some(&location[..cut + 1]);
            conn.set_head(&response);
            return true;
        }
        let mut index_buffer = [0u8; PATH];
        if let Some(index) = join(&mut index_buffer, root, path, index_name) {
            if let Some(stat) = lookup(index).filter(|stat| stat.kind == VFS_FILE) {
                return send_file(conn, shared, request, index, index_name.as_bytes(), &stat);
            }
        }
        if listing {
            return send_listing(conn, shared, reference, path);
        }
        return conn.error(404, conn.keep_alive, None, method, target);
    }
    if found.kind != VFS_FILE || trailing {
        return conn.error(404, conn.keep_alive, None, method, target);
    }
    let name = path.rsplit(|&b| b == b'/').next().unwrap_or(path);
    send_file(conn, shared, request, reference, name, &found)
}

/// What is at `reference`; an empty one is the boot volume's root.
fn lookup(reference: &str) -> Option<VfsStat> {
    if reference.is_empty() {
        return Some(VfsStat { kind: VFS_DIR, ..VfsStat::default() });
    }
    tape_rt::vfs_stat(reference).ok()
}

fn send_file(conn: &mut Conn, shared: &mut Shared, request: &Request, reference: &str, name: &[u8], stat: &VfsStat) -> bool {
    let (size, mtime) = (stat.size, stat.mtime);
    let keep_alive = conn.keep_alive;
    let file = Some((size, mtime));
    if not_modified(request, size, mtime) {
        let mut response = Response::new(304, mime(name), 0, keep_alive);
        response.file = file;
        conn.set_head(&response);
        return true;
    }
    let (mut first, mut last, mut status) = (0, size.saturating_sub(1), 200);
    if let Some(spec) = request.range.filter(|_| !conn.head_only && range_applies(request, size, mtime)) {
        match parse_range(spec, size) {
            RangeOutcome::Whole => {}
            RangeOutcome::Partial(a, b) => (first, last, status) = (a, b, 206),
            RangeOutcome::Unsatisfiable => return conn.error(416, keep_alive, Some((size, 0)), request.method, request.target),
        }
    }
    let length = if size == 0 { 0 } else { last - first + 1 };
    let wants_body = !conn.head_only && length > 0;
    if wants_body && conn.take_buffers(shared).is_none() {
        return false;
    }
    let mut response = Response::new(status, mime(name), length, keep_alive);
    response.file = file;
    if status == 206 {
        response.content_range = Some((first, last, size));
    }
    conn.set_head(&response);
    if wants_body {
        let bytes = reference.as_bytes();
        conn.path[..bytes.len()].copy_from_slice(bytes);
        conn.path_len = bytes.len();
        conn.next = first;
        conn.end = last + 1;
    }
    true
}

fn send_listing(conn: &mut Conn, shared: &mut Shared, reference: &str, path: &[u8]) -> bool {
    if !conn.head_only && conn.take_buffers(shared).is_none() {
        return false;
    }
    let mut length = 0;
    if !conn.head_only {
        let buf = &mut shared.bufs[conn.cur as usize - 1];
        let mut body = Out::new(unsafe { core::slice::from_raw_parts_mut(buf.data as *mut u8, CHUNK) });
        let _ = listing_start(&mut body, path);
        for index in 0..8192 {
            let Ok(Some(entry)) = tape_rt::vfs_read_dir(reference, index) else { break };
            let name = entry_name(&entry);
            if is_blocked_entry(path, name) {
                continue;
            }
            if listing_entry(&mut body, name, entry.stat.kind == VFS_DIR, entry.stat.size).is_err() {
                break;
            }
        }
        let _ = listing_end(&mut body);
        length = body.len();
        buf.len = length;
        conn.cur_pos = 0;
        conn.next = 0;
        conn.end = 0;
    }
    let response = Response::new(200, "text/html; charset=utf-8", length as u64, conn.keep_alive);
    conn.set_head(&response);
    true
}

fn entry_name(entry: &VfsEntry) -> &[u8] {
    &entry.name[..(entry.name_length as usize).min(entry.name.len())]
}

impl Conn {
    fn set_log(&mut self, method: &[u8], target: &[u8]) {
        self.method_len = method.len().min(self.method.len()) as u8;
        self.method[..self.method_len as usize].copy_from_slice(&method[..self.method_len as usize]);
        self.target_len = target.len().min(self.target.len()) as u8;
        self.target[..self.target_len as usize].copy_from_slice(&target[..self.target_len as usize]);
    }

    /// Writes the head of a bodiless answer or of one whose body follows (`cur`, or the file from `next`).
    fn set_head(&mut self, response: &Response) {
        let mut out = Out::new(&mut self.head);
        let _ = write_response(&mut out, response);
        self.head_len = out.len();
        self.head_sent = 0;
        self.status = response.status;
        self.sent = 0;
    }

    /// An answer that is a short page. Returns true so a caller can `return conn.error(...)` for "request dealt with".
    fn error(&mut self, status: u16, keep_alive: bool, file: Option<(u64, u64)>, method: &[u8], target: &[u8]) -> bool {
        if self.status == 0 {
            self.set_log(method, target);
        }
        let mut page = [0u8; 200];
        let mut body = Out::new(&mut page);
        let _ = message_page(&mut body, status);
        let page_length = body.len();
        let mut response = Response::new(status, "text/html", page_length as u64, keep_alive);
        response.file = file;
        self.keep_alive = keep_alive;
        self.set_head(&response);
        if !self.head_only {
            let end = (self.head_len + page_length).min(HEAD);
            let room = end - self.head_len;
            self.head[self.head_len..end].copy_from_slice(&page[..room]);
            self.head_len = end;
        }
        true
    }

    /// Takes the chunk buffers a body needs; `None` (and nothing taken) if the pool has them not.
    fn take_buffers(&mut self, shared: &mut Shared) -> Option<()> {
        let two = shared.worker_count > 0;
        let free = shared.bufs[..shared.buf_count].iter().filter(|buf| !buf.in_use).count();
        if free < if two { 2 } else { 1 } {
            return None;
        }
        self.cur = shared.acquire()?;
        self.ahead = if two { shared.acquire()? } else { 0 };
        self.ahead_state = EMPTY;
        self.cur_pos = 0;
        Some(())
    }

    fn log(&self, settings: &Settings) {
        if settings.log && self.status != 0 {
            let ip = self.peer_ip;
            println!(
                "httpd: {}.{}.{}.{} {} {} {} {}",
                ip[0],
                ip[1],
                ip[2],
                ip[3],
                core::str::from_utf8(&self.method[..self.method_len as usize]).unwrap_or("?"),
                core::str::from_utf8(&self.target[..self.target_len as usize]).unwrap_or("?"),
                self.status,
                self.sent
            );
        }
    }

    /// Back to waiting for a request: the chunk buffers go back to the pool.
    fn release(&mut self, shared: &mut Shared) {
        shared.give_back(self.cur);
        shared.give_back(self.ahead);
        self.cur = 0;
        self.ahead = 0;
        self.ahead_state = EMPTY;
        self.phase = REQUEST;
        self.status = 0;
    }
}

impl Shared<'_> {
    fn acquire(&mut self) -> Option<u8> {
        let index = self.bufs[..self.buf_count].iter().position(|buf| !buf.in_use)?;
        let buf = &mut self.bufs[index];
        buf.in_use = true;
        buf.orphan = false;
        buf.worker = 0;
        buf.len = 0;
        Some(index as u8 + 1)
    }

    fn give_back(&mut self, id: u8) {
        if id == 0 {
            return;
        }
        let buf = &mut self.bufs[id as usize - 1];
        if buf.worker == 0 {
            buf.in_use = false;
        } else {
            buf.orphan = true;
        }
    }

    /// Takes back the buffers of dead connections once their worker is done with them.
    fn reap(&mut self) {
        for index in 0..self.buf_count {
            if self.bufs[index].orphan && self.collect(index as u8 + 1).is_some() {
                self.bufs[index].in_use = false;
                self.bufs[index].orphan = false;
            }
        }
    }

    /// The outcome of the read a worker does into buffer `id`: `None` while it runs, `Some(Err)` if it failed.
    fn collect(&mut self, id: u8) -> Option<Result<usize, ()>> {
        let buf = &mut self.bufs[id as usize - 1];
        if buf.worker == 0 {
            return Some(Ok(buf.len));
        }
        let worker = unsafe { &*(self.workers[buf.worker as usize - 1] as *const Worker) };
        if worker.state.load(Ordering::Acquire) != DONE {
            return None;
        }
        let length = worker.length.load(Ordering::Relaxed);
        worker.state.store(IDLE, Ordering::Release);
        buf.worker = 0;
        if length == FAILED {
            return Some(Err(()));
        }
        buf.len = length;
        Some(Ok(length))
    }

    /// Asks an idle worker to read the next chunk of `conn`'s file into its `ahead` buffer; nothing if none is free.
    fn kick(&mut self, conn: &mut Conn) {
        if conn.ahead == 0 || conn.ahead_state != EMPTY || conn.next >= conn.end {
            return;
        }
        let Some(index) = self.workers[..self.worker_count].iter().position(|&address| unsafe { &*(address as *const Worker) }.state.load(Ordering::Acquire) == IDLE) else { return };
        let want = (conn.end - conn.next).min(CHUNK as u64) as usize;
        let worker = unsafe { &mut *(self.workers[index] as *mut Worker) };
        worker.path[..conn.path_len].copy_from_slice(&conn.path[..conn.path_len]);
        worker.path_len = conn.path_len;
        worker.offset = conn.next;
        worker.data = self.bufs[conn.ahead as usize - 1].data;
        worker.want = want;
        self.bufs[conn.ahead as usize - 1].worker = index as u8 + 1;
        conn.ahead_state = PENDING;
        conn.ahead_want = want;
        conn.next += want as u64;
        worker.state.store(JOB, Ordering::Release);
        tape_rt::futex_wake(&worker.state, 1);
    }
}

/// Reads until `buffer` is full or the file ends.
fn read_full(path: &str, offset: u64, buffer: &mut [u8]) -> Result<usize, ()> {
    let mut got = 0;
    while got < buffer.len() {
        match tape_rt::vfs_read(path, offset + got as u64, &mut buffer[got..]) {
            Ok(0) => break,
            Ok(count) => got += count,
            Err(_) => return Err(()),
        }
    }
    Ok(got)
}

extern "C" fn worker_main(address: usize) {
    let worker = address as *mut Worker;
    let state = unsafe { &(*worker).state };
    loop {
        let seen = state.load(Ordering::Acquire);
        if seen != JOB {
            tape_rt::futex_wait(state, seen);
            continue;
        }
        let (path, offset, data, want) = unsafe {
            let job = &*worker;
            (core::str::from_utf8(&job.path[..job.path_len]).unwrap_or(""), job.offset, job.data, job.want)
        };
        let buffer = unsafe { core::slice::from_raw_parts_mut(data as *mut u8, want) };
        let length = match read_full(path, offset, buffer) {
            Ok(count) if count == want => count,
            _ => FAILED,
        };
        unsafe { (*worker).length.store(length, Ordering::Relaxed) };
        state.store(DONE, Ordering::Release);
    }
}

fn start_workers(api: &TapeApi, shared: &mut Shared) {
    while shared.worker_count < WORKERS {
        let Some(worker) = tape_rt::alloc_zeroed::<Worker>(api) else { break };
        let address = worker as *mut Worker as usize;
        match tape_rt::spawn_thread(worker_main, address, 64 * 1024) {
            Ok(thread) => core::mem::forget(thread),
            Err(_) => break,
        }
        shared.workers[shared.worker_count] = address;
        shared.worker_count += 1;
    }
}

/// Sends what the connection's body has ready; reads the next chunk when the current one is out. True on progress.
fn body<const N: usize>(api: &TapeApi, server: &mut Server<RX, N>, conn: &mut Conn, shared: &mut Shared, slot: usize, now: u64) -> bool {
    let mut work = false;
    loop {
        shared.kick(conn);
        let length = shared.bufs[conn.cur as usize - 1].len;
        if conn.cur_pos >= length {
            match refill(conn, shared) {
                Refill::Ready => {}
                Refill::Wait => return work,
                Refill::Done => {
                    conn.phase = DRAIN;
                    conn.progress = now;
                    return true;
                }
                Refill::Failed => {
                    // The head promised more than the file has: all that is left is to hang up.
                    conn.keep_alive = false;
                    conn.phase = DRAIN;
                    conn.progress = now;
                    return true;
                }
            }
            continue;
        }
        let buf = &shared.bufs[conn.cur as usize - 1];
        let data = unsafe { core::slice::from_raw_parts((buf.data + conn.cur_pos) as *const u8, buf.len - conn.cur_pos) };
        let Ok(count) = server.send_some(api, slot, data) else { return work };
        if count == 0 {
            return work;
        }
        conn.cur_pos += count;
        conn.sent += count as u64;
        conn.progress = now;
        work = true;
        if conn.cur_pos < buf.len {
            return work;
        }
    }
}

enum Refill {
    Ready,
    Wait,
    Done,
    Failed,
}

/// The current chunk is out: bring in the next, from the read-ahead if it is there, else with a read right here.
fn refill(conn: &mut Conn, shared: &mut Shared) -> Refill {
    if conn.ahead_state == PENDING {
        match shared.collect(conn.ahead) {
            None => return Refill::Wait,
            Some(Ok(count)) if count == conn.ahead_want => conn.ahead_state = READY,
            Some(_) => return Refill::Failed,
        }
    }
    if conn.ahead_state == READY {
        core::mem::swap(&mut conn.cur, &mut conn.ahead);
        conn.ahead_state = EMPTY;
        conn.cur_pos = 0;
        return Refill::Ready;
    }
    if conn.next >= conn.end {
        return Refill::Done;
    }
    let want = (conn.end - conn.next).min(CHUNK as u64) as usize;
    let buf = &mut shared.bufs[conn.cur as usize - 1];
    let buffer = unsafe { core::slice::from_raw_parts_mut(buf.data as *mut u8, want) };
    let path = core::str::from_utf8(&conn.path[..conn.path_len]).unwrap_or("");
    match read_full(path, conn.next, buffer) {
        Ok(count) if count == want => {
            buf.len = count;
            conn.next += count as u64;
            conn.cur_pos = 0;
            Refill::Ready
        }
        _ => Refill::Failed,
    }
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

