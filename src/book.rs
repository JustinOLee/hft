//! A Kalshi order book for one market.
//!
//! Kalshi markets are yes/no questions. A YES contract pays $1 if the answer
//! is yes; a NO contract pays $1 if it's no. So YES + NO always = $1.
//!
//! The book only lists BIDS (offers to buy), on both sides:
//!   yes bids: "I'll buy YES at $0.56"
//!   no bids:  "I'll buy NO at $0.43"
//! Buying NO at $0.43 is economically the same as selling YES at $0.57
//! (you give up $0.57 of a YES-or-nothing payout). So:
//!   best YES bid = highest yes bid
//!   best YES ask = $1 - highest no bid
//! We store each side exactly as Kalshi sends it and convert when asked.

use std::collections::BTreeMap;

use crate::fixed::{Price, Qty, ONE_DOLLAR};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Yes,
    No,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Book {
    /// price -> total quantity resting there. A BTreeMap keeps keys sorted,
    /// so the best (highest) bid is always the last entry.
    pub yes: BTreeMap<Price, Qty>,
    pub no: BTreeMap<Price, Qty>,
}

impl Book {
    /// Replace the whole book with a snapshot.
    pub fn from_snapshot(yes: Vec<(Price, Qty)>, no: Vec<(Price, Qty)>) -> Book {
        Book { yes: yes.into_iter().collect(), no: no.into_iter().collect() }
    }

    /// Apply one change: quantity at `price` on `side` changes by `delta`
    /// (negative = orders canceled or filled). A level at zero disappears.
    /// Returns false if the result makes no sense (negative quantity), which
    /// means our book has drifted from the exchange's.
    pub fn apply_delta(&mut self, side: Side, price: Price, delta: Qty) -> bool {
        let levels = match side {
            Side::Yes => &mut self.yes,
            Side::No => &mut self.no,
        };
        let qty = levels.get(&price).copied().unwrap_or(0) + delta;
        if qty > 0 {
            levels.insert(price, qty);
        } else {
            levels.remove(&price);
        }
        qty >= 0
    }

    /// Highest YES bid: (price, quantity).
    pub fn best_bid(&self) -> Option<(Price, Qty)> {
        self.yes.last_key_value().map(|(&p, &q)| (p, q))
    }

    /// Lowest YES ask, derived from the highest NO bid.
    pub fn best_ask(&self) -> Option<(Price, Qty)> {
        self.no.last_key_value().map(|(&p, &q)| (ONE_DOLLAR - p, q))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book() -> Book {
        // yes bids at 0.55 and 0.56; no bids at 0.40 and 0.43
        Book::from_snapshot(vec![(5500, 1000), (5600, 300)], vec![(4000, 500), (4300, 200)])
    }

    #[test]
    fn best_prices() {
        let b = book();
        assert_eq!(b.best_bid(), Some((5600, 300)));
        assert_eq!(b.best_ask(), Some((5700, 200))); // $1 - $0.43
    }

    #[test]
    fn deltas_add_remove_and_detect_drift() {
        let mut b = book();
        assert!(b.apply_delta(Side::Yes, 5600, -300)); // level emptied
        assert_eq!(b.best_bid(), Some((5500, 1000)));
        assert!(b.apply_delta(Side::No, 4400, 100)); // new best no bid
        assert_eq!(b.best_ask(), Some((5600, 100)));
        assert!(!b.apply_delta(Side::Yes, 5000, -1)); // removing what isn't there
    }

    #[test]
    fn empty_sides() {
        let b = Book::default();
        assert_eq!(b.best_bid(), None);
        assert_eq!(b.best_ask(), None);
    }
}
