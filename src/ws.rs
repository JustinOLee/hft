//! Open a websocket: TCP + TLS (net.rs), then an HTTP "upgrade" request,
//! then websocket message framing for the rest of the connection.
//!
//!   HTTP    one request/response asking to "upgrade"          (written by hand)
//!   WS      message framing for the rest of the connection    (tungstenite)

use std::io::Write;
use std::time::Duration;

use serde_json::json;
use tungstenite::handshake::client::generate_key;
use tungstenite::handshake::derive_accept_key;
use tungstenite::protocol::Role;
use tungstenite::{Message, WebSocket};

use crate::eventlog::EventLog;
use crate::http::{header_value, read_body, read_head};
use crate::net::{self, Header, TlsStream};
use crate::Error;

/// A websocket running over TLS.
pub type Ws = WebSocket<TlsStream>;

pub fn connect(
    log: &mut EventLog,
    conn: u64,
    host: &str,
    path: &str,
    headers: &[Header],
) -> Result<Ws, Error> {
    let mut tls = net::tls_connect(log, conn, host)?;

    // A websocket starts life as an ordinary HTTP GET with special headers.
    // The random key lets us check that the server really speaks websocket.
    let key = generate_key();
    // Build the request twice: the real one, and a copy for the log with
    // secret header values blanked out.
    let build = |redact: bool| {
        let mut req = format!(
            "GET {path} HTTP/1.1\r\n\
             Host: {host}\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: {key}\r\n\
             Sec-WebSocket-Version: 13\r\n"
        );
        net::push_headers(&mut req, headers, redact);
        req.push_str("\r\n");
        req
    };
    log.write(conn, "http_request", json!(build(true)))?; // log BEFORE sending
    tls.write_all(build(false).as_bytes())?;
    tls.flush()?;

    let mut buf = Vec::new();
    let (status, head) = read_head(&mut tls, &mut buf)?;
    log.write(conn, "http_response", json!(head))?;

    // 101 Switching Protocols = "OK, this connection is a websocket now".
    if status != 101 {
        // A refusal usually explains itself in a short body.
        let body = read_body(&mut tls, &mut buf, status, &head).unwrap_or_default();
        let body = String::from_utf8_lossy(&body).into_owned();
        log.write(conn, "http_response_body", json!(body))?;
        let status_line = head.lines().next().unwrap_or("");
        return Err(format!("websocket upgrade refused: {status_line} {body}").into());
    }
    let expected = derive_accept_key(key.as_bytes());
    if header_value(&head, "sec-websocket-accept") != Some(expected.as_str()) {
        return Err("bad Sec-WebSocket-Accept from server".into());
    }

    // Anything read past the headers is already websocket data (the server
    // may have sent its first message right away). Hand it over, don't drop it.
    Ok(WebSocket::from_partially_read(tls, buf, Role::Client, None))
}

/// Make `read` give up after `timeout` instead of waiting forever, so the
/// network loop can also check for actions to send. A stopgap: step 7
/// replaces it with a proper event loop (kqueue/epoll).
pub fn set_read_timeout(ws: &mut Ws, timeout: Duration) -> Result<(), Error> {
    ws.get_mut().sock.set_read_timeout(Some(timeout))?;
    Ok(())
}

/// True if a read error just means "nothing arrived before the timeout".
pub fn is_timeout(e: &tungstenite::Error) -> bool {
    matches!(e, tungstenite::Error::Io(io)
        if matches!(io.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut))
}

/// Send a text message, logging it first.
pub fn send_text(ws: &mut Ws, log: &mut EventLog, conn: u64, text: &str) -> Result<(), Error> {
    log.write(conn, "ws_out", json!(text))?;
    ws.send(Message::text(text))?;
    Ok(())
}
