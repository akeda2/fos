//! SIP messages (RFC 3261): reading a datagram and writing requests and responses.

use core::fmt::Write;

/// A `fmt::Write` into a byte slice. After one write that does not fit, every later write is refused too.
pub struct Cursor<'a> {
    pub bytes: &'a mut [u8],
    pub length: usize,
    pub overflow: bool,
}

impl<'a> Cursor<'a> {
    pub fn new(bytes: &'a mut [u8]) -> Self {
        Cursor { bytes, length: 0, overflow: false }
    }
}

impl Write for Cursor<'_> {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let end = self.length + text.len();
        if self.overflow || end > self.bytes.len() {
            self.overflow = true;
            return Err(core::fmt::Error);
        }
        self.bytes[self.length..end].copy_from_slice(text.as_bytes());
        self.length = end;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Start<'a> {
    Request { method: &'a str, uri: &'a str },
    Status { code: u16, reason: &'a str },
}

/// One parsed datagram: the start line, the header block (as written) and the body.
pub struct Message<'a> {
    pub start: Start<'a>,
    headers: &'a str,
    pub body: &'a str,
}

impl<'a> Message<'a> {
    pub fn parse(packet: &'a [u8]) -> Option<Message<'a>> {
        let text = core::str::from_utf8(packet).ok()?;
        let (head, rest) = match text.find("\r\n\r\n") {
            Some(end) => (&text[..end], &text[end + 4..]),
            None => (text.trim_end_matches("\r\n"), ""),
        };
        let (first, headers) = head.split_once("\r\n").unwrap_or((head, ""));
        let mut words = first.splitn(3, ' ');
        let (one, two, three) = (words.next()?, words.next()?, words.next().unwrap_or(""));
        let start = if one == "SIP/2.0" {
            Start::Status { code: two.parse().ok()?, reason: three }
        } else if three == "SIP/2.0" && !one.is_empty() {
            Start::Request { method: one, uri: two }
        } else {
            return None;
        };
        let mut message = Message { start, headers, body: rest };
        if let Some(length) = message.header("content-length").and_then(|v| v.parse::<usize>().ok()) {
            message.body = rest.get(..length).unwrap_or(rest);
        }
        Some(message)
    }

    /// Every header called `name` (lower case; the compact forms `v f t i m c l` are matched too), in order.
    pub fn headers(&self, name: &'a str) -> impl Iterator<Item = &'a str> {
        let compact = match name {
            "via" => "v",
            "from" => "f",
            "to" => "t",
            "call-id" => "i",
            "contact" => "m",
            "content-type" => "c",
            "content-length" => "l",
            _ => "\0",
        };
        self.headers.split("\r\n").filter_map(move |line| {
            let (key, value) = line.split_once(':')?;
            let key = key.trim();
            (key.eq_ignore_ascii_case(name) || key.eq_ignore_ascii_case(compact)).then(|| value.trim())
        })
    }

    /// The header lines as written, in order (for a proxy that copies a message with a few changes).
    pub fn lines(&self) -> impl Iterator<Item = &'a str> {
        self.headers.split("\r\n").filter(|line| !line.is_empty())
    }

    pub fn header(&self, name: &'a str) -> Option<&'a str> {
        self.headers(name).next()
    }

    /// `(number, METHOD)` of the CSeq header.
    pub fn cseq(&self) -> Option<(u32, &'a str)> {
        let (number, method) = self.header("cseq")?.split_once(' ')?;
        Some((number.parse().ok()?, method.trim()))
    }
}

/// The URI inside `"Name" <sip:a@b>;tag=x` (or a bare `sip:a@b;tag=x`), without parameters.
pub fn uri_of(value: &str) -> &str {
    match (value.find('<'), value.find('>')) {
        (Some(start), Some(end)) if start < end => &value[start + 1..end],
        _ => value.split(';').next().unwrap_or(value).trim(),
    }
}

/// The `tag=` parameter after the URI, if any.
pub fn tag_of(value: &str) -> Option<&str> {
    let after = value.rfind('>').map_or(value, |end| &value[end..]);
    after.split(';').skip(1).find_map(|parameter| parameter.trim().strip_prefix("tag="))
}

pub fn reason(code: u16) -> &'static str {
    match code {
        100 => "Trying",
        180 => "Ringing",
        200 => "OK",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        480 => "Temporarily Unavailable",
        483 => "Too Many Hops",
        503 => "Service Unavailable",
        400 => "Bad Request",
        405 => "Method Not Allowed",
        415 => "Unsupported Media Type",
        481 => "Call/Transaction Does Not Exist",
        486 => "Busy Here",
        488 => "Not Acceptable Here",
        _ => "Error",
    }
}

pub const ALLOW: &str = "INVITE, ACK, BYE, OPTIONS";

/// A response to `request`: the Via, From, To (with `local_tag` added when it has none), Call-ID and CSeq copied back,
/// our `contact`, an optional warning text and an SDP body. Returns its length (0 if `out` is too small).
pub fn write_response(out: &mut [u8], request: &Message, code: u16, local_tag: &str, contact: &str, warning: Option<&str>, body: &[u8]) -> usize {
    let mut cursor = Cursor::new(out);
    let _ = write!(cursor, "SIP/2.0 {code} {}\r\n", reason(code));
    for via in request.headers("via") {
        let _ = write!(cursor, "Via: {via}\r\n");
    }
    let _ = write!(cursor, "From: {}\r\n", request.header("from").unwrap_or(""));
    let to = request.header("to").unwrap_or("");
    if tag_of(to).is_some() || code == 100 {
        let _ = write!(cursor, "To: {to}\r\n");
    } else {
        let _ = write!(cursor, "To: {to};tag={local_tag}\r\n");
    }
    let _ = write!(cursor, "Call-ID: {}\r\nCSeq: {} {}\r\n", request.header("call-id").unwrap_or(""), request.cseq().map_or(0, |c| c.0), request.cseq().map_or("", |c| c.1));
    let _ = write!(cursor, "Contact: <{contact}>\r\nAllow: {ALLOW}\r\n");
    if let Some(text) = warning {
        let _ = write!(cursor, "Warning: 399 fos \"{text}\"\r\n");
    }
    finish(&mut cursor, body)
}

/// The blank line, Content-Type/Length and the body. Returns the length of the datagram, 0 if it did not fit.
pub fn finish(cursor: &mut Cursor, body: &[u8]) -> usize {
    if body.is_empty() {
        let _ = write!(cursor, "Content-Length: 0\r\n\r\n");
    } else {
        let _ = write!(cursor, "Content-Type: application/sdp\r\nContent-Length: {}\r\n\r\n", body.len());
        let end = cursor.length + body.len();
        if end <= cursor.bytes.len() {
            cursor.bytes[cursor.length..end].copy_from_slice(body);
            cursor.length = end;
        } else {
            cursor.overflow = true;
        }
    }
    if cursor.overflow { 0 } else { cursor.length }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INVITE: &str = "INVITE sip:fos@10.0.0.2 SIP/2.0\r\nVia: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bKabc\r\nf: \"Op\" <sip:op@10.0.0.9>;tag=77\r\nTo: <sip:fos@10.0.0.2>\r\ni: call1@10.0.0.9\r\nCSeq: 5 INVITE\r\nContact: <sip:op@10.0.0.9:5060>\r\nContent-Length: 4\r\n\r\nv=0\r\nEXTRA";

    #[test]
    fn parses_a_request() {
        let message = Message::parse(INVITE.as_bytes()).unwrap();
        assert_eq!(message.start, Start::Request { method: "INVITE", uri: "sip:fos@10.0.0.2" });
        assert_eq!(message.header("call-id"), Some("call1@10.0.0.9"));
        assert_eq!(message.cseq(), Some((5, "INVITE")));
        assert_eq!(message.body, "v=0\r");
        assert_eq!(tag_of(message.header("from").unwrap()), Some("77"));
        assert_eq!(uri_of(message.header("from").unwrap()), "sip:op@10.0.0.9");
        assert_eq!(uri_of("sip:a@b;tag=1"), "sip:a@b");
    }

    #[test]
    fn parses_a_status_and_refuses_garbage() {
        let message = Message::parse(b"SIP/2.0 486 Busy Here\r\nCSeq: 1 INVITE\r\n\r\n").unwrap();
        assert_eq!(message.start, Start::Status { code: 486, reason: "Busy Here" });
        assert!(Message::parse(b"GET / HTTP/1.1\r\n\r\n").is_none());
        assert!(Message::parse(b"\xff\xfe").is_none());
    }

    #[test]
    fn response_copies_the_dialog_headers_and_adds_a_tag() {
        let request = Message::parse(INVITE.as_bytes()).unwrap();
        let mut out = [0u8; 600];
        let length = write_response(&mut out, &request, 486, "t1", "sip:fos@10.0.0.2:5060", Some("busy"), b"");
        let reply = Message::parse(&out[..length]).unwrap();
        assert_eq!(reply.start, Start::Status { code: 486, reason: "Busy Here" });
        assert_eq!(tag_of(reply.header("to").unwrap()), Some("t1"));
        assert_eq!(reply.header("via"), request.header("via"));
        assert_eq!(reply.cseq(), Some((5, "INVITE")));
        assert!(reply.header("warning").is_some());
        assert_eq!(write_response(&mut [0u8; 20], &request, 200, "t", "sip:x", None, b""), 0);
    }
}
