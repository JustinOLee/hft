//! The levers: every setting a backtest uses, loaded from a JSON file.
//!
//! Anything left out of the file keeps its default. A misspelled setting
//! is an error (`deny_unknown_fields`), so a typo can't silently do nothing.

use serde::Deserialize;

use crate::Error;

#[derive(Deserialize, Debug, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Label for the results table.
    pub name: String,
    /// Which strategy to run: "fair_value_taker", "fair_value_maker",
    /// "always_up", "none".
    pub strategy: String,

    // ---- Money ----
    /// Paper dollars to start with.
    pub budget: f64,
    /// Fee for taking liquidity: `fee_rate * shares * price * (1 - price)`.
    /// Polymarket's BTC 5- and 15-minute markets: 0.05 for takers, 0 for
    /// makers (docs.polymarket.com/trading/fees). Kalshi uses 0.07.
    pub fee_rate: f64,
    /// Same formula for resting (maker) orders. Polymarket: 0. (It even
    /// pays rebates to makers; we ignore those, to be conservative.)
    pub maker_fee_rate: f64,

    // ---- Strategy ----
    /// Only buy when fair value beats the price by at least this much ($).
    pub edge: f64,
    /// Shares per order.
    pub shares: f64,
    /// Most shares to hold of each side in one window.
    pub max_shares: f64,
    /// Don't trade in the last N seconds of a window.
    pub no_trade_last_s: f64,

    // ---- Model ----
    /// Which exchange(s) give the BTC price: "coinbase", "kraken", or
    /// "mean" (their average; needs a recording with both feeds).
    pub spot_source: String,
    /// Settlement rule the fair value assumes: "avg60" (last-minute average
    /// vs the minute-before-start average: what the market really uses) or
    /// "point" (end price vs start price: the first, simpler guess).
    pub model: String,
    /// Volatility estimate's half-life, seconds (see vol.rs). 30 min tested
    /// best on 30 days of history (RESEARCH.md, E5), though it matters little.
    pub vol_halflife_s: f64,
    /// Starting volatility guess, per sqrt(second). 1e-4 is about 50%/year.
    pub vol_prior: f64,

    // ---- Simulation ----
    /// Only trade and score windows starting in [first_window, last_window]
    /// (unix seconds; 0 = no limit). Lets you tune on one part of a
    /// recording and test on another.
    pub first_window: u64,
    pub last_window: u64,
    /// Pretend each order takes this long to reach the exchange. Fills use
    /// the book as it was then, not when we decided.
    pub latency_ms: u64,
    /// Ask the strategy what to do this often (event time).
    pub eval_ms: u64,
    /// Print every decision and fill.
    pub verbose: bool,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            name: "default".into(),
            strategy: "fair_value_taker".into(),
            budget: 1000.0,
            fee_rate: 0.05,
            maker_fee_rate: 0.0,
            edge: 0.05,
            shares: 10.0,
            max_shares: 100.0,
            no_trade_last_s: 10.0,
            model: "avg60".into(),
            spot_source: "coinbase".into(),
            vol_halflife_s: 1800.0,
            vol_prior: 1e-4,
            first_window: 0,
            last_window: 0,
            latency_ms: 100,
            eval_ms: 1000,
            verbose: false,
        }
    }
}

impl Config {
    pub fn load(path: &str) -> Result<Config, Error> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        let mut cfg: Config = serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"))?;
        if cfg.name == "default" {
            // Use the file name as the label if none was given.
            cfg.name = std::path::Path::new(path).file_stem().map_or(path.into(), |s| s.to_string_lossy().into_owned());
        }
        Ok(cfg)
    }
}
