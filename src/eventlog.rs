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

use flate2::write::GzEncoder;
use flate2::Compression;
use serde_json::{json, Value};

pub struct EventLog {
    sink: Sink,
}

enum Sink {
    /// Plain text: every line is visible to readers immediately (live mode).
    Plain(File),
    /// gzip-compressed, about 10x smaller. Lines sit in the compressor's
    /// memory until the next flush, so nobody can follow this file live.
    /// Used by the lab's recorder, where disk space matters more.
    Gz { enc: GzEncoder<File>, last_flush: u64 },
}

/// Push compressed data to disk at least this often, so a crash or Ctrl-C
/// loses at most about this much of a recording.
const GZ_FLUSH_NS: u64 = 1_000_000_000;

impl EventLog {
    /// Create a NEW log file. Fails if the file already exists: one file per
    /// session, so we never mix two sessions together by accident.
    pub fn create(path: &str) -> io::Result<EventLog> {
        let file = OpenOptions::new().create_new(true).append(true).open(path)?;
        let sink = if path.ends_with(".gz") {
            Sink::Gz { enc: GzEncoder::new(file, Compression::fast()), last_flush: now_nanos() }
        } else {
            Sink::Plain(file)
        };
        Ok(EventLog { sink })
    }

    /// Write one event. For plain files, `File` is unbuffered, so each call
    /// is a single `write` syscall: the line is in the OS's hands by the time
    /// this returns, and another thread tailing the file can see it at once.
    pub fn write(&mut self, conn: u64, kind: &str, data: Value) -> io::Result<()> {
        let ts = now_nanos();
        let event = json!({
            "ts": ts,
            "conn": conn,
            "kind": kind,
            "data": data,
        });
        let mut line = event.to_string();
        line.push('\n');
        match &mut self.sink {
            Sink::Plain(f) => f.write_all(line.as_bytes()),
            Sink::Gz { enc, last_flush } => {
                enc.write_all(line.as_bytes())?;
                if ts - *last_flush >= GZ_FLUSH_NS {
                    enc.flush()?; // a "sync flush": everything so far becomes readable
                    *last_flush = ts;
                }
                Ok(())
            }
        }
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
