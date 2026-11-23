//! The paper account: fake money, real prices.
//!
//! Orders fill against the RECORDED order book, as a taker: we buy from the
//! cheapest sellers first, up to our limit price, and only as many shares as
//! were really offered. When a window settles, each winning share pays $1.
//!
//! What this simple fill model gets wrong (to improve later, plan step 14):
//!   - Our order doesn't change the book. In reality, after we buy the
//!     shares at 52c, they're gone for everyone, including our next order.
//!   - Resting (maker) orders aren't simulated at all yet. Those need a
//!     guess at our place in line ("queue position").

use std::collections::BTreeMap;

use crate::lab::pmbook::{to_price, PmBook};
use crate::lab::strategy::Outcome;

#[derive(Debug, Clone)]
#[allow(dead_code)] // kept for reports and debugging
pub struct Fill {
    pub ts: u64,
    pub window: u64,
    pub outcome: Outcome,
    pub shares: f64,
    pub avg_price: f64,
    pub fee: f64,
}

#[derive(Default, Debug, Clone)]
struct Holding {
    shares: f64,
    /// Dollars spent on these shares, fees included.
    cost: f64,
}

pub struct Paper {
    pub cash: f64,
    pub fee_rate: f64,
    holdings: BTreeMap<(u64, Outcome), Holding>,
    pub fills: Vec<Fill>,
    pub fees: f64,
}

impl Paper {
    pub fn new(budget: f64, fee_rate: f64) -> Paper {
        Paper { cash: budget, fee_rate, holdings: BTreeMap::new(), fills: Vec::new(), fees: 0.0 }
    }

    pub fn held(&self, window: u64, outcome: Outcome) -> f64 {
        self.holdings.get(&(window, outcome)).map_or(0.0, |h| h.shares)
    }

    fn fee(&self, shares: f64, price: f64) -> f64 {
        self.fee_rate * shares * price * (1.0 - price)
    }

    /// Buy up to `shares` at up to `limit`, walking the asks from the
    /// cheapest. Stops when out of shares, out of cash, or past the limit.
    pub fn buy(&mut self, ts: u64, window: u64, outcome: Outcome, book: &PmBook, shares: f64, limit: f64) -> Option<Fill> {
        let mut want = shares;
        let (mut got, mut spent, mut fees) = (0.0, 0.0, 0.0);
        for (&t, &size) in &book.asks {
            let price = to_price(t);
            if price > limit + 1e-9 || want <= 0.0 {
                break;
            }
            let per_share = price + self.fee(1.0, price);
            let affordable = ((self.cash - spent - fees) / per_share).max(0.0);
            // Whole cents of shares only, so tiny float leftovers don't count.
            let n = (want.min(size).min(affordable) * 100.0).floor() / 100.0;
            if n <= 0.0 {
                break;
            }
            got += n;
            spent += n * price;
            fees += self.fee(n, price);
            want -= n;
        }
        if got <= 0.0 {
            return None;
        }
        self.cash -= spent + fees;
        self.fees += fees;
        let h = self.holdings.entry((window, outcome)).or_default();
        h.shares += got;
        h.cost += spent + fees;
        let fill = Fill { ts, window, outcome, shares: got, avg_price: spent / got, fee: fees };
        self.fills.push(fill.clone());
        Some(fill)
    }

    /// The window is over: pay $1 per winning share. Returns the window's
    /// profit or loss (payout minus everything spent on it).
    pub fn settle(&mut self, window: u64, up_won: bool) -> f64 {
        let mut pnl = 0.0;
        for outcome in [Outcome::Up, Outcome::Down] {
            if let Some(h) = self.holdings.remove(&(window, outcome)) {
                let won = (outcome == Outcome::Up) == up_won;
                let payout = if won { h.shares } else { 0.0 };
                self.cash += payout;
                pnl += payout - h.cost;
            }
        }
        pnl
    }

    /// Windows we still hold shares in.
    pub fn open_windows(&self) -> Vec<u64> {
        let mut w: Vec<u64> = self.holdings.keys().map(|(w, _)| *w).collect();
        w.dedup();
        w
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn book() -> PmBook {
        let mut b = PmBook::default();
        b.apply_snapshot(&json!({ "bids": [], "asks": [
            {"price": "0.50", "size": "5"}, {"price": "0.55", "size": "10"}, {"price": "0.70", "size": "100"} ] }));
        b
    }

    #[test]
    fn walks_the_book_up_to_the_limit() {
        let mut p = Paper::new(100.0, 0.0);
        let f = p.buy(1, 0, Outcome::Up, &book(), 20.0, 0.60).unwrap();
        assert_eq!(f.shares, 15.0); // 5 @ 0.50 + 10 @ 0.55; 0.70 is over the limit
        assert!((f.avg_price - (2.5 + 5.5) / 15.0).abs() < 1e-9);
        assert!((p.cash - 92.0).abs() < 1e-9);
    }

    #[test]
    fn cash_limits_size_and_settlement_pays() {
        let mut p = Paper::new(2.0, 0.0);
        let f = p.buy(1, 0, Outcome::Up, &book(), 100.0, 0.99).unwrap();
        assert_eq!(f.shares, 4.0); // $2 buys 4 shares at 50c
        let pnl = p.settle(0, true);
        assert!((pnl - 2.0).abs() < 1e-9 && (p.cash - 4.0).abs() < 1e-9);
    }

    #[test]
    fn fees_follow_price_times_one_minus_price() {
        let mut p = Paper::new(100.0, 0.07);
        let f = p.buy(1, 0, Outcome::Up, &book(), 5.0, 0.50).unwrap();
        assert!((f.fee - 0.07 * 5.0 * 0.25).abs() < 1e-9);
        assert!((p.settle(0, false) + 2.5 + f.fee).abs() < 1e-9); // lost it all
    }
}
