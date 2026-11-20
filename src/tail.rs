//! Follow a file as another thread appends to it (like `tail -f`).
//!
//! The tricky part is the same framing problem as before: when we hit the end
//! of the file, the writer may be halfway through a line. We must never hand
//! out half a line, so we keep the partial text and wait for the rest.
//!
//! This is a deliberately simple stand-in for a real inter-thread queue.
//! Plan step 9 replaces it with a proper queue interface.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::thread;
use std::time::Duration;

use crate::Error;

pub struct Tailer {
    reader: BufReader<File>,
    /// Text read so far for the current, possibly incomplete, line.
    partial: String,
    /// Stop at end of file instead of waiting for more (used by replay).
    follow: bool,
}

impl Tailer {
    pub fn open(path: &str, follow: bool) -> Result<Tailer, Error> {
        let file = File::open(path)?;
        Ok(Tailer { reader: BufReader::new(file), partial: String::new(), follow })
    }

    /// Return the next complete line, waiting for one if necessary.
    /// Returns `None` at end of file when not following.
    pub fn next_line(&mut self) -> Result<Option<String>, Error> {
        loop {
            // read_line appends whatever is available, up to and including '\n'.
            let n = self.reader.read_line(&mut self.partial)?;
            if self.partial.ends_with('\n') {
                self.partial.pop(); // drop the '\n'
                // `take` moves the String out and leaves an empty one behind.
                return Ok(Some(std::mem::take(&mut self.partial)));
            }
            if n == 0 {
                // End of file, and any partial line is still incomplete.
                if !self.follow {
                    return Ok(None);
                }
                // Polling: sleeping 1 ms means an event can wait up to 1 ms
                // before we notice it. Fine for now; the latency doesn't
                // matter yet (plan steps 8-9 come back to this).
                thread::sleep(Duration::from_millis(1));
            }
        }
    }
}
