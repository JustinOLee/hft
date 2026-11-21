//! Queues between the threads (plan step 9).
//!
//!   network thread ──[EventWriter]──► (the log) ──[EventReader]──► state machine
//!   state machine  ──[ActionSink, in main.rs]──────────────────►  network thread
//!
//! The rest of the program only sees these two traits, so how events travel
//! can change without touching the reactor or the state machine.
//!
//! Today the event queue is a plain file: the writer appends JSON lines and
//! the reader follows the file (tail.rs). It works, every event is on disk for
//! replay, and it costs ~1-3 ms per hop (step 8).
//!
//! TODO(later): write our own version of Chronicle Queue
//! (https://github.com/OpenHFT/Chronicle-Queue). The idea:
//!   - the "file" is memory-mapped: both threads see the same memory, so
//!     publishing an event is a memory write, not a system call
//!   - events are binary with a fixed header (length + type), not JSON
//!   - the writer bumps a sequence number after each event; readers spin on
//!     that number, so a hop takes well under a microsecond
//!   - the mapped file is still on disk, so replay keeps working unchanged
//!
//! A trait in Rust is a set of methods a type promises to provide. Code that
//! takes `&mut dyn EventWriter` works with ANY type that implements it.

use std::io;

use serde_json::Value;

use crate::event::Event;
use crate::eventlog::EventLog;
use crate::tail::Tailer;
use crate::Error;

/// Something events can be written to.
/// `Send` = may be moved to another thread (the network thread owns it).
pub trait EventWriter: Send {
    fn write(&mut self, conn: u64, kind: &str, data: Value) -> io::Result<()>;
}

/// Something events can be read from, in order.
pub trait EventReader {
    /// The next event; `None` when there will never be more (end of a
    /// replayed file).
    fn next_event(&mut self) -> Result<Option<Event>, Error>;
}

impl EventWriter for EventLog {
    fn write(&mut self, conn: u64, kind: &str, data: Value) -> io::Result<()> {
        EventLog::write(self, conn, kind, data)
    }
}

/// Reads events from a log file, optionally following it as it grows.
pub struct FileReader {
    tailer: Tailer,
}

impl FileReader {
    pub fn open(path: &str, follow: bool) -> Result<FileReader, Error> {
        Ok(FileReader { tailer: Tailer::open(path, follow)? })
    }
}

impl EventReader for FileReader {
    fn next_event(&mut self) -> Result<Option<Event>, Error> {
        match self.tailer.next_line()? {
            Some(line) => Ok(Some(Event::parse(&line)?)),
            None => Ok(None),
        }
    }
}
