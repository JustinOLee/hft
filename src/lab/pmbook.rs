//! A Polymarket order book for one outcome token (say "Up").
//!
//! Unlike Kalshi, Polymarket sends both sides directly: bids (people
//! wanting to buy) and asks (people wanting to sell). Prices are dollars
//! from 0.00 to 1.00, sizes are shares. We store prices as integers in
//! thousandths of a dollar so they can be exact map keys.

use std::collections::BTreeMap;

use serde_json::Value;

pub type Ticks = i64; // 1 tick = $0.001

pub fn to_ticks(price: f64) -> Ticks {
    (price * 1000.0).round() as Ticks
}

pub fn to_price(t: Ticks) -> f64 {
    t as f64 / 1000.0
}

#[derive(Default, Debug, Clone)]
pub struct PmBook {
    pub bids: BTreeMap<Ticks, f64>,
    pub asks: BTreeMap<Ticks, f64>,
}

fn num(v: &Value) -> Option<f64> {
    v.as_str()?.parse().ok()
}

impl PmBook {
    /// Replace everything with a "book" snapshot message.
    pub fn apply_snapshot(&mut self, msg: &Value) {
        self.bids.clear();
        self.asks.clear();
        for (side, key) in [(&mut self.bids, "bids"), (&mut self.asks, "asks")] {
            for level in msg[key].as_array().into_iter().flatten() {
                if let (Some(p), Some(s)) = (num(&level["price"]), num(&level["size"])) {
                    if s > 0.0 {
                        side.insert(to_ticks(p), s);
                    }
                }
            }
        }
    }

    /// One entry of a "price_change" message: the new TOTAL size at a price
    /// (0 = that level is gone). side BUY = bids, SELL = asks.
    pub fn apply_change(&mut self, change: &Value) {
        let (Some(p), Some(s)) = (num(&change["price"]), num(&change["size"])) else { return };
        let side = match change["side"].as_str() {
            Some("BUY") => &mut self.bids,
            Some("SELL") => &mut self.asks,
            _ => return,
        };
        if s > 0.0 {
            side.insert(to_ticks(p), s);
        } else {
            side.remove(&to_ticks(p));
        }
    }

    pub fn best_bid(&self) -> Option<f64> {
        self.bids.keys().next_back().map(|&t| to_price(t))
    }

    pub fn best_ask(&self) -> Option<f64> {
        self.asks.keys().next().map(|&t| to_price(t))
    }

    pub fn best_ask_size(&self) -> f64 {
        self.asks.values().next().copied().unwrap_or(0.0)
    }

    pub fn mid(&self) -> Option<f64> {
        Some((self.best_bid()? + self.best_ask()?) / 2.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn snapshot_then_changes() {
        let mut b = PmBook::default();
        b.apply_snapshot(&json!({
            "bids": [{"price": "0.48", "size": "10"}, {"price": "0.47", "size": "5"}],
            "asks": [{"price": "0.52", "size": "7"}],
        }));
        assert_eq!((b.best_bid(), b.best_ask(), b.best_ask_size()), (Some(0.48), Some(0.52), 7.0));
        b.apply_change(&json!({"price": "0.50", "size": "3", "side": "SELL"}));
        b.apply_change(&json!({"price": "0.48", "size": "0", "side": "BUY"}));
        assert_eq!((b.best_bid(), b.best_ask()), (Some(0.47), Some(0.50)));
        assert_eq!(b.mid(), Some(0.485));
    }
}
