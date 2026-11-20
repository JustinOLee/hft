//! Open a websocket over TLS over TCP, logging each layer as it comes up.
//!
//!   TCP     a reliable byte pipe to host:443                  (std::net)
//!   TLS     encrypts that pipe and proves who the server is   (rustls)
//!   HTTP    one request/response asking to "upgrade"          (written by hand)
//!   WS      message framing for the rest of the connection    (tungstenite)

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use serde_json::json;
use tungstenite::handshake::client::generate_key;
use tungstenite::handshake::derive_accept_key;
use tungstenite::protocol::Role;
use tungstenite::{Message, WebSocket};

use crate::eventlog::EventLog;
use crate::Error;

/// A TLS session wrapped around a TCP socket. Reading from it gives plaintext.
pub type TlsStream = StreamOwned<ClientConnection, TcpStream>;
/// A websocket running on top of that.
pub type Ws = WebSocket<TlsStream>;

pub fn connect(log: &mut EventLog, conn: u64, host: &str, path: &str) -> Result<Ws, Error> {
    // ---- TCP ---------------------------------------------------------------
    log.write(conn, "tcp_connect", json!({ "host": host, "port": 443 }))?;
    let tcp = TcpStream::connect((host, 443))?;
    // Nagle's algorithm delays small writes to batch them into fewer packets.
    // That trades latency for efficiency, which is the wrong trade for us.
    tcp.set_nodelay(true)?;
    log.write(
        conn,
        "tcp_open",
        json!({ "local": tcp.local_addr()?.to_string(), "peer": tcp.peer_addr()?.to_string() }),
    )?;

    // ---- TLS ---------------------------------------------------------------
    // Trust the same root certificate authorities Mozilla's browsers trust.
    let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    // The server name is used to check the certificate (and sent as SNI).
    let server_name = ServerName::try_from(host.to_string())?;
    let tls_conn = ClientConnection::new(Arc::new(config), server_name)?;
    let mut tls = StreamOwned::new(tls_conn, tcp);

    // rustls would handshake lazily on first use. We drive it to completion
    // now so the log shows exactly when the encrypted channel is ready.
    while tls.conn.is_handshaking() {
        tls.conn.complete_io(&mut tls.sock)?;
    }
    log.write(
        conn,
        "tls_open",
        json!({
            "version": format!("{:?}", tls.conn.protocol_version()),
            "cipher": format!("{:?}", tls.conn.negotiated_cipher_suite().map(|s| s.suite())),
        }),
    )?;

    // ---- HTTP upgrade ------------------------------------------------------
    // A websocket starts life as an ordinary HTTP GET with special headers.
    // The random key lets us check that the server really speaks websocket.
    let key = generate_key();
    let request = format!(
        "GET {path} HTTP/1.1\r\n\
         Host: {host}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: {key}\r\n\
         Sec-WebSocket-Version: 13\r\n\
         \r\n"
    );
    log.write(conn, "http_request", json!(request))?; // log BEFORE sending
    tls.write_all(request.as_bytes())?;
    tls.flush()?;

    // Read until the blank line that ends the headers. Same framing problem as
    // Lesson 1: reads return arbitrary-sized pieces, so we collect and search.
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if let Some(i) = find(&buf, b"\r\n\r\n") {
            break i + 4;
        }
        let n = tls.read(&mut chunk)?;
        if n == 0 {
            return Err("server closed the connection during the websocket handshake".into());
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    log.write(conn, "http_response", json!(head))?;

    // 101 Switching Protocols = "OK, this connection is a websocket now".
    if !head.starts_with("HTTP/1.1 101") {
        return Err(format!("websocket upgrade refused: {}", head.lines().next().unwrap_or("")).into());
    }
    let expected = derive_accept_key(key.as_bytes());
    if header(&head, "sec-websocket-accept") != Some(expected.as_str()) {
        return Err("bad Sec-WebSocket-Accept from server".into());
    }

    // Anything we read past the headers is already websocket data (the server
    // may have sent its first message right away). Hand it over, don't drop it.
    let leftover = buf[header_end..].to_vec();
    Ok(WebSocket::from_partially_read(tls, leftover, Role::Client, None))
}

/// Send a text message, logging it first.
pub fn send_text(ws: &mut Ws, log: &mut EventLog, conn: u64, text: &str) -> Result<(), Error> {
    log.write(conn, "ws_out", json!(text))?;
    ws.send(Message::text(text))?;
    Ok(())
}

/// Position of `needle` in `haystack`, if present.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Value of an HTTP header. Header names are case-insensitive.
fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().find_map(|line| {
        let (k, v) = line.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}
