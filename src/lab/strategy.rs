//! Strategies: look at the market, say what to buy.
//!
//! A strategy only sees a `View` (a snapshot of everything it's allowed to
//! know) and returns `Intent`s ("buy 10 Up at up to 55c"). It can't see the
//! future, the clock, or the paper account's internals. That keeps every
//! strategy honest and every backtest repeatable.
//!
//! To add a strategy: write a struct, implement `Strategy` for it, and add
//! a line to `build`.

use crate::lab::config::Config;
use crate::Error;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Outcome {
    Up,
    Down,
}

/// The top of one outcome's order book.
#[derive(Clone, Copy, Debug, Default)]
#[allow(dead_code)]
pub struct Quote {
    pub bid: Option<f64>,
    pub ask: Option<f64>,
    pub ask_size: f64,
}

/// Everything a strategy may know at one moment.
#[derive(Clone, Debug)]
#[allow(dead_code)] // not every strategy reads every field
pub struct View {
    pub window_start: u64,
    pub seconds_left: f64,
    /// Coinbase BTC-USD mid now, and at the start of the window.
    pub spot: f64,
    pub open: f64,
    /// Volatility estimate (vol.rs) and the model's P(Up) (model.rs).
    pub sigma: f64,
    pub p_up: f64,
    pub up: Quote,
    pub down: Quote,
    /// Shares held this window, and paper cash left.
    pub held_up: f64,
    pub held_down: f64,
    pub cash: f64,
}

/// "Buy up to `shares` of `outcome`, paying at most `limit` per share."
///
/// - Taker (`maker: false`): immediate-or-cancel. Whatever can't fill
///   right away against the book is dropped. Pays the taker fee.
/// - Maker (`maker: true`): a resting bid at exactly `limit`, "post-only"
///   (if it would trade immediately, it's rejected instead). It waits for a
///   seller to come to it. Each decision REPLACES all of the strategy's
///   resting orders in the window, so returning nothing cancels them.
#[derive(Clone, Debug, PartialEq)]
pub struct Intent {
    pub outcome: Outcome,
    pub shares: f64,
    pub limit: f64,
    pub maker: bool,
}

pub trait Strategy {
    fn decide(&mut self, v: &View) -> Vec<Intent>;
}

pub fn build(cfg: &Config) -> Result<Box<dyn Strategy>, Error> {
    Ok(match cfg.strategy.as_str() {
        "fair_value_taker" => Box::new(FairValueTaker { cfg: cfg.clone() }),
        "fair_value_maker" => Box::new(FairValueMaker { cfg: cfg.clone() }),
        "always_up" => Box::new(AlwaysUp { shares: cfg.shares }),
        "none" => Box::new(Nothing),
        other => return Err(format!("unknown strategy {other:?}").into()),
    })
}

/// Baseline: do nothing. Useful to check the model's predictions alone.
struct Nothing;

impl Strategy for Nothing {
    fn decide(&mut self, _: &View) -> Vec<Intent> {
        Vec::new()
    }
}

/// Baseline: buy Up once at the start of every window, at any price.
/// With no information, this should lose about the spread plus fees.
/// If a "smart" strategy can't beat this, it isn't smart.
struct AlwaysUp {
    shares: f64,
}

impl Strategy for AlwaysUp {
    fn decide(&mut self, v: &View) -> Vec<Intent> {
        if v.held_up > 0.0 || v.up.ask.is_none() {
            return Vec::new();
        }
        vec![Intent { outcome: Outcome::Up, shares: self.shares, limit: 0.99, maker: false }]
    }
}

/// The first real strategy: compare the model's fair value with the price.
///
///   fair value of Up   = p_up        (it pays $1 with probability p_up)
///   fair value of Down = 1 - p_up
///
/// If someone sells Up for less than fair value minus `edge`, buy it.
/// `edge` is a safety margin for everything the model gets wrong, plus fees.
struct FairValueTaker {
    cfg: Config,
}

impl Strategy for FairValueTaker {
    fn decide(&mut self, v: &View) -> Vec<Intent> {
        let c = &self.cfg;
        if v.seconds_left < c.no_trade_last_s {
            return Vec::new();
        }
        let mut out = Vec::new();
        for (outcome, fair, quote, held) in
            [(Outcome::Up, v.p_up, v.up, v.held_up), (Outcome::Down, 1.0 - v.p_up, v.down, v.held_down)]
        {
            let Some(ask) = quote.ask else { continue };
            let room = c.max_shares - held;
            if fair - ask >= c.edge && room > 0.0 {
                // Pay up to fair - edge: never more than we think it's worth.
                out.push(Intent { outcome, shares: c.shares.min(room), limit: fair - c.edge, maker: false });
            }
        }
        out
    }
}

/// Market making, the simple way: rest a bid one tick (1c) above the best
/// bid, on Up and on Down, whenever that price is still at least `edge`
/// below fair value.
///
/// Why bids on BOTH sides? One Up share plus one Down share always pays
/// exactly $1. Buy Up at 48c and Down at 48c and you've locked in 4c,
/// whatever BTC does. In practice fills come unevenly, so you end up
/// holding some of one side: that's inventory risk, capped by max_shares.
///
/// The danger is ADVERSE SELECTION: sellers who hit your bid often know
/// BTC just moved against it. Our quotes are only as fresh as our last
/// decision plus `latency_ms`.
struct FairValueMaker {
    cfg: Config,
}

impl Strategy for FairValueMaker {
    fn decide(&mut self, v: &View) -> Vec<Intent> {
        let c = &self.cfg;
        if v.seconds_left < c.no_trade_last_s {
            return Vec::new(); // cancels everything
        }
        let mut out = Vec::new();
        for (outcome, fair, quote, held) in
            [(Outcome::Up, v.p_up, v.up, v.held_up), (Outcome::Down, 1.0 - v.p_up, v.down, v.held_down)]
        {
            let (Some(bid), Some(ask)) = (quote.bid, quote.ask) else { continue };
            let price = ((bid + 0.01) * 100.0).round() / 100.0;
            let room = c.max_shares - held;
            if price < ask - 1e-9 && fair - price >= c.edge && room > 0.0 {
                out.push(Intent { outcome, shares: c.shares.min(room), limit: price, maker: true });
            }
        }
        out
    }
}
