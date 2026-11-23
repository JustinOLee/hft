//! Follow a file as another thread appends to it (like `tail -f`).
//!
//! The tricky part is the same framing problem as before: when we hit the end
//! of the file, the writer may be halfway through a line. We must never hand
//! out half a line, so we keep the partial text and wait for the rest.
//!
//! This is a deliberately simple stand-in for a real inter-thread queue.
//! queue.rs wraps it behind the EventReader interface (plan step 9).

use std::fs::File;
use std::io::{BufRead, BufReader, ErrorKind};

use flate2::read::MultiGzDecoder;
use std::thread;
use std::time::{Duration, Instant};

/// Keep checking without sleeping for this long after the last new data.
/// Busy-polling ("spinning") reacts within microseconds but keeps one CPU
/// core fully busy, so we fall back to sleeping once things go quiet.
const SPIN_FOR: Duration = Duration::from_millis(100);

use crate::Error;

pub struct Tailer {
    reader: Box<dyn BufRead>,
    /// Text read so far for the current, possibly incomplete, line.
    partial: String,
    /// Stop at end of file instead of waiting for more (used by replay).
    follow: bool,
    /// When we last got new data.
    last_data: Instant,
}

impl Tailer {
    pub fn open(path: &str, follow: bool) -> Result<Tailer, Error> {
        let file = File::open(path)?;
        let reader: Box<dyn BufRead> = if path.ends_with(".gz") {
            if follow {
                return Err("can't follow a .gz log live (it's written in compressed blocks)".into());
            }
            Box::new(BufReader::new(MultiGzDecoder::new(file)))
        } else {
            Box::new(BufReader::new(file))
        };
        Ok(Tailer { reader, partial: String::new(), follow, last_data: Instant::now() })
    }

    /// Return the next complete line, waiting for one if necessary.
    /// Returns `None` at end of file when not following.
    pub fn next_line(&mut self) -> Result<Option<String>, Error> {
        loop {
            // read_line appends whatever is available, up to and including '\n'.
            let n = match self.reader.read_line(&mut self.partial) {
                Ok(n) => n,
                // A .gz recording that was cut off (crash, Ctrl-C) ends
                // without its proper trailer. Treat that as the end.
                Err(e) if e.kind() == ErrorKind::UnexpectedEof && !self.follow => 0,
                Err(e) => return Err(e.into()),
            };
            if n > 0 {
                self.last_data = Instant::now();
            }
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
                // While data is flowing, spin: check again immediately.
                // Once quiet, sleep 1 ms between checks to save CPU (an
                // event arriving then can wait up to 1 ms).
                if self.last_data.elapsed() < SPIN_FOR {
                    std::hint::spin_loop();
                } else {
                    thread::sleep(Duration::from_millis(1));
                }
            }
        }
    }
}
