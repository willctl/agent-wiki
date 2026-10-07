//! A small HTTP/1.1 server for 127.0.0.1: one thread per connection (bounded), keep-alive, whole
//! request bodies read up front (Content-Length or chunked, capped), every response written in one
//! piece with Content-Length and TCP_NODELAY. Strict where ambiguity could smuggle a request: no
//! obsolete line folding, no Content-Length together with Transfer-Encoding, one Host.

use aw_core::waker::Waker;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// Largest request body read; larger ones get 413 and the connection closes.
pub const MAX_BODY: usize = 4 * 1024 * 1024;
const MAX_HEAD: usize = 16 * 1024; // as Node's default --max-http-header-size
const MAX_CONNECTIONS: usize = 128;
const KEEP_ALIVE_SECS: u64 = 5; // as Node's server.keepAliveTimeout
const READ_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Request {
    pub method: String,
    pub url: String,
    headers: Vec<(String, String)>,
    body: Result<Vec<u8>, u16>,
}

impl Request {
    /// A header's value; repeated headers are joined with ", " as Node joins them.
    pub fn header(&self, name: &str) -> Option<String> {
        let vals: Vec<&str> = self.headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str()).collect();
        if vals.is_empty() { None } else { Some(vals.join(", ")) }
    }
    /// The body, or the status to answer with (413: too large).
    pub fn body(&self) -> Result<&[u8], u16> {
        self.body.as_deref().map_err(|s| *s)
    }
}

pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

pub type Handler = dyn Fn(&Request) -> Response + Send + Sync;

/// Accepts connections until `stop`. Connections already open finish their current request.
pub fn serve(listener: TcpListener, handler: Arc<Handler>, stop: Arc<Waker>) {
    let open = Arc::new(AtomicUsize::new(0));
    if let Ok(addr) = listener.local_addr() {
        let stop = stop.clone();
        // accept() blocks: a connection to ourselves wakes it once stopping.
        std::thread::spawn(move || {
            while !stop.wait_stop(3_600_000) {}
            let _ = TcpStream::connect(addr);
        });
    }
    loop {
        let accepted = listener.accept();
        if stop.stopped() {
            return;
        }
        let Ok((stream, _)) = accepted else { continue };
        if open.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
            let _ = (&stream).write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            continue;
        }
        open.fetch_add(1, Ordering::SeqCst);
        let (count, handler) = (open.clone(), handler.clone());
        let spawned = std::thread::Builder::new().name("http".into()).spawn(move || {
            connection(stream, &*handler);
            count.fetch_sub(1, Ordering::SeqCst);
        });
        if spawned.is_err() {
            open.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

enum Next {
    Request(Request, bool), // keep-alive after answering?
    Reject(u16),            // answer, then close
    Closed,
}

fn connection(stream: TcpStream, handler: &Handler) {
    let _ = stream.set_nodelay(true);
    let _ = stream.set_write_timeout(Some(READ_TIMEOUT));
    let Ok(write_half) = stream.try_clone() else { return };
    let mut out = write_half;
    let mut reader = BufReader::with_capacity(8192, stream);
    loop {
        match read_request(&mut reader, &mut out) {
            Next::Closed => return,
            Next::Reject(status) => {
                let _ = out.write_all(&encode(&Response { status, headers: vec![], body: vec![] }, false, false));
                return;
            }
            Next::Request(req, keep) => {
                let resp = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler(&req))).unwrap_or_else(|_| Response { status: 500, headers: vec![], body: b"Internal error".to_vec() });
                let keep = keep && req.body.is_ok() && !resp.headers.iter().any(|(k, v)| k.eq_ignore_ascii_case("connection") && v.eq_ignore_ascii_case("close"));
                if out.write_all(&encode(&resp, keep, req.method == "HEAD")).is_err() || !keep {
                    return;
                }
            }
        }
    }
}

fn encode(resp: &Response, keep: bool, head: bool) -> Vec<u8> {
    let mut b = Vec::with_capacity(256 + resp.body.len());
    let _ = write!(b, "HTTP/1.1 {} {}\r\n", resp.status, reason(resp.status));
    for (k, v) in &resp.headers {
        if k.eq_ignore_ascii_case("connection") || k.eq_ignore_ascii_case("content-length") || k.contains(['\r', '\n', ':']) || v.contains(['\r', '\n']) {
            continue;
        }
        let _ = write!(b, "{k}: {v}\r\n");
    }
    let bodiless = resp.status == 204 || resp.status == 304 || (100..200).contains(&resp.status);
    if !bodiless {
        let _ = write!(b, "Content-Length: {}\r\n", resp.body.len());
    }
    if keep {
        let _ = write!(b, "Connection: keep-alive\r\nKeep-Alive: timeout={KEEP_ALIVE_SECS}\r\n\r\n");
    } else {
        b.extend_from_slice(b"Connection: close\r\n\r\n");
    }
    if !head && !bodiless {
        b.extend_from_slice(&resp.body);
    }
    b
}

fn reason(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        304 => "Not Modified",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        408 => "Request Timeout",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        505 => "HTTP Version Not Supported",
        _ => "",
    }
}

/// One line of the request head, without its CRLF; `budget` is what is left of MAX_HEAD.
fn read_line(r: &mut BufReader<TcpStream>, budget: &mut usize) -> Result<String, u16> {
    let mut line = Vec::new();
    let n = r.by_ref().take(*budget as u64 + 1).read_until(b'\n', &mut line).map_err(|_| 408u16)?;
    if n == 0 {
        return Err(0);
    }
    if n > *budget || !line.ends_with(b"\n") {
        return Err(if n > *budget { 431 } else { 400 });
    }
    *budget -= n;
    line.pop();
    if line.ends_with(b"\r") {
        line.pop();
    }
    String::from_utf8(line).map_err(|_| 400)
}

fn is_token(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|c| c.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&c))
}

fn read_request(r: &mut BufReader<TcpStream>, out: &mut TcpStream) -> Next {
    // Idle keep-alive connections close after a few seconds; a request, once started, gets longer.
    let _ = r.get_ref().set_read_timeout(Some(Duration::from_secs(KEEP_ALIVE_SECS)));
    match r.fill_buf() {
        Ok([]) | Err(_) => return Next::Closed,
        Ok(_) => {}
    }
    let _ = r.get_ref().set_read_timeout(Some(READ_TIMEOUT));
    let mut budget = MAX_HEAD;
    let mut line = match read_line(r, &mut budget) {
        Ok(l) => l,
        Err(0) => return Next::Closed,
        Err(s) => return Next::Reject(s),
    };
    while line.is_empty() {
        // RFC 9112 2.2: ignore an empty line before the request line.
        line = match read_line(r, &mut budget) {
            Ok(l) => l,
            Err(0) => return Next::Closed,
            Err(s) => return Next::Reject(s),
        };
    }
    let parts: Vec<&str> = line.split(' ').collect();
    if parts.len() != 3 || !is_token(parts[0]) || parts[1].is_empty() || parts[1].bytes().any(|c| c <= b' ' || c == 0x7f) {
        return Next::Reject(400);
    }
    let http10 = match parts[2] {
        "HTTP/1.1" => false,
        "HTTP/1.0" => true,
        v if v.starts_with("HTTP/") => return Next::Reject(505),
        _ => return Next::Reject(400),
    };
    let (method, url) = (parts[0].to_string(), parts[1].to_string());
    let mut headers: Vec<(String, String)> = vec![];
    loop {
        let l = match read_line(r, &mut budget) {
            Ok(l) => l,
            Err(0) => return Next::Closed,
            Err(s) => return Next::Reject(s),
        };
        if l.is_empty() {
            break;
        }
        if l.starts_with([' ', '\t']) {
            return Next::Reject(400); // obsolete line folding
        }
        let Some((k, v)) = l.split_once(':') else { return Next::Reject(400) };
        if !is_token(k) || v.bytes().any(|c| (c < b' ' && c != b'\t') || c == 0x7f) {
            return Next::Reject(400);
        }
        headers.push((k.to_string(), v.trim_matches([' ', '\t']).to_string()));
        if headers.len() > 100 {
            return Next::Reject(431);
        }
    }
    let all = |name: &str| headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone()).collect::<Vec<_>>();
    if all("host").len() != 1 && !(http10 && all("host").is_empty()) {
        return Next::Reject(400);
    }
    if all("origin").len() > 1 {
        return Next::Reject(400);
    }
    let conn = all("connection").join(",").to_ascii_lowercase();
    let conn_has = |t: &str| conn.split(',').any(|x| x.trim() == t);
    let keep = if http10 { conn_has("keep-alive") } else { !conn_has("close") };
    let te = all("transfer-encoding");
    let cl = all("content-length");
    if !te.is_empty() && !cl.is_empty() {
        return Next::Reject(400);
    }
    let chunked = match te.join(",").to_ascii_lowercase().split(',').map(str::trim).filter(|t| !t.is_empty()).collect::<Vec<_>>().as_slice() {
        [] => false,
        ["chunked"] if !http10 => true,
        _ => return Next::Reject(501),
    };
    let length = if cl.is_empty() {
        0
    } else {
        let first = cl[0].trim();
        if cl.iter().any(|v| v.trim() != first) || first.is_empty() || !first.bytes().all(|c| c.is_ascii_digit()) {
            return Next::Reject(400);
        }
        first.parse::<u64>().unwrap_or(u64::MAX)
    };
    let expects = all("expect").iter().any(|v| v.eq_ignore_ascii_case("100-continue"));
    let body = if !chunked && length > MAX_BODY as u64 {
        Err(413) // not read: answered, then the connection closes
    } else {
        if expects && (chunked || length > 0) && !http10 && out.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").is_err() {
            return Next::Closed;
        }
        if chunked {
            match read_chunked(r) {
                Ok(b) => Ok(b),
                Err(413) => Err(413),
                Err(s) => return Next::Reject(s),
            }
        } else {
            let mut b = vec![0u8; length as usize];
            if r.read_exact(&mut b).is_err() {
                return Next::Closed;
            }
            Ok(b)
        }
    };
    Next::Request(Request { method, url, headers, body }, keep)
}

fn read_chunked(r: &mut BufReader<TcpStream>) -> Result<Vec<u8>, u16> {
    let mut body = Vec::new();
    let mut budget = MAX_HEAD;
    loop {
        let line = read_line(r, &mut budget).map_err(|s| if s == 0 { 400 } else { s })?;
        let size = line.split(';').next().unwrap_or("").trim();
        if size.is_empty() || size.len() > 8 {
            return Err(if size.len() > 8 { 413 } else { 400 });
        }
        let n = usize::from_str_radix(size, 16).map_err(|_| 400u16)?;
        if n == 0 {
            // Trailers, then the empty line.
            while !read_line(r, &mut budget).map_err(|s| if s == 0 { 400 } else { s })?.is_empty() {}
            return Ok(body);
        }
        if body.len() + n > MAX_BODY {
            return Err(413);
        }
        let start = body.len();
        body.resize(start + n, 0);
        r.read_exact(&mut body[start..]).map_err(|_| 400u16)?;
        let mut crlf = [0u8; 2];
        r.read_exact(&mut crlf).map_err(|_| 400u16)?;
        if &crlf != b"\r\n" {
            return Err(400);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpStream;

    fn server() -> u16 {
        let l = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = l.local_addr().unwrap().port();
        let h: Arc<Handler> = Arc::new(|req: &Request| Response {
            status: 200,
            headers: vec![("Content-Type".into(), "text/plain".into())],
            body: format!("{} {} {}", req.method, req.url, req.body().map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_else(|s| s.to_string())).into_bytes(),
        });
        std::thread::spawn(move || serve(l, h, Waker::new()));
        port
    }

    fn exchange(port: u16, raw: &[u8]) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.write_all(raw).unwrap();
        s.shutdown(std::net::Shutdown::Write).unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out
    }

    #[test]
    fn keep_alive_and_bodies() {
        let port = server();
        let out = exchange(port, b"POST /a HTTP/1.1\r\nHost: x\r\nContent-Length: 3\r\n\r\nabcGET /b HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
        assert!(out.starts_with("HTTP/1.1 200 OK\r\n"), "{out}");
        assert!(out.contains("POST /a abc") && out.contains("GET /b "), "{out}");
        assert!(out.contains("Keep-Alive: timeout=5") && out.ends_with("GET /b "), "{out}");
        let out = exchange(port, b"POST /c HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n3;x=y\r\nabc\r\n2\r\nde\r\n0\r\nT: 1\r\n\r\n");
        assert!(out.contains("POST /c abcde"), "{out}");
        let out = exchange(port, b"POST /d HTTP/1.1\r\nHost: x\r\nExpect: 100-continue\r\nContent-Length: 2\r\n\r\nhi");
        assert!(out.starts_with("HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK"), "{out}");
    }

    #[test]
    fn refuses_ambiguous_requests() {
        let port = server();
        for raw in [
            &b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 3\r\nTransfer-Encoding: chunked\r\n\r\n"[..],
            b"GET / HTTP/1.1\r\nHost: x\r\nHost: y\r\n\r\n",
            b"GET / HTTP/1.1\r\n\r\n",
            b"GET / HTTP/1.1\r\nHost: x\r\n folded\r\n\r\n",
            b"GET / HTTP/1.1\r\nHost: x\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n",
            b"GET  / HTTP/1.1\r\nHost: x\r\n\r\n",
        ] {
            assert!(exchange(port, raw).starts_with("HTTP/1.1 400 "), "{}", String::from_utf8_lossy(raw));
        }
        let out = exchange(port, b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 99999999\r\n\r\n");
        assert!(out.contains("POST / 413") && out.contains("Connection: close"), "{out}");
        let big = format!("GET / HTTP/1.1\r\nHost: x\r\nX: {}\r\n\r\n", "a".repeat(MAX_HEAD));
        assert!(exchange(port, big.as_bytes()).starts_with("HTTP/1.1 431 "));
    }
}
