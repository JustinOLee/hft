//! The state machine: everything the program knows, updated one event at a time.
//!
//! The golden rule: `update` may only use (a) the event it is given and
//! (b) its own fields. No clock, no network, no randomness. Then the same
//! sequence of events always produces the same state, output and actions,
//! which is what makes replay possible.
//!
//! `update` returns the actions it wants taken. It never carries them out
//! itself. It finds out what happened to them the same way it finds out
//! anything: from later events in the log.

use std::collections::{BTreeMap, VecDeque};

use serde_json::Value;

use crate::action::Action;
use crate::book::{Book, Side};
use crate::event::{Event, Kind};
use crate::fixed::{self, parse_price, parse_qty, Price};

/// Connection numbers. The network thread uses the same ones.
pub const WS_CONN: u64 = 1;
pub const REST_CONN: u64 = 2;

/// Where a connection is in its life. Mirrors the layers in net.rs / ws.rs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnState {
    Connecting, // tcp_connect sent
    TcpOpen,    // TCP up
    TlsOpen,    // encrypted channel up (REST connections stop here)
    Upgrading,  // websocket upgrade request sent
    Open,       // websocket ready, messages flowing
    Closed,
}

/// What we believe about one of our orders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderStatus {
    /// We asked for it; Kalshi hasn't answered yet.
    Pending,
    /// Kalshi accepted the request.
    Accepted,
    /// In the book, waiting for someone to trade with it.
    Resting,
    /// We asked to cancel it; Kalshi hasn't answered yet.
    CancelPending,
    Canceled,
    /// Completely filled.
    Executed,
    /// Kalshi said no (or the request failed and we can't be sure).
    Rejected,
}

#[derive(Debug, Clone)]
pub struct Order {
    pub ticker: String,
    /// Kalshi's ID for the order, once we learn it. Needed to cancel.
    pub order_id: Option<String>,
    pub status: OrderStatus,
}

/// A REST request we've sent and not yet heard back about. Requests on one
/// keep-alive connection are answered strictly in order, so a queue lets us
/// match each response to its request.
#[derive(Debug, Clone)]
enum InFlight {
    Place(String),  // client_order_id
    Cancel(String), // client_order_id
}

/// Session settings, read from the log's `start` event so that replay uses
/// exactly the settings the live run used.
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub tickers: Vec<String>,
    /// Testing aid: request one resync right after the first snapshot, to
    /// exercise the whole action path without placing an order.
    pub test_resync: bool,
    pub strategy: Strategy,
}

/// Which trading logic to run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Strategy {
    /// Just watch.
    #[default]
    None,
    /// Plan step 5: place one tiny order far from the market, wait until
    /// it's resting, cancel it, done.
    PlaceCancel,
}

/// Progress of the place-then-cancel routine. This records what WE HAVE
/// ASKED FOR, which runs ahead of what Kalshi has confirmed. Without it,
/// every event that arrives between "decide to cancel" and "our cancel
/// shows up in the log" would trigger another cancel.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum PlaceCancel {
    #[default]
    Idle,
    Placed(String),    // client_order_id
    Canceling(String), // client_order_id
    Done,
}

/// The test order: buy 1 YES contract at $0.01. On any market trading well
/// above 1 cent it will sit far below the best bid and never fill, and even
/// if it did, it would cost one cent.
const TEST_PRICE: Price = 100;
const TEST_COUNT: i64 = 100; // 1.00 contract

#[derive(Default)]
pub struct StateMachine {
    pub config: Config,
    /// BTreeMap rather than HashMap everywhere: a HashMap's iteration order
    /// is randomized per run, a classic way to break determinism.
    pub conns: BTreeMap<u64, ConnState>,
    /// Order books by market ticker. A market is only here while we trust
    /// its book: from its snapshot until a gap or disconnect.
    pub books: BTreeMap<String, Book>,
    /// Our orders, by our own client_order_id.
    pub orders: BTreeMap<String, Order>,
    in_flight: VecDeque<InFlight>,
    /// Status code of the latest HTTP response per connection.
    last_http_status: BTreeMap<u64, u16>,
    /// Last sequence number seen per subscription id ("sid").
    seqs: BTreeMap<u64, u64>,
    /// Which channel each subscription id belongs to.
    sid_channels: BTreeMap<u64, String>,
    /// Last printed (bid, ask) per market, to print only on change.
    last_top: BTreeMap<String, (Option<Price>, Option<Price>)>,
    did_test_resync: bool,
    place_cancel: PlaceCancel,
    /// Replay-only testing aid: run this strategy regardless of what the
    /// log's start event says. Lets you try a new strategy on an old log.
    strategy_override: Option<Strategy>,
    /// Session number (start time in seconds), used to make our
    /// client_order_ids unique across runs.
    session: u64,
    next_order: u64,
    /// Actions produced during the current `update` call.
    out: Vec<Action>,
    /// Timestamp of the latest event seen. This is the state machine's "now".
    pub now: u64,
}

impl StateMachine {
    pub fn new() -> StateMachine {
        StateMachine::default()
    }

    pub fn with_strategy_override(strategy: Strategy) -> StateMachine {
        StateMachine { strategy_override: Some(strategy), ..StateMachine::default() }
    }

    /// Process one event. Returns the actions to take (usually none).
    pub fn update(&mut self, ev: &Event) -> Vec<Action> {
        self.now = ev.ts;
        match &ev.kind {
            Kind::Start(data) => self.on_start(ev.ts, data),
            Kind::TcpConnect => self.set_conn(ev.conn, ConnState::Connecting),
            Kind::TcpOpen => self.set_conn(ev.conn, ConnState::TcpOpen),
            Kind::TlsOpen => self.set_conn(ev.conn, ConnState::TlsOpen),
            Kind::HttpRequest(_) => {
                if ev.conn == WS_CONN {
                    self.set_conn(ev.conn, ConnState::Upgrading);
                }
            }
            Kind::HttpResponse(head) => self.on_http_response(ev.conn, head),
            Kind::HttpResponseBody(body) => {
                if ev.conn == REST_CONN {
                    self.on_rest_result(Ok(body));
                }
            }
            Kind::HttpError(e) => {
                if ev.conn == REST_CONN {
                    self.on_rest_result(Err(e));
                }
            }
            Kind::WsClose | Kind::TcpClose(_) => self.on_close(ev.conn),
            Kind::WsIn(text) => self.on_message(text),
            Kind::Action(action) => self.on_own_action(action),
            Kind::WsOut(_) | Kind::Other(_) => {}
        }
        self.run_strategy();
        std::mem::take(&mut self.out)
    }

    /// True once the strategy has nothing left to do. Live and replay both
    /// stop at this point, so they process exactly the same events.
    pub fn finished(&self) -> bool {
        self.config.strategy == Strategy::PlaceCancel && self.place_cancel == PlaceCancel::Done
    }

    fn on_start(&mut self, ts: u64, data: &Value) {
        self.session = ts / 1_000_000_000;
        self.config.strategy = match data["strategy"].as_str() {
            Some("place_cancel") => Strategy::PlaceCancel,
            _ => Strategy::None,
        };
        if let Some(s) = self.strategy_override {
            self.config.strategy = s;
        }
        self.config.tickers = data["tickers"]
            .as_array()
            .map(|a| a.iter().filter_map(|t| t.as_str().map(String::from)).collect())
            .unwrap_or_default();
        self.config.test_resync = data["test_resync"].as_bool().unwrap_or(false);
    }

    fn set_conn(&mut self, conn: u64, state: ConnState) {
        self.conns.insert(conn, state);
    }

    fn on_http_response(&mut self, conn: u64, head: &str) {
        let status = head.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        self.last_http_status.insert(conn, status);
        if self.conns.get(&conn) == Some(&ConnState::Upgrading) {
            let state = if status == 101 { ConnState::Open } else { ConnState::Closed };
            self.set_conn(conn, state);
        }
    }

    fn on_close(&mut self, conn: u64) {
        self.set_conn(conn, ConnState::Closed);
        if conn == WS_CONN {
            // Subscriptions don't survive a reconnect, and with the feed
            // gone our books stop updating while the real ones keep moving.
            // A stale book is worse than no book.
            self.books.clear();
            self.seqs.clear();
            self.sid_channels.clear();
            self.print_changes();
        }
    }

    /// One of our own actions, read back from the log. This is how we know
    /// what we've asked for, including requests Kalshi hasn't answered yet.
    fn on_own_action(&mut self, action: &Action) {
        match action {
            Action::Place { client_order_id, ticker, .. } => {
                let order = Order { ticker: ticker.clone(), order_id: None, status: OrderStatus::Pending };
                self.orders.insert(client_order_id.clone(), order);
                self.print_order(client_order_id);
                self.in_flight.push_back(InFlight::Place(client_order_id.clone()));
            }
            Action::Cancel { order_id, .. } => {
                let cid = self.cid_for_order_id(order_id).unwrap_or_else(|| format!("?{order_id}"));
                self.set_status(&cid, OrderStatus::CancelPending);
                self.in_flight.push_back(InFlight::Cancel(cid));
            }
            Action::Resync { .. } => {}
        }
    }

    /// The answer to the oldest REST request still waiting for one.
    fn on_rest_result(&mut self, result: Result<&str, &str>) {
        let Some(req) = self.in_flight.pop_front() else {
            return self.warn("REST response with no request waiting for it");
        };
        let status = self.last_http_status.get(&REST_CONN).copied().unwrap_or(0);
        let ok = result.is_ok() && (200..300).contains(&status);
        let body: Value = result.ok().and_then(|b| serde_json::from_str(b).ok()).unwrap_or(Value::Null);
        match req {
            InFlight::Place(cid) => {
                if let Some(id) = body["order_id"].as_str() {
                    if let Some(order) = self.orders.get_mut(&cid) {
                        order.order_id = Some(id.to_string());
                    }
                }
                if !ok {
                    self.warn(&format!("order {cid} rejected: HTTP {status} {body} {result:?}"));
                }
                if !ok {
                    self.set_status(&cid, OrderStatus::Rejected);
                } else if self.orders.get(&cid).map(|o| o.status) == Some(OrderStatus::Pending) {
                    // Only move forward: the private feed may already have
                    // told us it's resting (it travels on another connection).
                    self.set_status(&cid, OrderStatus::Accepted);
                }
            }
            InFlight::Cancel(cid) => {
                if ok {
                    if self.orders.get(&cid).map(|o| o.status) == Some(OrderStatus::CancelPending) {
                        self.set_status(&cid, OrderStatus::Canceled);
                    }
                } else {
                    // The order may still be live. Don't guess: the
                    // user_orders feed will tell us its real status.
                    self.warn(&format!("cancel of {cid} failed: HTTP {status} {body} {result:?}"));
                }
            }
        }
    }

    fn cid_for_order_id(&self, order_id: &str) -> Option<String> {
        self.orders.iter().find(|(_, o)| o.order_id.as_deref() == Some(order_id)).map(|(cid, _)| cid.clone())
    }

    fn set_status(&mut self, cid: &str, status: OrderStatus) {
        match self.orders.get_mut(cid) {
            Some(order) if order.status != status => {
                order.status = status;
                self.print_order(cid);
            }
            _ => {}
        }
    }

    fn print_order(&self, cid: &str) {
        if let Some(o) = self.orders.get(cid) {
            let id = o.order_id.as_deref().unwrap_or("-");
            println!("{} ORDER {cid} {} {:?} {id}", self.now, o.ticker, o.status);
        }
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
                if self.sid_channels.get(&sid).map(String::as_str) == Some("orderbook_delta") {
                    self.books.clear();
                    self.print_changes();
                    self.request_resync(sid);
                }
                return;
            }
        }

        match msg["type"].as_str() {
            Some("subscribed") => {
                if let (Some(channel), Some(sid)) = (msg["msg"]["channel"].as_str(), msg["msg"]["sid"].as_u64()) {
                    self.sid_channels.insert(sid, channel.to_string());
                }
            }
            Some("orderbook_snapshot") => self.on_snapshot(&msg),
            Some("orderbook_delta") => self.on_delta(&msg["msg"]),
            Some("user_order") => self.on_user_order(&msg["msg"]),
            Some("fill") => {
                let m = &msg["msg"];
                println!(
                    "{} FILL {} {} {} @ {} x {}",
                    self.now,
                    m["client_order_id"].as_str().unwrap_or("-"),
                    m["market_ticker"].as_str().unwrap_or("?"),
                    m["book_side"].as_str().unwrap_or("?"),
                    m["yes_price_dollars"].as_str().unwrap_or("?"),
                    m["count_fp"].as_str().unwrap_or("?"),
                );
            }
            Some("error") => self.warn(&format!("exchange error: {}", msg["msg"])),
            // ok, trade, ticker...: not used yet. Kalshi may add new message
            // types at any time; ignoring unknown ones is intentional (their
            // docs ask for it).
            _ => {}
        }
    }

    /// Ask for fresh snapshots of every market we follow.
    fn request_resync(&mut self, sid: u64) {
        self.out.push(Action::Resync { sid, tickers: self.config.tickers.clone() });
    }

    /// Kalshi's own report of an order's status: the authority.
    fn on_user_order(&mut self, m: &Value) {
        let order_id = m["order_id"].as_str().unwrap_or("");
        let cid = m["client_order_id"].as_str().map(String::from).or_else(|| self.cid_for_order_id(order_id));
        let Some(cid) = cid.filter(|c| self.orders.contains_key(c)) else {
            // Not one of ours from this session (placed on the website,
            // or by an earlier run). Note it and move on.
            return self.warn(&format!("update for unknown order {order_id}"));
        };
        if let Some(order) = self.orders.get_mut(&cid) {
            order.order_id.get_or_insert_with(|| order_id.to_string());
        }
        let status = match m["status"].as_str() {
            Some("resting") => OrderStatus::Resting,
            Some("canceled") => OrderStatus::Canceled,
            Some("executed") => OrderStatus::Executed,
            other => return self.warn(&format!("order {cid}: unknown status {other:?}")),
        };
        self.set_status(&cid, status);
    }

    fn on_snapshot(&mut self, msg: &Value) {
        let m = &msg["msg"];
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
            _ => return self.warn(&format!("malformed snapshot for {ticker}")),
        }
        if self.config.test_resync && !self.did_test_resync {
            self.did_test_resync = true;
            if let Some(sid) = msg["sid"].as_u64() {
                self.request_resync(sid);
            }
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

    // ---- strategy --------------------------------------------------------

    fn run_strategy(&mut self) {
        if self.config.strategy == Strategy::PlaceCancel {
            self.run_place_cancel();
        }
    }

    fn new_client_order_id(&mut self) -> String {
        self.next_order += 1;
        format!("hft{}-{}", self.session, self.next_order)
    }

    fn run_place_cancel(&mut self) {
        let Some(ticker) = self.config.tickers.first().cloned() else { return };
        match self.place_cancel.clone() {
            PlaceCancel::Idle => {
                // Wait until we can see the market and Kalshi can see us.
                let ws_open = self.conns.get(&WS_CONN) == Some(&ConnState::Open);
                let Some(book) = self.books.get(&ticker) else { return };
                if !ws_open {
                    return;
                }
                // Safety check: only proceed if our 1-cent bid would really
                // be far below the market.
                if book.best_bid().is_none_or(|(bid, _)| bid < TEST_PRICE * 5) {
                    self.warn("place_cancel: market's best bid is too low for a safe test; giving up");
                    self.place_cancel = PlaceCancel::Done;
                    return;
                }
                let cid = self.new_client_order_id();
                self.out.push(Action::Place {
                    client_order_id: cid.clone(),
                    ticker,
                    side: crate::action::OrderSide::Bid,
                    price: TEST_PRICE,
                    count: TEST_COUNT,
                    post_only: true,
                });
                self.place_cancel = PlaceCancel::Placed(cid);
            }
            PlaceCancel::Placed(cid) => {
                let Some(order) = self.orders.get(&cid) else { return }; // not back from the log yet
                match order.status {
                    OrderStatus::Resting => {
                        let Some(order_id) = order.order_id.clone() else { return };
                        self.out.push(Action::Cancel { ticker, order_id });
                        self.place_cancel = PlaceCancel::Canceling(cid);
                    }
                    OrderStatus::Rejected | OrderStatus::Canceled | OrderStatus::Executed => {
                        println!("{} DONE place_cancel: order ended as {:?} before we canceled", self.now, order.status);
                        self.place_cancel = PlaceCancel::Done;
                    }
                    _ => {} // Pending / Accepted: keep waiting
                }
            }
            PlaceCancel::Canceling(cid) => match self.orders.get(&cid).map(|o| o.status) {
                Some(OrderStatus::Canceled) => {
                    println!("{} DONE place_cancel: placed and canceled {cid}", self.now);
                    self.place_cancel = PlaceCancel::Done;
                }
                Some(OrderStatus::Executed) => {
                    println!("{} DONE place_cancel: {cid} FILLED before the cancel landed", self.now);
                    self.place_cancel = PlaceCancel::Done;
                }
                _ => {}
            },
            PlaceCancel::Done => {}
        }
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
    use crate::action::OrderSide;

    fn ev(ts: u64, conn: u64, kind: Kind) -> Event {
        Event { ts, conn, kind }
    }

    fn ws_in(ts: u64, json: &str) -> Event {
        ev(ts, WS_CONN, Kind::WsIn(json.to_string()))
    }

    fn started(tickers: &[&str]) -> StateMachine {
        let mut sm = StateMachine::new();
        let data = serde_json::json!({ "tickers": tickers });
        sm.update(&ev(0, 0, Kind::Start(data)));
        sm.update(&ws_in(0, r#"{"type":"subscribed","id":1,"msg":{"channel":"orderbook_delta","sid":1}}"#));
        sm
    }

    const SNAP: &str = r#"{"type":"orderbook_snapshot","sid":1,"seq":1,"msg":{"market_ticker":"T","yes_dollars_fp":[["0.5500","10.00"],["0.5600","3.00"]],"no_dollars_fp":[["0.4300","2.00"]]}}"#;

    #[test]
    fn snapshot_then_delta() {
        let mut sm = started(&["T"]);
        assert!(sm.update(&ws_in(1, SNAP)).is_empty());
        let b = &sm.books["T"];
        assert_eq!(b.best_bid(), Some((5600, 300)));
        assert_eq!(b.best_ask(), Some((5700, 200)));

        sm.update(&ws_in(2, r#"{"type":"orderbook_delta","sid":1,"seq":2,"msg":{"market_ticker":"T","price_dollars":"0.5600","delta_fp":"-3.00","side":"yes"}}"#));
        assert_eq!(sm.books["T"].best_bid(), Some((5500, 1000)));
    }

    #[test]
    fn gap_drops_books_and_asks_for_resync() {
        let mut sm = started(&["T"]);
        sm.update(&ws_in(1, SNAP));
        // seq jumps from 1 to 3: message 2 was lost.
        let actions = sm.update(&ws_in(2, r#"{"type":"orderbook_delta","sid":1,"seq":3,"msg":{"market_ticker":"T","price_dollars":"0.5600","delta_fp":"-3.00","side":"yes"}}"#));
        assert!(sm.books.is_empty());
        assert_eq!(actions, vec![Action::Resync { sid: 1, tickers: vec!["T".into()] }]);
        // The fresh snapshot (next seq) restores the book.
        sm.update(&ws_in(3, &SNAP.replace(r#""seq":1"#, r#""seq":4"#)));
        assert!(sm.books.contains_key("T"));
    }

    #[test]
    fn test_resync_fires_once() {
        let mut sm = StateMachine::new();
        sm.update(&ev(0, 0, Kind::Start(serde_json::json!({ "tickers": ["T"], "test_resync": true }))));
        assert_eq!(sm.update(&ws_in(1, SNAP)).len(), 1);
        assert!(sm.update(&ws_in(2, &SNAP.replace(r#""seq":1"#, r#""seq":2"#))).is_empty());
    }

    #[test]
    fn disconnect_drops_everything() {
        let mut sm = started(&["T"]);
        sm.update(&ws_in(1, SNAP));
        sm.update(&ev(2, WS_CONN, Kind::TcpClose("bye".into())));
        assert!(sm.books.is_empty());
        assert_eq!(sm.conns[&WS_CONN], ConnState::Closed);
    }

    #[test]
    fn rest_close_keeps_books() {
        let mut sm = started(&["T"]);
        sm.update(&ws_in(1, SNAP));
        sm.update(&ev(2, REST_CONN, Kind::TcpClose("idle".into())));
        assert!(sm.books.contains_key("T"));
    }

    #[test]
    fn order_lifecycle() {
        let mut sm = started(&["T"]);
        let place = Action::Place {
            client_order_id: "c1".into(),
            ticker: "T".into(),
            side: OrderSide::Bid,
            price: 100,
            count: 100,
            post_only: true,
        };
        // Our own action comes back from the log: now we know it's pending.
        sm.update(&ev(1, 0, Kind::Action(place)));
        assert_eq!(sm.orders["c1"].status, OrderStatus::Pending);

        // Kalshi's REST answer gives us its order ID.
        sm.update(&ev(2, REST_CONN, Kind::HttpResponse("HTTP/1.1 201 Created\r\n\r\n".into())));
        sm.update(&ev(3, REST_CONN, Kind::HttpResponseBody(r#"{"order_id":"o9","client_order_id":"c1"}"#.into())));
        assert_eq!(sm.orders["c1"].status, OrderStatus::Accepted);
        assert_eq!(sm.orders["c1"].order_id.as_deref(), Some("o9"));

        // The private feed confirms it's in the book.
        sm.update(&ws_in(4, r#"{"type":"user_order","sid":5,"msg":{"order_id":"o9","client_order_id":"c1","status":"resting"}}"#));
        assert_eq!(sm.orders["c1"].status, OrderStatus::Resting);

        // Cancel: pending until Kalshi answers.
        sm.update(&ev(5, 0, Kind::Action(Action::Cancel { ticker: "T".into(), order_id: "o9".into() })));
        assert_eq!(sm.orders["c1"].status, OrderStatus::CancelPending);
        sm.update(&ev(6, REST_CONN, Kind::HttpResponse("HTTP/1.1 200 OK\r\n\r\n".into())));
        sm.update(&ev(7, REST_CONN, Kind::HttpResponseBody(r#"{"order_id":"o9"}"#.into())));
        assert_eq!(sm.orders["c1"].status, OrderStatus::Canceled);
    }

    #[test]
    fn place_cancel_strategy() {
        let mut sm = StateMachine::new();
        let start = serde_json::json!({ "tickers": ["T"], "strategy": "place_cancel" });
        sm.update(&ev(1_000_000_000, 0, Kind::Start(start)));
        sm.update(&ev(1, WS_CONN, Kind::HttpRequest(String::new())));
        sm.update(&ev(2, WS_CONN, Kind::HttpResponse("HTTP/1.1 101 Switching Protocols\r\n\r\n".into())));

        // Book arrives: place exactly once, even as more events come in.
        let actions = sm.update(&ws_in(3, SNAP));
        assert_eq!(actions.len(), 1);
        let Action::Place { client_order_id: cid, price, post_only, .. } = &actions[0] else { panic!() };
        assert_eq!((cid.as_str(), *price, *post_only), ("hft1-1", TEST_PRICE, true));
        assert!(sm.update(&ws_in(4, &SNAP.replace(r#""seq":1"#, r#""seq":2"#))).is_empty());

        // Our action comes back, Kalshi accepts and confirms it's resting.
        sm.update(&ev(5, 0, Kind::Action(actions[0].clone())));
        sm.update(&ev(6, REST_CONN, Kind::HttpResponse("HTTP/1.1 201 Created\r\n\r\n".into())));
        assert!(sm.update(&ev(7, REST_CONN, Kind::HttpResponseBody(r#"{"order_id":"o9"}"#.into()))).is_empty());
        let actions = sm.update(&ws_in(8, r#"{"type":"user_order","sid":5,"msg":{"order_id":"o9","client_order_id":"hft1-1","status":"resting"}}"#));
        assert_eq!(actions, vec![Action::Cancel { ticker: "T".into(), order_id: "o9".into() }]);

        // More events before the cancel shows up in the log: no repeat.
        assert!(sm.update(&ws_in(9, &SNAP.replace(r#""seq":1"#, r#""seq":3"#))).is_empty());

        sm.update(&ev(10, 0, Kind::Action(actions[0].clone())));
        sm.update(&ev(11, REST_CONN, Kind::HttpResponse("HTTP/1.1 200 OK\r\n\r\n".into())));
        sm.update(&ev(12, REST_CONN, Kind::HttpResponseBody(r#"{"order_id":"o9"}"#.into())));
        assert!(sm.finished());
    }

    #[test]
    fn status_never_moves_backwards() {
        let mut sm = started(&["T"]);
        let place = Action::Place {
            client_order_id: "c1".into(),
            ticker: "T".into(),
            side: OrderSide::Bid,
            price: 100,
            count: 100,
            post_only: true,
        };
        sm.update(&ev(1, 0, Kind::Action(place)));
        // The private feed beats the REST response.
        sm.update(&ws_in(2, r#"{"type":"user_order","sid":5,"msg":{"order_id":"o9","client_order_id":"c1","status":"resting"}}"#));
        sm.update(&ev(3, REST_CONN, Kind::HttpResponse("HTTP/1.1 201 Created\r\n\r\n".into())));
        sm.update(&ev(4, REST_CONN, Kind::HttpResponseBody(r#"{"order_id":"o9"}"#.into())));
        assert_eq!(sm.orders["c1"].status, OrderStatus::Resting);
    }

    #[test]
    fn rejected_order() {
        let mut sm = started(&["T"]);
        let place = Action::Place {
            client_order_id: "c1".into(),
            ticker: "T".into(),
            side: OrderSide::Bid,
            price: 100,
            count: 100,
            post_only: true,
        };
        sm.update(&ev(1, 0, Kind::Action(place)));
        sm.update(&ev(2, REST_CONN, Kind::HttpError("connection reset".into())));
        assert_eq!(sm.orders["c1"].status, OrderStatus::Rejected);
    }
}
