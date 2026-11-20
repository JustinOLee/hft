//! Parsing log lines back into typed events.
//!
//! The log stores JSON (easy to inspect with any tool). The state machine
//! wants Rust types it can `match` on, so the compiler checks we've handled
//! every kind of event. This module converts one into the other.

use serde_json::Value;

use crate::Error;

/// One line of the log.
#[derive(Debug, Clone)]
pub struct Event {
    /// Local receive/send time, nanoseconds since the Unix epoch.
    /// The state machine must use THIS as "now", never the system clock.
    pub ts: u64,
    /// Which connection this belongs to (0 = none).
    pub conn: u64,
    pub kind: Kind,
}

/// What happened. An `enum` in Rust is a type that is exactly one of several
/// variants, and each variant can carry its own data.
#[derive(Debug, Clone)]
pub enum Kind {
    Start,
    TcpConnect,
    TcpOpen,
    TlsOpen,
    HttpRequest(String),
    HttpResponse(String),
    WsOut(String),
    WsIn(String),
    WsClose,
    TcpClose(String),
    /// Anything we don't model yet (pings, pongs, ...). Kept rather than
    /// rejected so old state machines can read newer logs.
    Other(String),
}

impl Event {
    /// Parse one line of the log (without the trailing newline).
    pub fn parse(line: &str) -> Result<Event, Error> {
        let v: Value = serde_json::from_str(line)?;
        let ts = v["ts"].as_u64().ok_or("event missing ts")?;
        let conn = v["conn"].as_u64().ok_or("event missing conn")?;
        let kind_name = v["kind"].as_str().ok_or("event missing kind")?;
        // For events whose data is a plain string, grab it (or "" if not).
        let text = || v["data"].as_str().unwrap_or("").to_string();

        let kind = match kind_name {
            "start" => Kind::Start,
            "tcp_connect" => Kind::TcpConnect,
            "tcp_open" => Kind::TcpOpen,
            "tls_open" => Kind::TlsOpen,
            "http_request" => Kind::HttpRequest(text()),
            "http_response" => Kind::HttpResponse(text()),
            "ws_out" => Kind::WsOut(text()),
            "ws_in" => Kind::WsIn(text()),
            "ws_close" => Kind::WsClose,
            "tcp_close" => Kind::TcpClose(text()),
            other => Kind::Other(other.to_string()),
        };
        Ok(Event { ts, conn, kind })
    }
}
