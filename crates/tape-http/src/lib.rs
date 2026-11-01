//! The parts of HTTP the `httpd` server and the `web` browser share, as plain `no_std` code with no allocation and host
//! tests. Only what a small GET-only server and a text browser need: HTTP/1.0 style (`Connection: close`, no chunking,
//! no keep-alive), request/response heads, a safe request path, MIME types, directory listings, URLs, and a rough
//! HTML-to-text renderer that numbers the links.
#![cfg_attr(not(test), no_std)]

use core::fmt::{self, Write};

/// A `fmt::Write` into a byte slice. A write that does not fit is refused whole and sets `overflow`.
pub struct Out<'a> {
    buf: &'a mut [u8],
    len: usize,
    pub overflow: bool,
}

impl<'a> Out<'a> {
    pub fn new(buf: &'a mut [u8]) -> Self {
        Out { buf, len: 0, overflow: false }
    }
    pub fn bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn room(&self) -> usize {
        self.buf.len() - self.len
    }
}

impl Write for Out<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        if text.len() > self.room() {
            self.overflow = true;
            return Err(fmt::Error);
        }
        self.buf[self.len..self.len + text.len()].copy_from_slice(text.as_bytes());
        self.len += text.len();
        Ok(())
    }
}

// ---------------------------------------------------------------- server side

/// A parsed request line. The head of a request ends at a blank line; a body (we serve GET and HEAD only) is ignored.
pub struct Request<'a> {
    pub method: &'a [u8],
    pub target: &'a [u8],
}

pub enum Parse<'a> {
    /// No blank line yet: read more.
    Incomplete,
    Bad,
    Done(Request<'a>),
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

pub fn parse_request(buf: &[u8]) -> Parse<'_> {
    let Some(end) = find(buf, b"\r\n\r\n").or_else(|| find(buf, b"\n\n")) else { return Parse::Incomplete };
    let line_end = buf.iter().position(|&b| b == b'\n').unwrap_or(end);
    let line = buf[..line_end].strip_suffix(b"\r").unwrap_or(&buf[..line_end]);
    let mut parts = line.split(|&b| b == b' ').filter(|part| !part.is_empty());
    let (Some(method), Some(target), Some(version)) = (parts.next(), parts.next(), parts.next()) else { return Parse::Bad };
    if !version.starts_with(b"HTTP/") || parts.next().is_some() {
        return Parse::Bad;
    }
    Parse::Done(Request { method, target })
}

/// The request target as a path relative to the web root: percent-decoded into `out`, the query and fragment cut off.
/// Returns (length, ends-with-slash); an empty path with `true` is the root. Refused (`None`): not starting with `/`,
/// a NUL, control character or backslash (also after decoding), an empty segment in the middle (`//`), `.` and `..`.
pub fn clean_path(target: &[u8], out: &mut [u8]) -> Option<(usize, bool)> {
    if target.first() != Some(&b'/') {
        return None;
    }
    let cut = target.iter().position(|&b| b == b'?' || b == b'#').unwrap_or(target.len());
    let raw = &target[1..cut];
    let mut length = 0;
    let mut index = 0;
    while index < raw.len() {
        let mut byte = raw[index];
        if byte == b'%' {
            let high = hex(*raw.get(index + 1)?)?;
            let low = hex(*raw.get(index + 2)?)?;
            byte = high << 4 | low;
            index += 2;
        }
        if byte < 0x20 || byte == 0x7f || byte == b'\\' || length == out.len() {
            return None;
        }
        out[length] = byte;
        length += 1;
        index += 1;
    }
    let trailing = length == 0 || out[length - 1] == b'/';
    let path = &out[..length - usize::from(length > 0 && trailing)];
    if !path.is_empty() && path.split(|&b| b == b'/').any(|segment| segment.is_empty() || segment == b"." || segment == b"..") {
        return None;
    }
    Some((path.len(), trailing))
}

fn hex(byte: u8) -> Option<u8> {
    (byte as char).to_digit(16).map(|digit| digit as u8)
}

/// Files that are never served: the machine's own configuration, whatever the web root (a root of the whole volume
/// would otherwise publish `synckey`), and everything under `config/`.
pub fn is_blocked(path: &[u8]) -> bool {
    const NAMES: [&[u8]; 7] = [b"synckey", b"settings", b"start", b"hostname", b"debug", b"fstab", b"password"];
    let first = path.split(|&b| b == b'/').next().unwrap_or(b"");
    if first.eq_ignore_ascii_case(b"config") && path.len() > first.len() {
        return true;
    }
    let last = path.rsplit(|&b| b == b'/').next().unwrap_or(b"");
    NAMES.iter().any(|name| last.eq_ignore_ascii_case(name))
}

pub fn mime(name: &[u8]) -> &'static str {
    let extension = match name.iter().rposition(|&b| b == b'.') {
        Some(dot) => &name[dot + 1..],
        None => &name[..0],
    };
    const TABLE: [(&str, &str); 28] = [
        ("html", "text/html; charset=utf-8"),
        ("htm", "text/html; charset=utf-8"),
        ("txt", "text/plain; charset=utf-8"),
        ("md", "text/plain; charset=utf-8"),
        ("log", "text/plain; charset=utf-8"),
        ("cfg", "text/plain; charset=utf-8"),
        ("rs", "text/plain; charset=utf-8"),
        ("c", "text/plain; charset=utf-8"),
        ("h", "text/plain; charset=utf-8"),
        ("css", "text/css; charset=utf-8"),
        ("js", "text/javascript; charset=utf-8"),
        ("json", "application/json"),
        ("xml", "application/xml"),
        ("png", "image/png"),
        ("jpg", "image/jpeg"),
        ("jpeg", "image/jpeg"),
        ("gif", "image/gif"),
        ("svg", "image/svg+xml"),
        ("ico", "image/x-icon"),
        ("pdf", "application/pdf"),
        ("wav", "audio/wav"),
        ("mp3", "audio/mpeg"),
        ("flac", "audio/flac"),
        ("ogg", "audio/ogg"),
        ("opus", "audio/ogg"),
        ("zip", "application/zip"),
        ("gz", "application/gzip"),
        ("csv", "text/csv; charset=utf-8"),
    ];
    for (name, kind) in TABLE {
        if extension.eq_ignore_ascii_case(name.as_bytes()) {
            return kind;
        }
    }
    "application/octet-stream"
}

pub fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        301 => "Moved Permanently",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        414 => "URI Too Long",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

/// Writes a response head (status line, headers, blank line). `location` makes a redirect.
pub fn response_head(out: &mut Out, status: u16, content_type: &str, length: u64, location: Option<&[u8]>) -> fmt::Result {
    write!(out, "HTTP/1.0 {} {}\r\nServer: fos-httpd\r\nConnection: close\r\nContent-Type: {}\r\nContent-Length: {}\r\n", status, reason(status), content_type, length)?;
    if let Some(location) = location {
        out.write_str("Location: ")?;
        out.write_str(core::str::from_utf8(location).map_err(|_| fmt::Error)?)?;
        out.write_str("\r\n")?;
    }
    if status == 405 {
        out.write_str("Allow: GET, HEAD\r\n")?;
    }
    out.write_str("\r\n")
}

/// A small HTML page for an error or a status.
pub fn message_page(out: &mut Out, status: u16) -> fmt::Result {
    write!(out, "<html><head><title>{0} {1}</title></head><body><h1>{0} {1}</h1></body></html>\n", status, reason(status))
}

/// HTML-escapes a name for display (names are UTF-8 where they can be; anything else is shown byte by byte).
struct Escaped<'a>(&'a [u8]);

impl fmt::Display for Escaped<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let put = |ch: char, f: &mut fmt::Formatter<'_>| match ch {
            '<' => f.write_str("&lt;"),
            '>' => f.write_str("&gt;"),
            '&' => f.write_str("&amp;"),
            '"' => f.write_str("&quot;"),
            ch if (ch as u32) < 0x20 => f.write_char('?'),
            ch => f.write_char(ch),
        };
        match core::str::from_utf8(self.0) {
            Ok(text) => text.chars().try_for_each(|ch| put(ch, f)),
            Err(_) => self.0.iter().try_for_each(|&byte| put(byte as char, f)),
        }
    }
}

struct Encoded<'a>(&'a [u8]);

impl fmt::Display for Encoded<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for &byte in self.0 {
            if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
                f.write_char(byte as char)?;
            } else {
                write!(f, "%{byte:02X}")?;
            }
        }
        Ok(())
    }
}

/// The top of a directory listing page for `path` (`""` is the root).
pub fn listing_start(out: &mut Out, path: &[u8]) -> fmt::Result {
    write!(out, "<html><head><title>Index of /{0}</title></head><body><h1>Index of /{0}</h1>\n<pre>\n", Escaped(path))?;
    if !path.is_empty() {
        out.write_str("<a href=\"../\">../</a>\n")?;
    }
    Ok(())
}

/// One entry; a folder gets a trailing slash so the link needs no redirect.
pub fn listing_entry(out: &mut Out, name: &[u8], directory: bool, length: u64) -> fmt::Result {
    let slash = if directory { "/" } else { "" };
    write!(out, "<a href=\"{}{}\">{}{}</a>", Encoded(name), slash, Escaped(name), slash)?;
    if directory { out.write_str("\n") } else { write!(out, "  {length}\n") }
}

pub fn listing_end(out: &mut Out) -> fmt::Result {
    out.write_str("</pre></body></html>\n")
}

// ---------------------------------------------------------------- client side

pub struct Url<'a> {
    pub host: &'a str,
    pub port: u16,
    /// Path and query, starting with `/`.
    pub path: &'a str,
}

/// `http://host[:port]/path`; the scheme may be left out. Fragments are dropped. A path-less `host?x` gets `/` first.
pub fn parse_url(text: &str) -> Result<Url<'_>, &'static str> {
    let text = text.trim();
    let rest = match text.split_once("://") {
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("http") => rest,
        Some((scheme, _)) if scheme.eq_ignore_ascii_case("https") => return Err("https is not supported"),
        Some(_) => return Err("only http: addresses work"),
        None if text.starts_with("mailto:") || text.starts_with("javascript:") => return Err("only http: addresses work"),
        None => text,
    };
    let rest = rest.split('#').next().unwrap_or("");
    let (authority, path) = match rest.find('/') {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, "/"),
    };
    let (authority, path) = match authority.find('?') {
        Some(_) => return Err("put a / before the ? in the address"),
        None => (authority, path),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, port.parse().map_err(|_| "bad port")?),
        None => (authority, 80),
    };
    if host.is_empty() {
        return Err("no host in the address");
    }
    Ok(Url { host, port, path })
}

/// Writes the absolute `http://` address a link `href` on the page at `base` leads to. `mailto:` and other schemes are
/// copied as they are (and refused when fetched). A bare `#fragment` is the page itself.
pub fn resolve_link(base: &Url, href: &str, out: &mut Out) -> fmt::Result {
    let href = href.trim();
    let href = href.split('#').next().unwrap_or("");
    let authority = |out: &mut Out| -> fmt::Result {
        if base.port == 80 { write!(out, "http://{}", base.host) } else { write!(out, "http://{}:{}", base.host, base.port) }
    };
    if href.is_empty() {
        authority(out)?;
        return out.write_str(base.path);
    }
    if href.contains("://") || href.starts_with("mailto:") || href.starts_with("javascript:") {
        return out.write_str(href);
    }
    if let Some(rest) = href.strip_prefix("//") {
        return write!(out, "http://{rest}");
    }
    authority(out)?;
    if href.starts_with('/') {
        return normalize(href, out);
    }
    let path = base.path.split('?').next().unwrap_or("/");
    if href.starts_with('?') {
        out.write_str(path)?;
        return out.write_str(href);
    }
    let directory = &path[..path.rfind('/').map_or(0, |slash| slash + 1)];
    // Join, then collapse `.` and `..`: in a scratch buffer (links are short).
    let mut scratch = [0u8; 512];
    let mut joined = Out::new(&mut scratch);
    write!(joined, "{directory}{href}")?;
    normalize(core::str::from_utf8(joined.bytes()).map_err(|_| fmt::Error)?, out)
}

/// Collapses `.` and `..` segments of an absolute path (a query is kept as it is).
fn normalize(path: &str, out: &mut Out) -> fmt::Result {
    let (path, query) = match path.find('?') {
        Some(at) => (&path[..at], &path[at..]),
        None => (path, ""),
    };
    let mut scratch = [0u8; 512];
    let mut result = Out::new(&mut scratch);
    let mut starts = [0usize; 32];
    let mut depth = 0;
    let mut iterator = path.strip_prefix('/').unwrap_or(path).split('/').peekable();
    let mut trailing = false;
    while let Some(segment) = iterator.next() {
        let last = iterator.peek().is_none();
        trailing = last && matches!(segment, "" | "." | "..");
        match segment {
            "." | "" => {}
            ".." => {
                if depth > 0 {
                    depth -= 1;
                    result.len = starts[depth];
                }
            }
            segment => {
                if depth == starts.len() {
                    return Err(fmt::Error);
                }
                starts[depth] = result.len;
                depth += 1;
                result.write_str("/")?;
                result.write_str(segment)?;
            }
        }
    }
    if trailing || result.is_empty() {
        result.write_str("/")?;
    }
    out.write_str(core::str::from_utf8(result.bytes()).map_err(|_| fmt::Error)?)?;
    out.write_str(query)
}

pub struct Head<'a> {
    pub status: u16,
    /// Bytes of the head including the blank line: the body starts here.
    pub header_len: usize,
    pub content_length: Option<u64>,
    pub location: Option<&'a [u8]>,
    pub content_type: Option<&'a [u8]>,
}

/// Parses a response head; `None` while the blank line has not arrived or the status line is not HTTP.
pub fn parse_head(buf: &[u8]) -> Option<Head<'_>> {
    let (end, gap) = match (find(buf, b"\r\n\r\n"), find(buf, b"\n\n")) {
        (Some(a), Some(b)) if b < a => (b, 2),
        (Some(a), _) => (a, 4),
        (None, Some(b)) => (b, 2),
        (None, None) => return None,
    };
    let head = &buf[..end];
    let mut lines = head.split(|&b| b == b'\n').map(|line| line.strip_suffix(b"\r").unwrap_or(line));
    let status_line = lines.next()?;
    let mut parts = status_line.split(|&b| b == b' ').filter(|part| !part.is_empty());
    if !parts.next()?.starts_with(b"HTTP/") {
        return None;
    }
    let status = core::str::from_utf8(parts.next()?).ok()?.parse().ok()?;
    let mut result = Head { status, header_len: end + gap, content_length: None, location: None, content_type: None };
    for line in lines {
        let Some(colon) = line.iter().position(|&b| b == b':') else { continue };
        let (name, value) = (&line[..colon], line[colon + 1..].trim_ascii());
        if name.eq_ignore_ascii_case(b"content-length") {
            result.content_length = core::str::from_utf8(value).ok().and_then(|text| text.parse().ok());
        } else if name.eq_ignore_ascii_case(b"location") {
            result.location = Some(value);
        } else if name.eq_ignore_ascii_case(b"content-type") {
            result.content_type = Some(value);
        }
    }
    Some(result)
}

// ---------------------------------------------------------------- HTML to text

/// Marks in `Page::text` around the words of a link: the nth `LINK_START` is link n (from 0).
pub const LINK_START: u8 = 1;
pub const LINK_END: u8 = 2;

/// A page rendered as text: lines separated by `\n`, words of a link between `LINK_START` and `LINK_END`, the link
/// targets (as written in the page) in a pool. `T` text bytes, `P` pool bytes, `L` links.
pub struct Page<const T: usize, const P: usize, const L: usize> {
    text: [u8; T],
    len: usize,
    pool: [u8; P],
    pool_len: usize,
    links: [(u16, u16); L],
    link_count: usize,
    title: [u8; 80],
    title_len: usize,
    pending_space: bool,
    in_link: bool,
}

impl<const T: usize, const P: usize, const L: usize> Page<T, P, L> {
    pub const fn new() -> Self {
        Page { text: [0; T], len: 0, pool: [0; P], pool_len: 0, links: [(0, 0); L], link_count: 0, title: [0; 80], title_len: 0, pending_space: false, in_link: false }
    }

    pub fn clear(&mut self) {
        self.len = 0;
        self.pool_len = 0;
        self.link_count = 0;
        self.title_len = 0;
        self.pending_space = false;
        self.in_link = false;
    }

    pub fn text(&self) -> &[u8] {
        &self.text[..self.len]
    }

    pub fn title(&self) -> &[u8] {
        &self.title[..self.title_len]
    }

    pub fn link_count(&self) -> usize {
        self.link_count
    }

    /// The target of link `index`, as written in the page (relative links stay relative: see `resolve_link`).
    pub fn link(&self, index: usize) -> Option<&[u8]> {
        if index >= self.link_count {
            return None;
        }
        let (start, length) = self.links[index];
        Some(&self.pool[start as usize..start as usize + length as usize])
    }

    fn at_line_start(&self) -> bool {
        self.len == 0 || self.text[self.len - 1] == b'\n'
    }

    fn raw(&mut self, byte: u8) {
        if self.len < T {
            self.text[self.len] = byte;
            self.len += 1;
        }
    }

    /// Makes the text end in at least `count` newlines (never at the very start, never more than one blank line).
    fn newline(&mut self, count: usize) {
        self.pending_space = false;
        if self.len == 0 {
            return;
        }
        let mut have = 0;
        while have < count && have < self.len && self.text[self.len - 1 - have] == b'\n' {
            have += 1;
        }
        for _ in have..count {
            self.raw(b'\n');
        }
    }

    fn flush_space(&mut self) {
        if self.pending_space && !self.at_line_start() {
            self.raw(b' ');
        }
        self.pending_space = false;
    }

    fn glyph(&mut self, byte: u8) {
        self.flush_space();
        self.raw(byte);
    }

    fn glyphs(&mut self, text: &str) {
        for &byte in text.as_bytes() {
            self.glyph(byte);
        }
    }

    fn open_link(&mut self, href: &[u8]) {
        if self.in_link || href.is_empty() || self.link_count == L || self.pool_len + href.len() > P {
            return;
        }
        self.links[self.link_count] = (self.pool_len as u16, href.len() as u16);
        self.pool[self.pool_len..self.pool_len + href.len()].copy_from_slice(href);
        self.pool_len += href.len();
        self.link_count += 1;
        self.in_link = true;
        self.flush_space();
        self.raw(LINK_START);
    }

    fn close_link(&mut self) {
        if self.in_link {
            self.in_link = false;
            self.raw(LINK_END);
        }
    }

    /// Takes a plain text document as it is (control characters are dropped, tabs become spaces).
    pub fn set_plain(&mut self, body: &[u8]) {
        self.clear();
        for &byte in body {
            match byte {
                b'\n' => self.raw(b'\n'),
                b'\t' => self.raw(b' '),
                byte if byte >= 0x20 && byte != 0x7f => self.raw(byte),
                _ => {}
            }
        }
    }

    /// Renders an HTML document: tags become line breaks, bullets and link marks, scripts and styles vanish,
    /// entities are decoded, whitespace is collapsed (except in `<pre>`).
    pub fn set_html(&mut self, html: &[u8]) {
        self.clear();
        let mut index = 0;
        let mut in_pre = false;
        let mut in_title = false;
        while index < html.len() && self.len < T {
            let byte = html[index];
            if byte == b'<' {
                if html[index..].starts_with(b"<!--") {
                    index = find(&html[index + 4..], b"-->").map_or(html.len(), |end| index + 4 + end + 3);
                    continue;
                }
                let end = tag_end(&html[index..]).map_or(html.len(), |end| index + end);
                let inner_end = if html[..end].ends_with(b">") { end - 1 } else { end };
                let tag = &html[(index + 1).min(inner_end)..inner_end];
                index = end;
                let closing = tag.first() == Some(&b'/');
                let tag = if closing { &tag[1..] } else { tag };
                let name_end = tag.iter().position(|&b| b.is_ascii_whitespace() || b == b'/').unwrap_or(tag.len());
                let mut name = [0u8; 12];
                let name_length = name_end.min(name.len());
                for (slot, &letter) in name.iter_mut().zip(&tag[..name_length]) {
                    *slot = letter.to_ascii_lowercase();
                }
                let name = &name[..name_length];
                match name {
                    b"script" | b"style" if !closing => {
                        let close: &[u8] = if name == b"script" { b"</script" } else { b"</style" };
                        index = find_nocase(&html[index..], close).map_or(html.len(), |at| index + at);
                    }
                    b"title" => {
                        in_title = !closing;
                        if !closing {
                            self.title_len = 0;
                        }
                    }
                    b"br" => self.newline(1),
                    b"p" | b"h1" | b"h2" | b"h3" | b"h4" | b"h5" | b"h6" | b"ul" | b"ol" | b"table" | b"blockquote" | b"dl" => self.newline(2),
                    b"div" | b"tr" | b"section" | b"article" | b"header" | b"footer" | b"nav" | b"form" | b"dt" | b"dd" => self.newline(1),
                    b"li" if !closing => {
                        self.newline(1);
                        self.glyphs("* ");
                    }
                    b"li" => self.newline(1),
                    b"hr" => {
                        self.newline(1);
                        self.glyphs("--------------------");
                        self.newline(1);
                    }
                    b"td" | b"th" => self.pending_space = true,
                    b"pre" => {
                        self.newline(2);
                        in_pre = !closing;
                    }
                    b"img" if !closing => {
                        self.glyphs("[");
                        match attribute(tag, b"alt").filter(|alt| !alt.is_empty()).and_then(|alt| core::str::from_utf8(alt).ok()) {
                            Some(alt) => self.glyphs(alt),
                            None => self.glyphs("image"),
                        }
                        self.glyphs("]");
                    }
                    b"a" if closing => self.close_link(),
                    b"a" => {
                        if let Some(href) = attribute(tag, b"href") {
                            self.open_link(href);
                        }
                    }
                    _ => {}
                }
                continue;
            }
            if byte == b'&' {
                if let Some((ch, used)) = entity(&html[index..]) {
                    let mut buffer = [0u8; 4];
                    for &part in ch.encode_utf8(&mut buffer).as_bytes() {
                        if in_title {
                            self.title_byte(part);
                        } else if in_pre {
                            self.raw(part);
                        } else if ch == ' ' {
                            self.pending_space = true;
                        } else {
                            self.glyph(part);
                        }
                    }
                    index += used;
                    continue;
                }
            }
            index += 1;
            if in_title {
                self.title_byte(if byte >= 0x20 { byte } else { b' ' });
            } else if in_pre {
                match byte {
                    b'\n' => self.raw(b'\n'),
                    b'\t' => self.glyphs("    "),
                    byte if byte >= 0x20 && byte != 0x7f => self.raw(byte),
                    _ => {}
                }
            } else if byte.is_ascii_whitespace() || byte < 0x20 || byte == 0x7f {
                self.pending_space = true;
            } else {
                self.glyph(byte);
            }
        }
        self.close_link();
        if self.len > 0 && self.text[self.len - 1] != b'\n' {
            self.raw(b'\n');
        }
    }

    fn title_byte(&mut self, byte: u8) {
        let doubled = byte == b' ' && (self.title_len == 0 || self.title[self.title_len - 1] == b' ');
        if self.title_len < self.title.len() && !doubled {
            self.title[self.title_len] = byte;
            self.title_len += 1;
        }
    }
}

impl<const T: usize, const P: usize, const L: usize> Default for Page<T, P, L> {
    fn default() -> Self {
        Self::new()
    }
}

/// Offset just after the `>` that closes the tag starting at `html[0]`, quotes respected.
fn tag_end(html: &[u8]) -> Option<usize> {
    let mut quote = 0u8;
    for (offset, &byte) in html.iter().enumerate().skip(1) {
        match byte {
            b'"' | b'\'' if quote == 0 => quote = byte,
            byte if byte == quote => quote = 0,
            b'>' if quote == 0 => return Some(offset + 1),
            _ => {}
        }
    }
    None
}

fn find_nocase(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window.eq_ignore_ascii_case(needle))
}

/// The value of attribute `wanted` in the text of a tag (after `<`, name included).
fn attribute<'a>(tag: &'a [u8], wanted: &[u8]) -> Option<&'a [u8]> {
    let mut index = tag.iter().position(|b| b.is_ascii_whitespace())?;
    while index < tag.len() {
        while index < tag.len() && (tag[index].is_ascii_whitespace() || tag[index] == b'/') {
            index += 1;
        }
        let start = index;
        while index < tag.len() && !tag[index].is_ascii_whitespace() && tag[index] != b'=' && tag[index] != b'/' {
            index += 1;
        }
        let name = &tag[start..index];
        while index < tag.len() && tag[index].is_ascii_whitespace() {
            index += 1;
        }
        if tag.get(index) != Some(&b'=') {
            continue;
        }
        index += 1;
        while index < tag.len() && tag[index].is_ascii_whitespace() {
            index += 1;
        }
        let value = match tag.get(index) {
            Some(&quote) if quote == b'"' || quote == b'\'' => {
                let begin = index + 1;
                let end = tag[begin..].iter().position(|&b| b == quote).map_or(tag.len(), |at| begin + at);
                index = end + 1;
                &tag[begin..end]
            }
            _ => {
                let begin = index;
                while index < tag.len() && !tag[index].is_ascii_whitespace() {
                    index += 1;
                }
                &tag[begin..index]
            }
        };
        if name.eq_ignore_ascii_case(wanted) {
            return Some(value);
        }
    }
    None
}

/// `&amp;`, `&#228;`, `&#xe4;`: the character and how many bytes the entity takes.
fn entity(text: &[u8]) -> Option<(char, usize)> {
    let end = text.iter().take(12).position(|&b| b == b';')?;
    let name = &text[1..end];
    let ch = if let Some(number) = name.strip_prefix(b"#") {
        let value = match number.first() {
            Some(b'x' | b'X') => u32::from_str_radix(core::str::from_utf8(&number[1..]).ok()?, 16).ok()?,
            _ => core::str::from_utf8(number).ok()?.parse().ok()?,
        };
        char::from_u32(value).filter(|&ch| ch >= ' ' && ch != '\u{7f}')?
    } else {
        match name {
            b"amp" => '&',
            b"lt" => '<',
            b"gt" => '>',
            b"quot" => '"',
            b"apos" => '\'',
            b"nbsp" => ' ',
            b"copy" => '\u{a9}',
            b"reg" => '\u{ae}',
            b"hellip" => '\u{2026}',
            b"mdash" => '\u{2014}',
            b"ndash" => '\u{2013}',
            b"laquo" => '\u{ab}',
            b"raquo" => '\u{bb}',
            b"middot" => '\u{b7}',
            b"bull" => '\u{2022}',
            b"aring" => '\u{e5}',
            b"Aring" => '\u{c5}',
            b"auml" => '\u{e4}',
            b"Auml" => '\u{c4}',
            b"ouml" => '\u{f6}',
            b"Ouml" => '\u{d6}',
            b"eacute" => '\u{e9}',
            b"uuml" => '\u{fc}',
            b"szlig" => '\u{df}',
            _ => return None,
        }
    };
    Some((ch, end + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(target: &str) -> Option<(String, bool)> {
        let mut out = [0u8; 128];
        clean_path(target.as_bytes(), &mut out).map(|(length, slash)| (String::from_utf8_lossy(&out[..length]).into_owned(), slash))
    }

    #[test]
    fn request_line() {
        let Parse::Done(request) = parse_request(b"GET /a/b?x=1 HTTP/1.1\r\nHost: x\r\n\r\n") else { panic!() };
        assert_eq!((request.method, request.target), (&b"GET"[..], &b"/a/b?x=1"[..]));
        assert!(matches!(parse_request(b"GET / HTTP/1.1\r\nHost: x\r\n"), Parse::Incomplete));
        assert!(matches!(parse_request(b"GARBAGE\r\n\r\n"), Parse::Bad));
        assert!(matches!(parse_request(b"GET /\n\n"), Parse::Bad));
    }

    #[test]
    fn paths_are_cleaned_and_traversal_refused() {
        assert_eq!(path("/"), Some((String::new(), true)));
        assert_eq!(path("/index.html?x=1#y"), Some(("index.html".into(), false)));
        assert_eq!(path("/docs/"), Some(("docs".into(), true)));
        assert_eq!(path("/a%20b/c%C3%A4.txt"), Some(("a b/c\u{e4}.txt".into(), false)));
        assert_eq!(path("/../x"), None);
        assert_eq!(path("/a/../x"), None);
        assert_eq!(path("/%2e%2e/x"), None);
        assert_eq!(path("/a%2f..%2fb"), None);
        assert_eq!(path("/a//b"), None);
        assert_eq!(path("/a\\b"), None);
        assert_eq!(path("/a%00b"), None);
        assert_eq!(path("/a%zz"), None);
        assert_eq!(path("x"), None);
    }

    #[test]
    fn configuration_is_blocked() {
        assert!(is_blocked(b"synckey"));
        assert!(is_blocked(b"SETTINGS"));
        assert!(is_blocked(b"www/start"));
        assert!(is_blocked(b"config/anything"));
        assert!(!is_blocked(b"config"));
        assert!(!is_blocked(b"docs/readme.txt"));
    }

    #[test]
    fn mime_types() {
        assert_eq!(mime(b"a.HTML"), "text/html; charset=utf-8");
        assert_eq!(mime(b"a.png"), "image/png");
        assert_eq!(mime(b"noext"), "application/octet-stream");
        assert_eq!(mime(b"a.weird"), "application/octet-stream");
    }

    #[test]
    fn heads_and_listing() {
        let mut buffer = [0u8; 512];
        let mut out = Out::new(&mut buffer);
        response_head(&mut out, 301, "text/html", 0, Some(b"/docs/")).unwrap();
        let head = core::str::from_utf8(out.bytes()).unwrap();
        assert!(head.starts_with("HTTP/1.0 301 Moved Permanently\r\n"));
        assert!(head.contains("Location: /docs/\r\n") && head.ends_with("\r\n\r\n"));
        let mut buffer = [0u8; 512];
        let mut out = Out::new(&mut buffer);
        listing_start(&mut out, b"a<b").unwrap();
        listing_entry(&mut out, b"x y&z.txt", false, 12).unwrap();
        listing_entry(&mut out, "k\u{e4}t".as_bytes(), true, 0).unwrap();
        let page = core::str::from_utf8(out.bytes()).unwrap();
        assert!(page.contains("Index of /a&lt;b"));
        assert!(page.contains("href=\"x%20y%26z.txt\">x y&amp;z.txt</a>  12"));
        assert!(page.contains("href=\"k%C3%A4t/\">k\u{e4}t/</a>"));
        assert!(page.contains("../"));
    }

    #[test]
    fn urls() {
        let url = parse_url("http://example.com:8080/a/b?q=1#frag").unwrap();
        assert_eq!((url.host, url.port, url.path), ("example.com", 8080, "/a/b?q=1"));
        let url = parse_url("10.0.2.2").unwrap();
        assert_eq!((url.host, url.port, url.path), ("10.0.2.2", 80, "/"));
        assert!(parse_url("host?x=1").is_err());
        assert!(parse_url("https://x/").is_err());
        assert!(parse_url("ftp://x/").is_err());
    }

    fn link(base: &str, href: &str) -> String {
        let mut buffer = [0u8; 256];
        let mut out = Out::new(&mut buffer);
        resolve_link(&parse_url(base).unwrap(), href, &mut out).unwrap();
        String::from_utf8(out.bytes().to_vec()).unwrap()
    }

    #[test]
    fn links_resolve() {
        assert_eq!(link("http://h/a/b.html", "c.html"), "http://h/a/c.html");
        assert_eq!(link("http://h/a/b.html", "../c.html"), "http://h/c.html");
        assert_eq!(link("http://h/a/b.html", "/x/y"), "http://h/x/y");
        assert_eq!(link("http://h:81/a/", "sub/"), "http://h:81/a/sub/");
        assert_eq!(link("http://h/a/b.html", "http://o/z"), "http://o/z");
        assert_eq!(link("http://h/a/b.html", "//o/z"), "http://o/z");
        assert_eq!(link("http://h/a/b.html", "#top"), "http://h/a/b.html");
        assert_eq!(link("http://h/a/b.html", "?q=1"), "http://h/a/b.html?q=1");
        assert_eq!(link("http://h/a/b.html", "./d/../e.txt?x=../y"), "http://h/a/e.txt?x=../y");
        assert_eq!(link("http://h/a/b/c", ".."), "http://h/a/");
        assert_eq!(link("http://h/", "mailto:a@b"), "mailto:a@b");
    }

    #[test]
    fn response_head_parses() {
        let reply = b"HTTP/1.1 302 Found\r\nlocation: /next\r\nContent-Length: 5\r\nContent-Type: text/html\r\n\r\nhello";
        let head = parse_head(reply).unwrap();
        assert_eq!((head.status, head.content_length, head.location, head.content_type), (302, Some(5), Some(&b"/next"[..]), Some(&b"text/html"[..])));
        assert_eq!(&reply[head.header_len..], b"hello");
        assert!(parse_head(b"HTTP/1.0 200 OK\r\nX: y\r\n").is_none());
        assert!(parse_head(b"SSH-2.0\r\n\r\n").is_none());
    }

    #[test]
    fn html_renders_text_and_links() {
        let mut page = Page::<2048, 512, 16>::new();
        page.set_html(b"<html><head><title>My  &amp; Page</title><style>p{x}</style></head><body><h1>Hi</h1><p>One <a href=\"/a.html\">two\n three</a>, &auml;&#246;&#xe5; &bogus; <b>x</b></p><script>var a='<p>';</script><ul><li>first<li>second</ul><pre>  a\n  b</pre><!-- gone --><img alt=\"pic\"></body></html>");
        assert_eq!(page.title(), b"My & Page");
        let shown = core::str::from_utf8(page.text()).unwrap().replace('\u{1}', "[").replace('\u{2}', "]");
        assert_eq!(shown, "Hi\n\nOne [two three], \u{e4}\u{f6}\u{e5} &bogus; x\n\n* first\n* second\n\n  a\n  b\n\n[pic]\n");
        assert_eq!(page.link_count(), 1);
        assert_eq!(page.link(0), Some(&b"/a.html"[..]));
        assert_eq!(page.link(1), None);
    }

    #[test]
    fn html_survives_junk_and_limits() {
        let mut page = Page::<64, 16, 2>::new();
        page.set_html(b"<a href=1>a</a><a href=2>b</a><a href=3>c</a><p>unterminated <b");
        assert_eq!(page.link_count(), 2);
        page.set_html(&[b'x'; 500]);
        assert_eq!(page.text().len(), 64);
        page.set_html(b"<<<>>><a");
        page.set_plain(b"a\tb\r\n\x01c");
        assert_eq!(page.text(), b"a b\nc");
    }
}
