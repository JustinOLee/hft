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

use std::collections::{BTreeMap, VecDeque};

use serde_json::Value;

use crate::event::{Event, Kind};
use crate::lab::config::Config;
use crate::lab::feeds::{COINBASE_CONN, KRAKEN_CONN, POLY_WS_CONN};
use crate::lab::model;
use crate::lab::paper::Paper;
use crate::lab::pmbook::{to_ticks, PmBook};
use crate::lab::strategy::{self, Intent, Outcome, Quote, Strategy, View};
use crate::lab::vol::Vol;
use crate::Error;

const NS: u64 = 1_000_000_000;

/// Only trust a window's start price if we saw a spot tick within this many
/// seconds of the start. (If recording began mid-window, we didn't.)
const OPEN_TOLERANCE_S: u64 = 2;

/// The market settles on one-minute averages (see model.rs).
const AVG_S: u64 = 60;

/// Only trust a one-minute average if we have prices for most of its seconds.
const MIN_AVG_SAMPLES: usize = 45;

#[derive(Default)]
struct Window {
    start: u64,
    end: u64,
    up_token: String,
    down_token: String,
    up: PmBook,
    down: PmBook,
    /// Coinbase mid at the first tick at/after start and end ("point" rule).
    open_spot: Option<f64>,
    close_spot: Option<f64>,
    /// Coinbase average over the minute before start, and over the last
    /// minute ("avg60" rule, what the market really uses).
    ref_avg: Option<f64>,
    end_avg: Option<f64>,
    /// The official result, from Polymarket.
    up_won: Option<bool>,
    /// (model P(up), market mid of Up), sampled at each evaluation, to score
    /// both forecasts once we know the answer.
    forecasts: Vec<(f64, Option<f64>)>,
    pnl: f64,
}

impl Window {
    /// The best guess of how it ended: official if known, else our proxy
    /// computed from Coinbase. The proxy uses a different price source than
    /// the market (Chainlink), so it's occasionally wrong when the move is tiny.
    fn result(&self, avg: bool) -> Option<(bool, bool)> {
        if let Some(w) = self.up_won {
            return Some((w, true));
        }
        Some((self.proxy(avg)?, false))
    }

    fn proxy(&self, avg: bool) -> Option<bool> {
        if avg {
            Some(self.end_avg? >= self.ref_avg?)
        } else {
            Some(self.close_spot? >= self.open_spot?)
        }
    }

    /// The price the window is measured against, under the chosen rule.
    fn reference(&self, avg: bool) -> Option<f64> {
        if avg {
            self.ref_avg
        } else {
            self.open_spot
        }
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

/// A maker order resting in the (simulated) book.
///
/// It reaches the exchange `latency_ms` after the decision, and a cancel
/// also takes `latency_ms` to arrive. In between it can be filled, even if
/// we've already decided it's a bad price: that's how quotes get "picked off".
struct Resting {
    live_from: u64,
    dead_from: u64,
    window: u64,
    outcome: Outcome,
    price: f64,
    remaining: f64,
    /// Checked for post-only when it first goes live.
    checked: bool,
}

pub struct Sim {
    cfg: Config,
    strat: Box<dyn Strategy>,
    now: u64,
    spot: Option<f64>,
    /// Latest mid on each exchange, and which of them make up `spot`.
    coinbase_mid: Option<f64>,
    kraken_mid: Option<f64>,
    spot_source: SpotSource,
    /// The latest mid in each of the last few hundred seconds: (second, mid).
    seconds: VecDeque<(u64, f64)>,
    /// True for the "avg60" settlement rule, false for "point".
    avg_rule: bool,
    vol: Vol,
    windows: BTreeMap<u64, Window>,
    /// token id -> (window start, which outcome)
    tokens: BTreeMap<String, (u64, Outcome)>,
    paper: Paper,
    pending: Vec<Pending>,
    resting: Vec<Resting>,
    next_eval: u64,
    /// If set, every evaluation is also written here as a CSV row.
    export: Option<Box<dyn std::io::Write>>,
}

/// Column names for `hft lab export`.
pub const EXPORT_HEADER: &str =
    "ts_ns,window,secs_left,spot,reference,sigma,p_up,up_bid,up_ask,up_bid_size,up_ask_size,down_bid,down_ask";

impl Sim {
    pub fn new(cfg: Config) -> Result<Sim, Error> {
        let avg_rule = match cfg.model.as_str() {
            "avg60" => true,
            "point" => false,
            other => return Err(format!("unknown model {other:?} (use \"avg60\" or \"point\")").into()),
        };
        let spot_source = match cfg.spot_source.as_str() {
            "coinbase" => SpotSource::Coinbase,
            "kraken" => SpotSource::Kraken,
            "mean" => SpotSource::Mean,
            other => return Err(format!("unknown spot_source {other:?} (coinbase, kraken, mean)").into()),
        };
        Ok(Sim {
            strat: strategy::build(&cfg)?,
            vol: Vol::new(cfg.vol_prior, cfg.vol_halflife_s),
            paper: Paper::new(cfg.budget, cfg.fee_rate),
            cfg,
            now: 0,
            spot: None,
            coinbase_mid: None,
            kraken_mid: None,
            spot_source,
            seconds: VecDeque::new(),
            avg_rule,
            windows: BTreeMap::new(),
            tokens: BTreeMap::new(),
            pending: Vec::new(),
            resting: Vec::new(),
            next_eval: 0,
            export: None,
        })
    }

    /// Write one CSV row per evaluation to `out` (header first).
    pub fn export_to(&mut self, mut out: Box<dyn std::io::Write>) -> Result<(), Error> {
        writeln!(out, "{EXPORT_HEADER}")?;
        self.export = Some(out);
        Ok(())
    }

    pub fn update(&mut self, ev: &Event) {
        let msg = parse_message(ev);
        self.update_parsed(ev, msg.as_ref());
    }

    /// Same as `update`, for callers that already parsed the message inside
    /// the event (see `parse_message`). A sweep parses each message once and
    /// hands it to many simulators.
    pub fn update_parsed(&mut self, ev: &Event, msg: Option<&Value>) {
        self.now = ev.ts;
        self.fill_due_orders();
        let now = self.now;
        let windows = &self.windows;
        self.resting.retain(|r| {
            r.dead_from > now && r.remaining > 0.0 && windows.get(&r.window).is_none_or(|w| now < w.end * NS)
        });
        match (&ev.kind, msg) {
            (Kind::WsIn(_), Some(v)) if ev.conn == COINBASE_CONN => self.on_coinbase(v),
            (Kind::WsIn(_), Some(v)) if ev.conn == POLY_WS_CONN => self.on_polymarket(v),
            (Kind::WsIn(_), Some(v)) if ev.conn == KRAKEN_CONN => self.on_kraken(v),
            (Kind::Other(name, data), _) if name == "market" => self.on_market(data),
            (Kind::Other(name, data), _) if name == "resolution" => {
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

    fn on_coinbase(&mut self, v: &Value) {
        if v["type"] != "ticker" {
            return;
        }
        let num = |k: &str| v[k].as_str().and_then(|s| s.parse::<f64>().ok());
        let (Some(bid), Some(ask)) = (num("best_bid"), num("best_ask")) else { return };
        self.coinbase_mid = Some((bid + ask) / 2.0);
        self.on_spot_change();
    }

    /// Kraken v2 ticker: {"channel":"ticker","data":[{"bid":..,"ask":..}]}
    /// (numbers, not strings, unlike Coinbase).
    fn on_kraken(&mut self, v: &Value) {
        if v["channel"] != "ticker" {
            return;
        }
        let d = &v["data"][0];
        let (Some(bid), Some(ask)) = (d["bid"].as_f64(), d["ask"].as_f64()) else { return };
        self.kraken_mid = Some((bid + ask) / 2.0);
        self.on_spot_change();
    }

    fn on_spot_change(&mut self) {
        let mid = match (self.spot_source, self.coinbase_mid, self.kraken_mid) {
            (SpotSource::Coinbase, Some(c), _) => c,
            (SpotSource::Kraken, _, Some(k)) => k,
            (SpotSource::Mean, Some(c), Some(k)) => (c + k) / 2.0,
            _ => return, // the chosen source(s) haven't spoken yet
        };
        self.spot = Some(mid);
        let sec = self.now / NS;
        self.vol.sample(sec, mid);
        match self.seconds.back_mut() {
            Some((s, m)) if *s == sec => *m = mid,
            _ => self.seconds.push_back((sec, mid)),
        }
        while self.seconds.front().is_some_and(|&(s, _)| s + 600 < sec) {
            self.seconds.pop_front();
        }
        for w in self.windows.values_mut() {
            if w.open_spot.is_none() && sec >= w.start && sec < w.start + OPEN_TOLERANCE_S {
                w.open_spot = Some(mid);
            }
            if w.close_spot.is_none() && w.end > 0 && sec >= w.end {
                w.close_spot = Some(mid);
            }
            // The tick that starts a new second closes the previous one, so
            // the minute [from, to) is complete once sec >= to.
            if w.ref_avg.is_none() && w.end > 0 && sec >= w.start && sec < w.start + OPEN_TOLERANCE_S {
                w.ref_avg = average(&self.seconds, w.start - AVG_S, w.start, MIN_AVG_SAMPLES);
            }
            if w.end_avg.is_none() && w.end > 0 && sec >= w.end {
                w.end_avg = average(&self.seconds, w.end - AVG_S, w.end, MIN_AVG_SAMPLES);
            }
        }
    }

    fn on_polymarket(&mut self, v: &Value) {
        // Orders that just reached the exchange get their post-only check
        // against the book as it was, before this message changes it.
        if !self.resting.is_empty() {
            self.check_book_crosses();
        }
        // A message is one object or a list of them.
        let msgs = match v {
            Value::Array(a) => a.iter().collect(),
            other => vec![other],
        };
        for m in msgs {
            match m["event_type"].as_str() {
                Some("book") => {
                    if let Some(book) = self.book_mut(m["asset_id"].as_str()) {
                        book.apply_snapshot(m);
                    }
                }
                Some("price_change") => {
                    for c in m["price_changes"].as_array().into_iter().flatten() {
                        if let Some(book) = self.book_mut(c["asset_id"].as_str()) {
                            book.apply_change(c);
                        }
                    }
                }
                Some("last_trade_price") => self.on_trade(m),
                _ => {} // tick_size_change
            }
        }
        if !self.resting.is_empty() {
            self.check_book_crosses();
        }
    }

    /// A trade printed. Would it have hit one of our resting bids?
    /// Polymarket matches a buyer of Up with a seller of Up, OR with a buyer
    /// of Down (together they pay $1 for a new Up+Down pair). So our Up bid
    /// at P would have been hit by:
    ///   - a taker SELLING Up at P or below (we're 1c above the old best
    ///     bid, so we'd have been first in line), or
    ///   - a taker BUYING Down at 1 - P or above.
    fn on_trade(&mut self, m: &Value) {
        let Some(&(window, traded)) = m["asset_id"].as_str().and_then(|a| self.tokens.get(a)) else { return };
        let num = |k: &str| m[k].as_str().and_then(|s| s.parse::<f64>().ok());
        let (Some(price), Some(mut size)) = (num("price"), num("size")) else { return };
        let side = m["side"].as_str().unwrap_or("");
        let mut fills = Vec::new();
        for (i, r) in self.resting.iter().enumerate() {
            if r.window != window || !r.checked || self.now < r.live_from || r.remaining <= 0.0 || size <= 0.0 {
                continue;
            }
            let hits = if r.outcome == traded {
                side == "SELL" && price <= r.price + 1e-9
            } else {
                side == "BUY" && price >= 1.0 - r.price - 1e-9
            };
            if hits {
                let n = size.min(r.remaining);
                size -= n;
                fills.push((i, n));
            }
        }
        self.apply_maker_fills(fills);
    }

    /// Someone offered our outcome at or below our bid. In the recording
    /// that offer just rests in the book; with our bid there, it would have
    /// traded with us. Also: an order that's just arrived and would cross
    /// is rejected (post-only).
    fn check_book_crosses(&mut self) {
        let mut fills = Vec::new();
        for (i, r) in self.resting.iter_mut().enumerate() {
            if self.now < r.live_from || r.remaining <= 0.0 {
                continue;
            }
            let Some(w) = self.windows.get(&r.window) else { continue };
            let book = w.book(r.outcome);
            let crossing: f64 = book.asks.range(..=to_ticks(r.price)).map(|(_, s)| s).sum();
            if !r.checked {
                r.checked = true;
                if crossing > 0.0 {
                    r.remaining = 0.0; // post-only: would have traded at once
                }
                continue;
            }
            if crossing > 0.0 {
                fills.push((i, crossing.min(r.remaining)));
            }
        }
        // After a cross the order is done: the resting ask stays in the
        // recorded book, so counting it again would fill us twice.
        let done: Vec<usize> = fills.iter().map(|(i, _)| *i).collect();
        self.apply_maker_fills(fills);
        for i in done {
            self.resting[i].remaining = 0.0;
        }
    }

    fn apply_maker_fills(&mut self, fills: Vec<(usize, f64)>) {
        for (i, n) in fills {
            let r = &mut self.resting[i];
            r.remaining -= n;
            let (window, outcome, price) = (r.window, r.outcome, r.price);
            if let Some(f) = self.paper.fill_maker(self.now, window, outcome, price, n, self.cfg.maker_fee_rate) {
                if self.cfg.verbose {
                    println!("{} MAKER  {:?} {} @ {:.3}", self.now, f.outcome, f.shares, f.avg_price);
                }
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

    /// The window in progress right now, if we know its reference price.
    fn active_window(&self) -> Option<&Window> {
        let sec = self.now / NS;
        let (first, last) = (self.cfg.first_window, self.cfg.last_window);
        self.windows.values().find(|w| {
            w.start <= sec
                && sec < w.end
                && w.reference(self.avg_rule).is_some()
                && w.start >= first
                && (last == 0 || w.start <= last)
        })
    }

    fn evaluate(&mut self) {
        let Some(spot) = self.spot else { return };
        let Some(w) = self.active_window() else { return };
        let open = w.reference(self.avg_rule).expect("active windows have a reference");
        let seconds_left = (w.end * NS).saturating_sub(self.now) as f64 / NS as f64;
        let sigma = self.vol.sigma();
        let p_up = if self.avg_rule {
            // Inside the last minute, part of the final average is known.
            let partial = average(&self.seconds, w.end - AVG_S, self.now / NS + 1, 1);
            model::prob_up_avg(spot, open, sigma, seconds_left, AVG_S as f64, partial)
        } else {
            model::prob_up(spot, open, sigma, seconds_left)
        };
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
        // Build the CSV row now, while `w` (borrowed from self.windows) is
        // in hand; write it below, once we're done with `w`.
        let row = self.export.is_some().then(|| {
            let f = |x: Option<f64>| x.map_or(String::new(), |v| format!("{v}"));
            let best_size = |m: &std::collections::BTreeMap<i64, f64>, last: bool| {
                let v = if last { m.values().next_back() } else { m.values().next() };
                v.map_or(String::new(), |s| format!("{s}"))
            };
            format!(
                "{},{},{:.3},{},{},{:e},{:.5},{},{},{},{},{},{}",
                self.now, start, seconds_left, spot, open, sigma, p_up,
                f(view.up.bid), f(view.up.ask), best_size(&w.up.bids, true), best_size(&w.up.asks, false),
                f(view.down.bid), f(view.down.ask)
            )
        });
        if let (Some(out), Some(row)) = (self.export.as_mut(), row) {
            let _ = writeln!(out, "{row}");
        }
        self.windows.get_mut(&start).expect("exists").forecasts.push((p_up, market));

        let due = self.now + self.cfg.latency_ms * 1_000_000;
        // A decision replaces all of our resting orders in this window: the
        // old ones are canceled, which takes effect after the latency.
        for r in self.resting.iter_mut().filter(|r| r.window == start) {
            r.dead_from = r.dead_from.min(due);
        }
        for intent in self.strat.decide(&view) {
            if self.cfg.verbose {
                println!(
                    "{} DECIDE {:?} {} @<= {:.3}  (p_up {:.3}, up {:?}/{:?}, {:.0}s left, spot-open {:+.1})",
                    self.now, intent.outcome, intent.shares, intent.limit, p_up, view.up.bid, view.up.ask,
                    seconds_left, spot - open
                );
            }
            if intent.maker {
                self.resting.push(Resting {
                    live_from: due,
                    dead_from: u64::MAX,
                    window: start,
                    outcome: intent.outcome,
                    price: intent.limit,
                    remaining: intent.shares,
                    checked: false,
                });
            } else {
                self.pending.push(Pending { due, window: start, intent });
            }
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
            let Some((up_won, official)) = w.result(self.avg_rule) else { continue };
            r.windows += 1;
            if official {
                r.official += 1;
                if let Some(proxy) = w.proxy(self.avg_rule) {
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

#[derive(Clone, Copy)]
enum SpotSource {
    Coinbase,
    Kraken,
    Mean,
}

/// The JSON inside a websocket message, if the sim cares about this event.
pub fn parse_message(ev: &Event) -> Option<Value> {
    match &ev.kind {
        Kind::WsIn(text) if [COINBASE_CONN, KRAKEN_CONN, POLY_WS_CONN].contains(&ev.conn) => {
            serde_json::from_str(text).ok()
        }
        _ => None,
    }
}

/// Average of the per-second prices in [from, to), if there are at least
/// `min` of them.
fn average(seconds: &VecDeque<(u64, f64)>, from: u64, to: u64, min: usize) -> Option<f64> {
    let (sum, n) = seconds.iter().filter(|(s, _)| *s >= from && *s < to).fold((0.0, 0), |(a, n), (_, m)| (a + m, n + 1));
    (n >= min && n > 0).then(|| sum / n as f64)
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
        let mut evs = vec![
            ev(999.0, 21, Kind::Other("market".into(), json!({ "start": 1000, "end": 1300, "up_token": "U", "down_token": "D" }))),
            ev(999.5, POLY_WS_CONN, Kind::WsIn(book.to_string())),
        ];
        // A quiet minute before the window: the reference average is 70,000.
        evs.extend((940..1000).map(|s| ticker(s as f64 + 0.1, 70_000.0)));
        evs.extend([
            ticker(1000.5, 70_000.0),
            ticker(1100.0, 70_100.0),
            ticker(1101.0, 70_100.0),
            ticker(1301.0, 70_120.0),
        ]);
        // The last minute averages above 70,000 too (for the proxy result).
        evs.extend((1240..1302).map(|s| ticker(s as f64 + 0.2, 70_120.0)));
        evs.push(ev(1400.0, 21, Kind::Other("resolution".into(), json!({ "start": 1000, "up_won": true }))));
        evs.sort_by_key(|e| e.ts);
        evs
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
    fn maker_bids_fill_on_trades_and_pairs() {
        // Model says 50/50 (spot = reference). Up trades 38/42, so the maker
        // bids 39c for Up (11c under fair). Down trades 56/60: 57c is over
        // fair, so no Down bid.
        let mut evs = vec![
            ev(999.0, 21, Kind::Other("market".into(), json!({ "start": 1000, "end": 1300, "up_token": "U", "down_token": "D" }))),
            ev(999.5, POLY_WS_CONN, Kind::WsIn(json!([
                { "event_type": "book", "asset_id": "U", "bids": [{"price": "0.38", "size": "100"}], "asks": [{"price": "0.42", "size": "100"}] },
                { "event_type": "book", "asset_id": "D", "bids": [{"price": "0.56", "size": "100"}], "asks": [{"price": "0.60", "size": "100"}] },
            ]).to_string())),
        ];
        evs.extend((940..1000).map(|s| ticker(s as f64 + 0.1, 70_000.0)));
        let trade = |sec: f64, asset: &str, side: &str, price: &str, size: &str| {
            ev(sec, POLY_WS_CONN, Kind::WsIn(json!({ "event_type": "last_trade_price", "asset_id": asset,
                "side": side, "price": price, "size": size }).to_string()))
        };
        evs.extend([
            ticker(1000.5, 70_000.0),                // decide: bid Up 39c, live at 1000.6
            trade(1000.55, "U", "SELL", "0.38", "50"), // before our bid arrives: no fill
            trade(1000.8, "U", "SELL", "0.38", "4"),   // a seller: hits our 39c bid first
            trade(1000.9, "D", "BUY", "0.62", "3"),    // Down buyer at 62c >= 1 - 39c: pairs with us
            trade(1000.95, "D", "BUY", "0.60", "50"),  // 60c < 61c: matches someone else
            ev(1400.0, 21, Kind::Other("resolution".into(), json!({ "start": 1000, "up_won": true }))),
        ]);
        evs.sort_by_key(|e| e.ts);
        let cfg = Config { strategy: "fair_value_maker".into(), edge: 0.05, ..Config::default() };
        let mut sim = Sim::new(cfg).unwrap();
        for e in &evs {
            sim.update(e);
        }
        let r = sim.finish();
        assert_eq!(r.fills, 2, "{r:?}");
        // 7 shares at 39c, no maker fee, Up won: 7 * 0.61
        assert!((r.end_cash - r.start_cash - 7.0 * 0.61).abs() < 1e-9, "{r:?}");
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
