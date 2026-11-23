//! Parsing log lines back into typed events.
//!
//! The log stores JSON (easy to inspect with any tool). The state machine
//! wants Rust types it can `match` on, so the compiler checks we've handled
//! every kind of event. This module converts one into the other.

use serde_json::Value;

use crate::action::Action;
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
#[allow(dead_code)] // some payloads aren't read yet; later steps use them
pub enum Kind {
    /// First event of a session. Carries the session's settings, because
    /// settings are inputs too: replay must see the same ones.
    Start(Value),
    TcpConnect,
    TcpOpen,
    TlsOpen,
    HttpRequest(String),
    HttpResponse(String),
    HttpResponseBody(String),
    /// A REST request failed before we got a response.
    HttpError(String),
    /// An action the network thread is about to carry out.
    Action(Action),
    /// Once a second, from the network loop: lets the state machine act
    /// even when no messages arrive.
    Timer,
    WsOut(String),
    WsIn(String),
    WsClose,
    TcpClose(String),
    /// Any other kind, with its name and data. The lab's recorder uses this
    /// for "market" and "resolution" events. Kept rather than rejected so
    /// old state machines can read newer logs.
    Other(String, Value),
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
            "start" => Kind::Start(v["data"].clone()),
            "tcp_connect" => Kind::TcpConnect,
            "tcp_open" => Kind::TcpOpen,
            "tls_open" => Kind::TlsOpen,
            "http_request" => Kind::HttpRequest(text()),
            "http_response" => Kind::HttpResponse(text()),
            "http_response_body" => Kind::HttpResponseBody(text()),
            "http_error" => Kind::HttpError(text()),
            "timer" => Kind::Timer,
            "action" => Kind::Action(serde_json::from_value(v["data"].clone())?),
            "ws_out" => Kind::WsOut(text()),
            "ws_in" => Kind::WsIn(text()),
            "ws_close" => Kind::WsClose,
            "tcp_close" => Kind::TcpClose(text()),
            other => Kind::Other(other.to_string(), v["data"].clone()),
        };
        Ok(Event { ts, conn, kind })
    }
}
