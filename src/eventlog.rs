//! Append-only event log: one JSON object per line.
//!
//! Every line looks like:
//!   {"ts":1763661180123456789,"conn":1,"kind":"ws_in","data":"..."}
//!
//! - `ts`   local wall-clock time in nanoseconds since the Unix epoch
//! - `conn` which connection the event belongs to (0 = not connection-specific)
//! - `kind` what happened (tcp_open, http_request, ws_out, ws_in, ...)
//! - `data` the payload, stored exactly as sent or received
//!
//! This file is the source of truth. Later, the state machine will be driven
//! purely by reading these lines, so replaying a file reproduces a session.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

pub struct EventLog {
    file: File,
}

impl EventLog {
    /// Create a NEW log file. Fails if the file already exists: one file per
    /// session, so we never mix two sessions together by accident.
    pub fn create(path: &str) -> io::Result<EventLog> {
        let file = OpenOptions::new().create_new(true).append(true).open(path)?;
        Ok(EventLog { file })
    }

    /// Write one event. `File` is unbuffered, so each call is a single
    /// `write` syscall: the line is in the OS's hands by the time this returns,
    /// and another thread tailing the file can see it immediately.
    pub fn write(&mut self, conn: u64, kind: &str, data: Value) -> io::Result<()> {
        let event = json!({
            "ts": now_nanos(),
            "conn": conn,
            "kind": kind,
            "data": data,
        });
        let mut line = event.to_string();
        line.push('\n');
        self.file.write_all(line.as_bytes())
    }
}

/// Nanoseconds since 1970-01-01. This is the ONLY place the program reads the
/// clock. Everything downstream must use the `ts` stored in the event instead,
/// or replays won't be deterministic (plan step 3).
pub fn now_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before 1970")
        .as_nanos() as u64
}
