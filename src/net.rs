//! Opening encrypted connections, shared by the websocket and the REST client.
//!
//!   TCP     a reliable byte pipe to host:443                  (std::net)
//!   TLS     encrypts that pipe and proves who the server is   (rustls)

use std::net::TcpStream;
use std::sync::Arc;

use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use serde_json::json;

use crate::eventlog::EventLog;
use crate::Error;

/// A TLS session wrapped around a TCP socket. Reading from it gives plaintext.
pub type TlsStream = StreamOwned<ClientConnection, TcpStream>;

/// An extra HTTP header for a request.
pub struct Header {
    pub name: String,
    pub value: String,
    /// Secret values are replaced with "<redacted>" in the log. The log is
    /// a record of what happened, and must never become a credential store.
    pub secret: bool,
}

impl Header {
    pub fn public(name: &str, value: &str) -> Header {
        Header { name: name.into(), value: value.into(), secret: false }
    }
    pub fn secret(name: &str, value: &str) -> Header {
        Header { name: name.into(), value: value.into(), secret: true }
    }
}

/// Append `Name: value\r\n` lines, blanking secret values if `redact`.
pub fn push_headers(out: &mut String, headers: &[Header], redact: bool) {
    for h in headers {
        let value = if redact && h.secret { "<redacted>" } else { h.value.as_str() };
        out.push_str(&format!("{}: {}\r\n", h.name, value));
    }
}

/// Open TCP, then TLS, to host:443, logging each step.
pub fn tls_connect(log: &mut EventLog, conn: u64, host: &str) -> Result<TlsStream, Error> {
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
    Ok(tls)
}
