//! A small HTTP/1.1 client, written by hand.
//!
//! Kalshi takes orders over REST: plain HTTP requests like
//!     POST /trade-api/v2/portfolio/events/orders
//! We keep one connection open and send requests one after another on it
//! ("keep-alive"). That saves the TCP + TLS setup, about 3 round trips, on
//! every order.
//!
//! Keeping a connection open means we can't use "read until the server hangs
//! up" to find the end of a response (Lesson 1's trick). So we have to frame
//! each response properly, one of two ways:
//!   Content-Length: N         -> the body is exactly N bytes
//!   Transfer-Encoding: chunked -> <hex size>\r\n<data>\r\n ... 0\r\n\r\n

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::eventlog::EventLog;
use crate::net::{self, Header, TlsStream};
use crate::Error;

pub struct Response {
    pub status: u16,
    pub head: String,
    pub body: Vec<u8>,
}

impl Response {
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Try to parse one complete response from the start of `buf`.
///
/// Ok(None)            = not all here yet: read more bytes and call again.
/// Ok(Some((resp, n))) = done; the response used the first n bytes of `buf`
///                       (anything after belongs to the next response).
///
/// A pure function on a buffer: it never reads from the network itself, so
/// the same code works for blocking and non-blocking connections.
pub fn parse_response(buf: &[u8]) -> Result<Option<(Response, usize)>, Error> {
    let Some(i) = find_from(buf, 0, b"\r\n\r\n") else { return Ok(None) };
    let head_len = i + 4;
    let head = String::from_utf8_lossy(&buf[..head_len]).into_owned();
    // Status line: "HTTP/1.1 201 Created"
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("bad status line: {:?}", head.lines().next()))?;
    let rest = &buf[head_len..];

    // These statuses never have a body, whatever the headers say.
    if status == 204 || status == 304 || (100..200).contains(&status) {
        return Ok(Some((Response { status, head, body: Vec::new() }, head_len)));
    }
    let chunked = header_value(&head, "transfer-encoding")
        .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
    if chunked {
        return Ok(decode_chunked(rest)?.map(|(body, n)| (Response { status, head, body }, head_len + n)));
    }
    let Some(len) = header_value(&head, "content-length") else {
        // Legal HTTP ("read until the server hangs up"), but useless on a
        // keep-alive connection, and Kalshi never does it.
        return Err("response has neither Content-Length nor chunked encoding".into());
    };
    let len: usize = len.parse().map_err(|_| format!("bad Content-Length {len:?}"))?;
    if rest.len() < len {
        return Ok(None);
    }
    let body = rest[..len].to_vec();
    Ok(Some((Response { status, head, body }, head_len + len)))
}

/// Blocking version: read from `r` until one whole response is in `buf`.
pub fn read_response<R: Read>(r: &mut R, buf: &mut Vec<u8>) -> Result<Response, Error> {
    loop {
        if let Some((resp, used)) = parse_response(buf)? {
            buf.drain(..used);
            return Ok(resp);
        }
        let mut chunk = [0u8; 4096];
        let n = r.read(&mut chunk)?;
        if n == 0 {
            return Err("connection closed in the middle of a response".into());
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// Try to decode a complete chunked body from the start of `buf`.
/// Ok(None) = not all here yet, read more and call again.
/// Ok(Some((body, n))) = done, and the body used the first n bytes of `buf`.
fn decode_chunked(buf: &[u8]) -> Result<Option<(Vec<u8>, usize)>, Error> {
    let mut body = Vec::new();
    let mut pos = 0;
    loop {
        // Size line: hex digits, maybe ";extensions", then \r\n.
        let Some(eol) = find_from(buf, pos, b"\r\n") else { return Ok(None) };
        let line = String::from_utf8_lossy(&buf[pos..eol]);
        let hex = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(hex, 16).map_err(|_| format!("bad chunk size {hex:?}"))?;
        pos = eol + 2;
        if size == 0 {
            // Last chunk. Optional "trailer" headers, then a blank line.
            loop {
                let Some(eol) = find_from(buf, pos, b"\r\n") else { return Ok(None) };
                let blank = eol == pos;
                pos = eol + 2;
                if blank {
                    return Ok(Some((body, pos)));
                }
            }
        }
        if buf.len() < pos + size + 2 {
            return Ok(None);
        }
        body.extend_from_slice(&buf[pos..pos + size]);
        if &buf[pos + size..pos + size + 2] != b"\r\n" {
            return Err("chunk not followed by \\r\\n".into());
        }
        pos += size + 2;
    }
}

/// Position of `needle` in `haystack` at or after `from`.
pub fn find_from(haystack: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    haystack.get(from..)?.windows(needle.len()).position(|w| w == needle).map(|i| i + from)
}

/// Value of an HTTP header. Header names are case-insensitive.
pub fn header_value<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().skip(1).find_map(|line| {
        let (k, v) = line.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

/// One keep-alive HTTPS connection to one host.
pub struct Client {
    host: String,
    conn: u64,
    tls: Option<TlsStream>,
    buf: Vec<u8>,
    last_used: Option<Instant>,
}

/// Servers close idle keep-alive connections, and a blocking client only
/// finds out when its next request fails. Rather than risk that on an order,
/// reconnect proactively if the connection has been idle this long.
const MAX_IDLE: Duration = Duration::from_secs(15);

impl Client {
    pub fn new(host: &str, conn: u64) -> Client {
        Client { host: host.into(), conn, tls: None, buf: Vec::new(), last_used: None }
    }

    /// Send one request and wait for its response, logging both.
    /// `path` may include a query string ("?a=b").
    pub fn request(
        &mut self,
        log: &mut EventLog,
        method: &str,
        path: &str,
        headers: &[Header],
        body: Option<&str>,
    ) -> Result<Response, Error> {
        // Reading the clock is fine here: this is the network edge.
        if self.tls.is_some() && self.last_used.is_some_and(|t| t.elapsed() > MAX_IDLE) {
            self.disconnect(log, "idle, reconnecting")?;
        }
        if self.tls.is_none() {
            self.tls = Some(net::tls_connect(log, self.conn, &self.host)?);
            self.buf.clear();
        }

        let build = |redact: bool| {
            let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {}\r\nAccept: application/json\r\n", self.host);
            if let Some(b) = body {
                req.push_str(&format!("Content-Type: application/json\r\nContent-Length: {}\r\n", b.len()));
            }
            net::push_headers(&mut req, headers, redact);
            req.push_str("\r\n");
            req.push_str(body.unwrap_or(""));
            req
        };
        log.write(self.conn, "http_request", json!(build(true)))?; // log BEFORE sending

        let result = (|| {
            let tls = self.tls.as_mut().expect("connected above");
            tls.write_all(build(false).as_bytes())?;
            tls.flush()?;
            read_response(tls, &mut self.buf)
        })();

        match result {
            Ok(resp) => {
                log.write(self.conn, "http_response", json!(resp.head))?;
                log.write(self.conn, "http_response_body", json!(resp.body_text()))?;
                self.last_used = Some(Instant::now());
                if header_value(&resp.head, "connection").is_some_and(|v| v.eq_ignore_ascii_case("close")) {
                    self.disconnect(log, "server asked to close")?;
                }
                Ok(resp)
            }
            Err(e) => {
                // We don't know what state the connection is in: drop it.
                self.disconnect(log, &e.to_string())?;
                Err(e)
            }
        }
    }

    fn disconnect(&mut self, log: &mut EventLog, why: &str) -> Result<(), Error> {
        if self.tls.take().is_some() {
            log.write(self.conn, "tcp_close", json!(why))?;
        }
        self.buf.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake connection that hands out at most 3 bytes per read, like a
    /// very slow network. Good code must not care.
    struct Trickle {
        data: Vec<u8>,
        pos: usize,
    }

    impl Read for Trickle {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            let n = (self.data.len() - self.pos).min(3).min(out.len());
            out[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    fn trickle(s: &str) -> Trickle {
        Trickle { data: s.as_bytes().to_vec(), pos: 0 }
    }

    fn read_one(r: &mut Trickle, buf: &mut Vec<u8>) -> (u16, String) {
        let resp = read_response(r, buf).unwrap();
        (resp.status, resp.body_text())
    }

    #[test]
    fn content_length_and_keep_alive() {
        // Two responses back to back on one connection.
        let mut r = trickle(
            "HTTP/1.1 201 Created\r\nContent-Length: 11\r\n\r\n{\"ok\":true}\
             HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}",
        );
        let mut buf = Vec::new();
        assert_eq!(read_one(&mut r, &mut buf), (201, "{\"ok\":true}".into()));
        assert_eq!(read_one(&mut r, &mut buf), (200, "{}".into()));
        assert!(buf.is_empty());
    }

    #[test]
    fn chunked_body_in_tiny_pieces() {
        let mut r = trickle(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
             4\r\n{\"a\"\r\n6;ext=1\r\n:12345\r\n1\r\n}\r\n0\r\n\r\n\
             HTTP/1.1 204 No Content\r\n\r\n",
        );
        let mut buf = Vec::new();
        assert_eq!(read_one(&mut r, &mut buf), (200, "{\"a\":12345}".into()));
        assert_eq!(read_one(&mut r, &mut buf), (204, "".into()));
    }

    #[test]
    fn parse_response_incrementally() {
        let full = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}NEXT";
        // Every prefix shorter than the whole response is "not yet".
        for n in 0..full.len() - 4 {
            assert!(parse_response(&full[..n]).unwrap().is_none(), "prefix {n}");
        }
        let (resp, used) = parse_response(full).unwrap().unwrap();
        assert_eq!((resp.status, resp.body_text().as_str(), used), (200, "{}", full.len() - 4));
        assert!(parse_response(b"HTTP/1.1 200 OK\r\n\r\nbody").is_err()); // no length
    }

    #[test]
    fn chunked_needs_more_data() {
        assert!(decode_chunked(b"5\r\nhel").unwrap().is_none());
        assert!(decode_chunked(b"5\r\nhello\r\n0\r\n").unwrap().is_none());
        assert_eq!(decode_chunked(b"5\r\nhello\r\n0\r\n\r\nXYZ").unwrap(), Some((b"hello".to_vec(), 15)));
        assert!(decode_chunked(b"zz\r\n").is_err());
    }

    #[test]
    fn connection_closed_mid_response_is_an_error() {
        let mut r = trickle("HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nshort");
        let mut buf = Vec::new();
        assert!(read_response(&mut r, &mut buf).is_err());
    }
}
