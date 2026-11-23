//! Read a session folder (several log files) back as ONE stream of events,
//! in time order.
//!
//! Each feed wrote its own file, and each file is already in time order.
//! So at every step we look at the next event of every file and take the
//! earliest. (This is the "merge" step of merge sort.) If two events have
//! the same timestamp, the file whose name sorts first wins, so the order
//! is the same every time: the merge is deterministic.

use crate::event::Event;
use crate::queue::{EventReader, FileReader};
use crate::Error;

pub struct MergedReader {
    sources: Vec<FileReader>,
    /// The next unread event of each source (None = that file is finished).
    heads: Vec<Option<Event>>,
}

impl MergedReader {
    /// Open every `*.log` file in `dir`.
    pub fn open_dir(dir: &str) -> Result<MergedReader, Error> {
        let mut paths: Vec<String> = std::fs::read_dir(dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path().to_string_lossy().into_owned())
            .filter(|p| p.ends_with(".log"))
            .collect();
        paths.sort();
        if paths.is_empty() {
            return Err(format!("no .log files in {dir}").into());
        }
        MergedReader::open(&paths)
    }

    pub fn open(paths: &[String]) -> Result<MergedReader, Error> {
        let mut sources = Vec::new();
        let mut heads = Vec::new();
        for p in paths {
            let mut r = FileReader::open(p, false)?;
            heads.push(r.next_event()?);
            sources.push(r);
        }
        Ok(MergedReader { sources, heads })
    }
}

impl EventReader for MergedReader {
    fn next_event(&mut self) -> Result<Option<Event>, Error> {
        // (ts, index): comparing tuples compares ts first, then index.
        let earliest = self.heads.iter().enumerate().filter_map(|(i, h)| h.as_ref().map(|e| (e.ts, i))).min();
        let Some((_, i)) = earliest else { return Ok(None) };
        let ev = self.heads[i].take();
        self.heads[i] = self.sources[i].next_event()?;
        Ok(ev)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_by_time_with_stable_ties() {
        let dir = std::env::temp_dir().join(format!("hft-merge-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let line = |ts: u64, conn: u64| format!("{{\"ts\":{ts},\"conn\":{conn},\"kind\":\"timer\",\"data\":null}}\n");
        std::fs::write(dir.join("a.log"), line(1, 1) + &line(5, 1) + &line(7, 1)).unwrap();
        std::fs::write(dir.join("b.log"), line(2, 2) + &line(5, 2)).unwrap();
        let mut r = MergedReader::open_dir(dir.to_str().unwrap()).unwrap();
        let mut got = Vec::new();
        while let Some(e) = r.next_event().unwrap() {
            got.push((e.ts, e.conn));
        }
        assert_eq!(got, vec![(1, 1), (2, 2), (5, 1), (5, 2), (7, 1)]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
