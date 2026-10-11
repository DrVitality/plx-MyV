//! The bridge's wire: a deliberately small HTTP/1.1 server for ONE client, this process.
//!
//! The only peer is the app itself on `127.0.0.1` (the listener never binds another address), so
//! this is not a general web server: one request per connection, `Connection: close` on every
//! answer, a bounded head and body, no chunked request bodies (the app never sends one). What it
//! must get exactly right is the part the app's two HTTP clients read — the status line,
//! `Content-Length`, `Content-Type` and, for a media part, `Location` — because `plx_net::stream`
//! and libcurl are both strict about those.
//!
//! [`parse_head`] and [`decode_component`] are pure, so the rules are host-tested without a socket.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// The largest request head accepted. The app's heads are a few hundred bytes of `X-Plex-*`
/// identity headers; anything near this is not the app.
const MAX_HEAD: usize = 32 * 1024;
/// The largest request body accepted. The app sends empty bodies (PUTs and POSTs whose parameters
/// ride the query string); a small allowance keeps an unexpected one from being a hang.
const MAX_BODY: usize = 256 * 1024;
/// How long a connection may sit idle while its request is read. The peer is local and has the
/// whole request ready when it connects.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// One parsed request. Header names keep their spelling; lookups ignore case.
pub struct Request {
    pub method: String,
    pub path: String,
    query: Vec<(String, String)>,
    headers: Vec<(String, String)>,
}

impl Request {
    /// A request built directly: for tests, and for the phone remote, which asks the bridge as
    /// the app does (`super::remote`).
    pub fn new(method: &str, target: &str, headers: &[(&str, &str)]) -> Request {
        let (path, query) = split_target(target);
        Request {
            method: method.to_string(),
            path,
            query,
            headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        }
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn query(&self, name: &str) -> Option<&str> {
        self.query.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    /// The credential the app attached: the `X-Plex-Token` header (control requests) or the query
    /// parameter of the same name (artwork and media URLs, which carry it in the URL). For this
    /// bridge it IS the Stremio account's auth key — see the module doc of `super`.
    pub fn token(&self) -> Option<&str> {
        self.header("X-Plex-Token")
            .or_else(|| self.query("X-Plex-Token"))
            .filter(|t| !t.is_empty())
    }
}

/// One answer. `location` is sent only on a redirect.
pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
    pub location: Option<String>,
}

impl Response {
    pub fn json(status: u16, value: &serde_json::Value) -> Response {
        Response {
            status,
            content_type: "application/json",
            body: serde_json::to_vec(value).unwrap_or_default(),
            location: None,
        }
    }

    pub fn png(bytes: Vec<u8>) -> Response {
        Response { status: 200, content_type: "image/png", body: bytes, location: None }
    }

    pub fn empty(status: u16) -> Response {
        Response { status, content_type: "text/plain", body: Vec::new(), location: None }
    }
}

/// Splits a request target into its path and decoded query pairs.
fn split_target(target: &str) -> (String, Vec<(String, String)>) {
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };
    let pairs = query
        .split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| match kv.split_once('=') {
            Some((k, v)) => (decode_component(k), decode_component(v)),
            None => (decode_component(kv), String::new()),
        })
        .collect();
    (decode_component(path), pairs)
}

/// Percent-decoding of one URL component, `+` read as a space as a form query writes it. An
/// invalid escape is kept literally rather than refused: the peer is the app, and a mangled
/// value is easier to see in a log than a dropped request.
pub fn decode_component(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(b) => {
                        out.push(b);
                        i += 3;
                    }
                    None => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parses a request head (everything before the blank line) into a [`Request`] and the body
/// length it announced. `None` for anything that is not a well-formed HTTP/1.x request.
pub fn parse_head(head: &str) -> Option<(Request, usize)> {
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next()?.split(' ');
    let method = request_line.next()?.to_string();
    let target = request_line.next()?;
    let version = request_line.next()?;
    if method.is_empty() || !target.starts_with('/') || !version.starts_with("HTTP/1.") {
        return None;
    }
    let mut headers = Vec::new();
    for line in lines.filter(|l| !l.is_empty()) {
        let (name, value) = line.split_once(':')?;
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }
    let (path, query) = split_target(target);
    let request = Request { method, path, query, headers };
    let length = match request.header("Content-Length") {
        Some(v) => v.parse::<usize>().ok()?,
        None => 0,
    };
    Some((request, length))
}

/// Reads one request off the connection: the head up to the blank line, then exactly the body it
/// announced (read and discarded — no route needs it). `None` closes the connection unanswered.
pub fn read_request(stream: &mut TcpStream) -> Option<Request> {
    stream.set_read_timeout(Some(READ_TIMEOUT)).ok()?;
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    let head_end = loop {
        if let Some(at) = find_blank_line(&buf) {
            break at;
        }
        if buf.len() > MAX_HEAD {
            return None;
        }
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = std::str::from_utf8(&buf[..head_end]).ok()?;
    let (request, length) = parse_head(head)?;
    if length > MAX_BODY {
        return None;
    }
    let mut have = buf.len() - (head_end + 4);
    while have < length {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        have += n;
    }
    Some(request)
}

fn find_blank_line(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

/// Writes `response` and closes the exchange. A `HEAD` gets the same headers with no body.
pub fn write_response(stream: &mut TcpStream, head_only: bool, response: &Response) -> io::Result<()> {
    let mut out = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status,
        reason(response.status),
        response.content_type,
        response.body.len()
    );
    if let Some(location) = &response.location {
        out.push_str("Location: ");
        out.push_str(location);
        out.push_str("\r\n");
    }
    out.push_str("\r\n");
    stream.write_all(out.as_bytes())?;
    if !head_only {
        stream.write_all(&response.body)?;
    }
    stream.flush()
}
