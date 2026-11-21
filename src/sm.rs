//! The state machine: everything the program knows, updated one event at a time.
//!
//! The golden rule: `update` may only use (a) the event it is given and
//! (b) its own fields. No clock, no network, no randomness. Then the same
//! sequence of events always produces the same state and the same output,
//! which is what makes replay possible.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::book::{Book, Side};
use crate::event::{Event, Kind};
use crate::fixed::{self, parse_price, parse_qty, Price};

/// Where a connection is in its life. Mirrors the layers in ws.rs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnState {
    Connecting, // tcp_connect sent
    TcpOpen,    // TCP up
    TlsOpen,    // encrypted channel up
    Upgrading,  // HTTP upgrade request sent
    Open,       // websocket ready, messages flowing
    Closed,
}

#[derive(Default)]
pub struct StateMachine {
    /// BTreeMap rather than HashMap everywhere: a HashMap's iteration order
    /// is randomized per run, a classic way to break determinism.
    pub conns: BTreeMap<u64, ConnState>,
    /// Order books by market ticker. A market is only here while we trust
    /// its book: from its snapshot until a gap or disconnect.
    pub books: BTreeMap<String, Book>,
    /// Last sequence number seen per subscription id ("sid").
    seqs: BTreeMap<u64, u64>,
    /// Last printed (bid, ask) per market, to print only on change.
    last_top: BTreeMap<String, (Option<Price>, Option<Price>)>,
    /// Timestamp of the latest event seen. This is the state machine's "now".
    pub now: u64,
}

impl StateMachine {
    pub fn new() -> StateMachine {
        StateMachine::default()
    }

    pub fn update(&mut self, ev: &Event) {
        self.now = ev.ts;
        match &ev.kind {
            Kind::TcpConnect => self.set_conn(ev.conn, ConnState::Connecting),
            Kind::TcpOpen => self.set_conn(ev.conn, ConnState::TcpOpen),
            Kind::TlsOpen => self.set_conn(ev.conn, ConnState::TlsOpen),
            Kind::HttpRequest(_) => self.set_conn(ev.conn, ConnState::Upgrading),
            Kind::HttpResponse(head) => {
                let ok = head.starts_with("HTTP/1.1 101");
                self.set_conn(ev.conn, if ok { ConnState::Open } else { ConnState::Closed });
            }
            Kind::WsClose | Kind::TcpClose(_) => {
                self.set_conn(ev.conn, ConnState::Closed);
                // Subscriptions don't survive a reconnect, and with the feed
                // gone our books stop updating while the real ones keep
                // moving. A stale book is worse than no book.
                self.books.clear();
                self.seqs.clear();
                self.print_changes();
            }
            Kind::WsIn(text) => self.on_message(text),
            Kind::Start | Kind::WsOut(_) | Kind::Other(_) => {}
        }
    }

    fn set_conn(&mut self, conn: u64, state: ConnState) {
        self.conns.insert(conn, state);
    }

    fn on_message(&mut self, text: &str) {
        let msg: Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(e) => return self.warn(&format!("bad json: {e}")),
        };

        // Sequence check. Messages on a subscription are numbered 1, 2, 3...
        // If we see 5 after 3, message 4 was lost and anything built from
        // that subscription can no longer be trusted.
        if let (Some(sid), Some(seq)) = (msg["sid"].as_u64(), msg["seq"].as_u64()) {
            let expected = self.seqs.get(&sid).map(|last| last + 1);
            self.seqs.insert(sid, seq);
            if expected.is_some_and(|e| e != seq) {
                self.warn(&format!("sequence gap on sid {sid}: expected {expected:?}, got {seq}"));
                // We don't track which markets belong to which sid yet, so
                // distrust every book. Each market comes back with its next
                // snapshot. TODO(step 4): ask the network thread to request
                // fresh snapshots (update_subscription / get_snapshot).
                self.books.clear();
                self.print_changes();
                return;
            }
        }

        match msg["type"].as_str() {
            Some("orderbook_snapshot") => self.on_snapshot(&msg["msg"]),
            Some("orderbook_delta") => self.on_delta(&msg["msg"]),
            Some("error") => self.warn(&format!("exchange error: {}", msg["msg"])),
            // subscribed, ok, trade, ticker...: not used yet. Kalshi may add
            // new message types at any time; ignoring unknown ones is
            // intentional (their docs ask for it).
            _ => {}
        }
    }

    fn on_snapshot(&mut self, m: &Value) {
        let Some(ticker) = m["market_ticker"].as_str() else {
            return self.warn("snapshot without market_ticker");
        };
        // A side with no orders is left out of the message entirely.
        let side = |key: &str| match m.get(key) {
            None => Some(Vec::new()),
            Some(v) => parse_levels(v),
        };
        match (side("yes_dollars_fp"), side("no_dollars_fp")) {
            (Some(yes), Some(no)) => {
                self.books.insert(ticker.to_string(), Book::from_snapshot(yes, no));
                self.print_changes();
            }
            _ => self.warn(&format!("malformed snapshot for {ticker}")),
        }
    }

    fn on_delta(&mut self, m: &Value) {
        let parsed = (|| {
            let ticker = m["market_ticker"].as_str()?;
            let price = parse_price(m["price_dollars"].as_str()?)?;
            let delta = parse_qty(m["delta_fp"].as_str()?)?;
            let side = match m["side"].as_str()? {
                "yes" => Side::Yes,
                "no" => Side::No,
                _ => return None,
            };
            Some((ticker, side, price, delta))
        })();
        let Some((ticker, side, price, delta)) = parsed else {
            return self.warn(&format!("malformed delta: {m}"));
        };
        // No book (we never got its snapshot, or threw it away): ignore.
        let Some(book) = self.books.get_mut(ticker) else { return };
        if !book.apply_delta(side, price, delta) {
            self.warn(&format!("{ticker}: delta made a level negative; dropping book"));
            self.books.remove(ticker);
        }
        self.print_changes();
    }

    /// Print "<ts> <ticker> <bid> <ask> <mid>" for every market whose best
    /// prices changed since we last printed it ("-" = no price).
    fn print_changes(&mut self) {
        // Markets that lost their book need a line too, so check both sets.
        let mut tickers: Vec<String> = self.books.keys().cloned().collect();
        tickers.extend(self.last_top.keys().filter(|t| !self.books.contains_key(*t)).cloned());
        tickers.sort();
        for ticker in tickers {
            let book = self.books.get(&ticker);
            let bid = book.and_then(Book::best_bid).map(|(p, _)| p);
            let ask = book.and_then(Book::best_ask).map(|(p, _)| p);
            if self.last_top.get(&ticker) == Some(&(bid, ask)) {
                continue;
            }
            self.last_top.insert(ticker.clone(), (bid, ask));
            let show = |p: Option<Price>| p.map(fixed::format_price).unwrap_or("-".into());
            // The mid can fall between two price units ($0.565), so compute
            // it with one more decimal place to keep it exact.
            let mid = match (bid, ask) {
                (Some(b), Some(a)) => fixed::format((b + a) * 5, fixed::PRICE_DECIMALS + 1),
                _ => "-".into(),
            };
            println!("{} {ticker} {} {} {mid}", self.now, show(bid), show(ask));
        }
    }

    fn warn(&self, what: &str) {
        eprintln!("{} WARN {what}", self.now);
    }
}

/// Parse `[["0.0800", "300.00"], ...]` into (price, quantity) pairs.
fn parse_levels(v: &Value) -> Option<Vec<(Price, i64)>> {
    v.as_array()?
        .iter()
        .map(|level| Some((parse_price(level[0].as_str()?)?, parse_qty(level[1].as_str()?)?)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws_in(ts: u64, json: &str) -> Event {
        Event { ts, conn: 1, kind: Kind::WsIn(json.to_string()) }
    }

    const SNAP: &str = r#"{"type":"orderbook_snapshot","sid":1,"seq":1,"msg":{"market_ticker":"T","yes_dollars_fp":[["0.5500","10.00"],["0.5600","3.00"]],"no_dollars_fp":[["0.4300","2.00"]]}}"#;

    #[test]
    fn snapshot_then_delta() {
        let mut sm = StateMachine::new();
        sm.update(&ws_in(1, SNAP));
        let b = &sm.books["T"];
        assert_eq!(b.best_bid(), Some((5600, 300)));
        assert_eq!(b.best_ask(), Some((5700, 200)));

        sm.update(&ws_in(2, r#"{"type":"orderbook_delta","sid":1,"seq":2,"msg":{"market_ticker":"T","price_dollars":"0.5600","delta_fp":"-3.00","side":"yes"}}"#));
        assert_eq!(sm.books["T"].best_bid(), Some((5500, 1000)));
    }

    #[test]
    fn gap_drops_books_until_next_snapshot() {
        let mut sm = StateMachine::new();
        sm.update(&ws_in(1, SNAP));
        // seq jumps from 1 to 3: message 2 was lost.
        sm.update(&ws_in(2, r#"{"type":"orderbook_delta","sid":1,"seq":3,"msg":{"market_ticker":"T","price_dollars":"0.5600","delta_fp":"-3.00","side":"yes"}}"#));
        assert!(sm.books.is_empty());
        // A fresh snapshot (here with the next seq) restores it.
        sm.update(&ws_in(3, &SNAP.replace(r#""seq":1"#, r#""seq":4"#)));
        assert!(sm.books.contains_key("T"));
    }

    #[test]
    fn disconnect_drops_everything() {
        let mut sm = StateMachine::new();
        sm.update(&ws_in(1, SNAP));
        sm.update(&Event { ts: 2, conn: 1, kind: Kind::TcpClose("bye".into()) });
        assert!(sm.books.is_empty());
        assert_eq!(sm.conns[&1], ConnState::Closed);
    }
}
