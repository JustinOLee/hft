//! The lab's state machine: recorded events in, paper trades and a report out.
//!
//! Same rules as the trading state machine (sm.rs): it never reads the
//! clock, the network or random numbers. "Now" is the current event's
//! timestamp. So the same session + the same config gives the same result,
//! every time, and two configs can be compared fairly on identical data.
//!
//! For each event:
//!   1. fill any orders whose pretend delay (latency_ms) is over, using the
//!      book as it is right now, before this event changes it
//!   2. apply the event: spot price, order book, new market, or resolution
//!   3. once per `eval_ms`, ask the strategy what it wants

use std::collections::BTreeMap;

use serde_json::Value;

use crate::event::{Event, Kind};
use crate::lab::config::Config;
use crate::lab::feeds::{COINBASE_CONN, POLY_WS_CONN};
use crate::lab::model;
use crate::lab::paper::Paper;
use crate::lab::pmbook::PmBook;
use crate::lab::strategy::{self, Intent, Outcome, Quote, Strategy, View};
use crate::lab::vol::Vol;
use crate::Error;

const NS: u64 = 1_000_000_000;

/// Only trust a window's start price if we saw a spot tick within this many
/// seconds of the start. (If recording began mid-window, we didn't.)
const OPEN_TOLERANCE_S: u64 = 2;

#[derive(Default)]
struct Window {
    start: u64,
    end: u64,
    up_token: String,
    down_token: String,
    up: PmBook,
    down: PmBook,
    /// Coinbase mid at the first tick at/after start and end.
    open_spot: Option<f64>,
    close_spot: Option<f64>,
    /// The official result, from Polymarket.
    up_won: Option<bool>,
    /// (model P(up), market mid of Up), sampled at each evaluation, to score
    /// both forecasts once we know the answer.
    forecasts: Vec<(f64, Option<f64>)>,
    pnl: f64,
}

impl Window {
    /// The best guess of how it ended: official if known, else our proxy
    /// (Coinbase close >= open). The proxy uses a different price source,
    /// so it's occasionally wrong when the move is tiny.
    fn result(&self) -> Option<(bool, bool)> {
        if let Some(w) = self.up_won {
            return Some((w, true));
        }
        Some((self.close_spot? >= self.open_spot?, false))
    }

    fn book(&self, o: Outcome) -> &PmBook {
        match o {
            Outcome::Up => &self.up,
            Outcome::Down => &self.down,
        }
    }
}

struct Pending {
    due: u64,
    window: u64,
    intent: Intent,
}

pub struct Sim {
    cfg: Config,
    strat: Box<dyn Strategy>,
    now: u64,
    spot: Option<f64>,
    vol: Vol,
    windows: BTreeMap<u64, Window>,
    /// token id -> (window start, which outcome)
    tokens: BTreeMap<String, (u64, Outcome)>,
    paper: Paper,
    pending: Vec<Pending>,
    next_eval: u64,
}

impl Sim {
    pub fn new(cfg: Config) -> Result<Sim, Error> {
        Ok(Sim {
            strat: strategy::build(&cfg)?,
            vol: Vol::new(cfg.vol_prior, cfg.vol_halflife_s),
            paper: Paper::new(cfg.budget, cfg.fee_rate),
            cfg,
            now: 0,
            spot: None,
            windows: BTreeMap::new(),
            tokens: BTreeMap::new(),
            pending: Vec::new(),
            next_eval: 0,
        })
    }

    pub fn update(&mut self, ev: &Event) {
        self.now = ev.ts;
        self.fill_due_orders();
        match &ev.kind {
            Kind::WsIn(text) if ev.conn == COINBASE_CONN => self.on_coinbase(text),
            Kind::WsIn(text) if ev.conn == POLY_WS_CONN => self.on_polymarket(text),
            Kind::Other(name, data) if name == "market" => self.on_market(data),
            Kind::Other(name, data) if name == "resolution" => {
                if let (Some(s), Some(w)) = (data["start"].as_u64(), data["up_won"].as_bool()) {
                    self.windows.entry(s).or_default().up_won = Some(w);
                }
            }
            _ => {}
        }
        if self.now >= self.next_eval {
            self.evaluate();
            let step = self.cfg.eval_ms * 1_000_000;
            self.next_eval = (self.now / step + 1) * step;
        }
    }

    fn on_coinbase(&mut self, text: &str) {
        let Ok(v) = serde_json::from_str::<Value>(text) else { return };
        if v["type"] != "ticker" {
            return;
        }
        let num = |k: &str| v[k].as_str().and_then(|s| s.parse::<f64>().ok());
        let (Some(bid), Some(ask)) = (num("best_bid"), num("best_ask")) else { return };
        let mid = (bid + ask) / 2.0;
        self.spot = Some(mid);
        let sec = self.now / NS;
        self.vol.sample(sec, mid);
        for w in self.windows.values_mut() {
            if w.open_spot.is_none() && sec >= w.start && sec < w.start + OPEN_TOLERANCE_S {
                w.open_spot = Some(mid);
            }
            if w.close_spot.is_none() && w.end > 0 && sec >= w.end {
                w.close_spot = Some(mid);
            }
        }
    }

    fn on_polymarket(&mut self, text: &str) {
        let Ok(v) = serde_json::from_str::<Value>(text) else { return };
        // A message is one object or a list of them.
        let msgs = match v {
            Value::Array(a) => a,
            other => vec![other],
        };
        for m in msgs {
            match m["event_type"].as_str() {
                Some("book") => {
                    if let Some(book) = self.book_mut(m["asset_id"].as_str()) {
                        book.apply_snapshot(&m);
                    }
                }
                Some("price_change") => {
                    for c in m["price_changes"].as_array().into_iter().flatten() {
                        if let Some(book) = self.book_mut(c["asset_id"].as_str()) {
                            book.apply_change(c);
                        }
                    }
                }
                _ => {} // last_trade_price, tick_size_change: TODO use trades as a signal
            }
        }
    }

    fn book_mut(&mut self, token: Option<&str>) -> Option<&mut PmBook> {
        let &(start, outcome) = self.tokens.get(token?)?;
        let w = self.windows.get_mut(&start)?;
        Some(match outcome {
            Outcome::Up => &mut w.up,
            Outcome::Down => &mut w.down,
        })
    }

    fn on_market(&mut self, d: &Value) {
        let (Some(start), Some(end)) = (d["start"].as_u64(), d["end"].as_u64()) else { return };
        let up = d["up_token"].as_str().unwrap_or("").to_string();
        let down = d["down_token"].as_str().unwrap_or("").to_string();
        self.tokens.insert(up.clone(), (start, Outcome::Up));
        self.tokens.insert(down.clone(), (start, Outcome::Down));
        let w = self.windows.entry(start).or_default();
        (w.start, w.end, w.up_token, w.down_token) = (start, end, up, down);
    }

    /// The window in progress right now, if we know its start price.
    fn active_window(&self) -> Option<&Window> {
        let sec = self.now / NS;
        self.windows.values().find(|w| w.start <= sec && sec < w.end && w.open_spot.is_some())
    }

    fn evaluate(&mut self) {
        let Some(spot) = self.spot else { return };
        let Some(w) = self.active_window() else { return };
        let open = w.open_spot.expect("active windows have an open");
        let seconds_left = (w.end * NS).saturating_sub(self.now) as f64 / NS as f64;
        let sigma = self.vol.sigma();
        let p_up = model::prob_up(spot, open, sigma, seconds_left);
        let quote = |b: &PmBook| Quote { bid: b.best_bid(), ask: b.best_ask(), ask_size: b.best_ask_size() };
        let view = View {
            window_start: w.start,
            seconds_left,
            spot,
            open,
            sigma,
            p_up,
            up: quote(&w.up),
            down: quote(&w.down),
            held_up: self.paper.held(w.start, Outcome::Up),
            held_down: self.paper.held(w.start, Outcome::Down),
            cash: self.paper.cash,
        };
        let market = w.up.mid();
        let start = w.start;
        self.windows.get_mut(&start).expect("exists").forecasts.push((p_up, market));

        let due = self.now + self.cfg.latency_ms * 1_000_000;
        for intent in self.strat.decide(&view) {
            if self.cfg.verbose {
                println!(
                    "{} DECIDE {:?} {} @<= {:.3}  (p_up {:.3}, up {:?}/{:?}, {:.0}s left, spot-open {:+.1})",
                    self.now, intent.outcome, intent.shares, intent.limit, p_up, view.up.bid, view.up.ask,
                    seconds_left, spot - open
                );
            }
            self.pending.push(Pending { due, window: start, intent });
        }
    }

    fn fill_due_orders(&mut self) {
        // Pending orders are in the order they were made, and all share the
        // same delay, so the due ones are always at the front.
        let n = self.pending.iter().take_while(|p| p.due <= self.now).count();
        for p in self.pending.drain(..n).collect::<Vec<_>>() {
            let Some(w) = self.windows.get(&p.window) else { continue };
            if self.now >= w.end * NS {
                continue; // too late: the window closed while the order was "in flight"
            }
            let i = &p.intent;
            if let Some(f) = self.paper.buy(self.now, p.window, i.outcome, w.book(i.outcome), i.shares, i.limit) {
                if self.cfg.verbose {
                    println!("{} FILL   {:?} {} @ {:.3} fee {:.3}", self.now, f.outcome, f.shares, f.avg_price, f.fee);
                }
            }
        }
    }

    /// End of data: settle every window we can and score the forecasts.
    pub fn finish(mut self) -> Report {
        let mut r = Report { name: self.cfg.name.clone(), start_cash: self.cfg.budget, ..Report::default() };
        let (mut model_sq, mut market_sq) = (0.0, 0.0);
        for (&start, w) in self.windows.iter_mut() {
            let Some((up_won, official)) = w.result() else { continue };
            r.windows += 1;
            if official {
                r.official += 1;
                if let Some(proxy) = w.close_spot.zip(w.open_spot).map(|(c, o)| c >= o) {
                    r.proxy_checked += 1;
                    r.proxy_agreed += (proxy == up_won) as u32;
                }
            }
            if !self.paper.open_windows().contains(&start) && w.forecasts.is_empty() {
                continue;
            }
            w.pnl = self.paper.settle(start, up_won);
            let y = if up_won { 1.0 } else { 0.0 };
            for &(model, market) in &w.forecasts {
                if let Some(m) = market {
                    model_sq += (model - y) * (model - y);
                    market_sq += (m - y) * (m - y);
                    r.forecasts += 1;
                }
            }
        }
        r.unsettled = self.paper.open_windows().len() as u32;
        if r.forecasts > 0 {
            r.brier_model = model_sq / r.forecasts as f64;
            r.brier_market = market_sq / r.forecasts as f64;
        }
        r.fills = self.paper.fills.len() as u32;
        r.fees = self.paper.fees;
        r.traded_windows = {
            let mut w: Vec<u64> = self.paper.fills.iter().map(|f| f.window).collect();
            w.dedup();
            w.len() as u32
        };
        r.window_pnls = self.windows.values().filter(|w| w.pnl != 0.0).map(|w| (w.start, w.pnl)).collect();
        r.end_cash = self.paper.cash;
        r
    }
}

/// What a backtest found.
#[derive(Default, Debug)]
pub struct Report {
    pub name: String,
    pub start_cash: f64,
    /// Cash at the end. Shares in unsettled windows count as zero.
    pub end_cash: f64,
    pub fills: u32,
    pub fees: f64,
    pub traded_windows: u32,
    /// Windows we know the result of, and how many of those are official.
    pub windows: u32,
    pub official: u32,
    /// How often "Coinbase close >= open" matched the official result.
    pub proxy_checked: u32,
    pub proxy_agreed: u32,
    pub unsettled: u32,
    /// Brier score: the average of (forecast - outcome)^2. 0 is perfect,
    /// 0.25 is "always say 50%". Lower is better. Comparing the model with
    /// the market's own price shows whether the model knows anything the
    /// market doesn't.
    pub forecasts: u32,
    pub brier_model: f64,
    pub brier_market: f64,
    pub window_pnls: Vec<(u64, f64)>,
}

impl Report {
    pub fn header() -> String {
        format!(
            "{:<22} {:>9} {:>6} {:>7} {:>8} {:>7} {:>7} {:>7} {:>9}",
            "config", "pnl $", "fills", "fees $", "windows", "traded", "brier", "market", "forecasts"
        )
    }

    pub fn row(&self) -> String {
        format!(
            "{:<22} {:>+9.2} {:>6} {:>7.2} {:>8} {:>7} {:>7.4} {:>7.4} {:>9}",
            self.name,
            self.end_cash - self.start_cash,
            self.fills,
            self.fees,
            self.windows,
            self.traded_windows,
            self.brier_model,
            self.brier_market,
            self.forecasts
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(sec: f64, conn: u64, kind: Kind) -> Event {
        Event { ts: (sec * NS as f64) as u64, conn, kind }
    }

    fn ticker(sec: f64, mid: f64) -> Event {
        let t = json!({ "type": "ticker", "best_bid": format!("{:.2}", mid - 0.005), "best_ask": format!("{:.2}", mid + 0.005) });
        ev(sec, COINBASE_CONN, Kind::WsIn(t.to_string()))
    }

    /// A tiny made-up session: BTC jumps $100 early in the window while
    /// Polymarket still offers Up at 40c, then the window settles Up.
    fn session() -> Vec<Event> {
        let book = json!([
            { "event_type": "book", "asset_id": "U", "bids": [{"price": "0.38", "size": "100"}], "asks": [{"price": "0.40", "size": "100"}] },
            { "event_type": "book", "asset_id": "D", "bids": [{"price": "0.58", "size": "100"}], "asks": [{"price": "0.62", "size": "100"}] },
        ]);
        vec![
            ev(999.0, 21, Kind::Other("market".into(), json!({ "start": 1000, "end": 1300, "up_token": "U", "down_token": "D" }))),
            ev(999.5, POLY_WS_CONN, Kind::WsIn(book.to_string())),
            ticker(1000.5, 70_000.0),
            ticker(1100.0, 70_100.0),
            ticker(1101.0, 70_100.0),
            ticker(1301.0, 70_120.0),
            ev(1400.0, 21, Kind::Other("resolution".into(), json!({ "start": 1000, "up_won": true }))),
        ]
    }

    fn run(cfg: Config) -> Report {
        let mut sim = Sim::new(cfg).unwrap();
        for e in session() {
            sim.update(&e);
        }
        sim.finish()
    }

    #[test]
    fn buys_cheap_up_and_settles() {
        let r = run(Config { max_shares: 10.0, ..Config::default() });
        assert_eq!((r.windows, r.official, r.fills, r.traded_windows), (1, 1, 1, 1));
        // 10 shares at 40c, fee 0.05 * 10 * 0.4 * 0.6, each pays $1.
        let expected = 10.0 - 4.0 - 0.12;
        assert!((r.end_cash - r.start_cash - expected).abs() < 1e-9, "{r:?}");
        assert_eq!(r.proxy_agreed, 1);
    }

    #[test]
    fn same_input_same_report() {
        assert_eq!(run(Config::default()).row(), run(Config::default()).row());
    }

    #[test]
    fn latency_can_miss_the_window() {
        // Orders take 10 minutes to "arrive": the window closes first.
        let r = run(Config { latency_ms: 600_000, ..Config::default() });
        assert_eq!(r.fills, 0);
    }
}
