//! The state machine: everything the program knows, updated one event at a time.
//!
//! The golden rule: `update` may only use (a) the event it is given and
//! (b) its own fields. No clock, no network, no randomness. Then the same
//! sequence of events always produces the same state and the same output,
//! which is what makes replay (plan step 3) possible.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::event::{Event, Kind};

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

/// One price level: everyone's orders at a single price, added together.
#[derive(Debug, Clone, PartialEq)]
pub struct Level {
    // NOTE: f64 is fine for printing a mid price. Before we place orders we
    // will switch to exact integer prices, because 0.1 + 0.2 != 0.3 in
    // floating point, and exchanges reject prices that are off by a hair.
    pub px: f64,
    pub sz: f64,
    /// Number of separate orders making up this level.
    pub n: u64,
}

#[derive(Debug, Clone)]
pub struct Book {
    /// Exchange's timestamp for this snapshot, in milliseconds.
    pub exch_time: u64,
    /// Best (highest) bid first.
    pub bids: Vec<Level>,
    /// Best (lowest) ask first.
    pub asks: Vec<Level>,
}

/// Top of the book: just the best bid and best ask.
///
/// Two feeds tell us about it. `bbo` updates whenever the best prices change
/// (about every 100 ms for BTC). `l2Book` carries depth but arrives only
/// every ~12 s. Whichever has the newer exchange time wins.
#[derive(Debug, Clone, PartialEq)]
pub struct Top {
    /// Exchange's timestamp, in milliseconds.
    pub exch_time: u64,
    /// Either side can be missing if nobody is quoting it.
    pub bid: Option<Level>,
    pub ask: Option<Level>,
}

impl Top {
    /// Midpoint between the best bid and best ask, if both sides exist.
    pub fn mid(&self) -> Option<f64> {
        Some((self.bid.as_ref()?.px + self.ask.as_ref()?.px) / 2.0)
    }
}

#[derive(Default)]
pub struct StateMachine {
    /// BTreeMap rather than HashMap: a HashMap's iteration order is randomized
    /// per run, a classic way to accidentally break determinism.
    pub conns: BTreeMap<u64, ConnState>,
    /// Depth, from l2Book. Its top levels can be up to ~12 s stale.
    pub book: Option<Book>,
    /// Best bid/ask, from whichever feed is fresher.
    pub top: Option<Top>,
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
                // With the connection gone, our copy of the book stops
                // updating while the real one keeps moving. A stale book is
                // worse than no book, so throw it away.
                self.book = None;
                self.top = None;
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
            Err(e) => {
                eprintln!("{} bad json: {e}", self.now);
                return;
            }
        };
        match msg["channel"].as_str() {
            Some("l2Book") => match parse_book(&msg["data"]) {
                Some(book) => {
                    let top = Top {
                        exch_time: book.exch_time,
                        bid: book.bids.first().cloned(),
                        ask: book.asks.first().cloned(),
                    };
                    // Each l2Book message is a complete snapshot, so we simply
                    // replace the old book rather than applying changes.
                    self.book = Some(book);
                    self.set_top(top);
                }
                None => eprintln!("{} malformed l2Book message", self.now),
            },
            Some("bbo") => match parse_bbo(&msg["data"]) {
                Some(top) => self.set_top(top),
                None => eprintln!("{} malformed bbo message", self.now),
            },
            // trades, subscriptionResponse, ...: not used yet.
            _ => {}
        }
    }

    /// Accept a new top of book unless we already have a newer one, and print
    /// the mid whenever it changes.
    fn set_top(&mut self, top: Top) {
        if let Some(current) = &self.top {
            if top.exch_time < current.exch_time {
                return; // older than what we have: ignore it
            }
        }
        let old_mid = self.top.as_ref().and_then(Top::mid);
        let new_mid = top.mid();
        self.top = Some(top);
        if new_mid != old_mid {
            if let Some(mid) = new_mid {
                println!("{} {}", self.now, mid);
            }
        }
    }
}

/// Parse `{"coin":..,"time":..,"levels":[[bids...],[asks...]]}`.
/// Returns None if anything is missing or malformed.
fn parse_book(data: &Value) -> Option<Book> {
    let exch_time = data["time"].as_u64()?;
    let sides = data["levels"].as_array()?;
    let bids = parse_levels(sides.first()?)?;
    let asks = parse_levels(sides.get(1)?)?;
    Some(Book { exch_time, bids, asks })
}

/// Parse `{"coin":..,"time":..,"bbo":[bid or null, ask or null]}`.
fn parse_bbo(data: &Value) -> Option<Top> {
    let exch_time = data["time"].as_u64()?;
    let sides = data["bbo"].as_array()?;
    // `null` means nobody is quoting that side. That's a valid state, but
    // a level that's present and malformed is an error.
    let side = |v: &Value| if v.is_null() { Some(None) } else { parse_level(v).map(Some) };
    Some(Top { exch_time, bid: side(sides.first()?)?, ask: side(sides.get(1)?)? })
}

fn parse_levels(side: &Value) -> Option<Vec<Level>> {
    // A list of Options becomes one Option: None if any level failed.
    side.as_array()?.iter().map(parse_level).collect()
}

fn parse_level(l: &Value) -> Option<Level> {
    Some(Level {
        // Prices and sizes arrive as strings ("59093.0") so no precision
        // is lost in transit; we parse them ourselves.
        px: l["px"].as_str()?.parse().ok()?,
        sz: l["sz"].as_str()?.parse().ok()?,
        n: l["n"].as_u64()?,
    })
}
