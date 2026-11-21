//! Actions: what the state machine asks the network thread to do.
//!
//! The state machine never sends anything itself. `update` returns a list of
//! actions, the network thread writes each one to the log, then carries it
//! out. In replay mode the actions are only printed.
//!
//! `#[derive(Serialize, Deserialize)]` (from the serde library) generates
//! the code to turn these into JSON and back, so they can go in the log.

use serde::{Deserialize, Serialize};

use crate::fixed::{Price, Qty};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderSide {
    /// Buy YES.
    Bid,
    /// Sell YES (equivalently, buy NO at $1 - price).
    Ask,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    /// Place a limit order that rests until filled or canceled.
    Place {
        /// Our own ID for the order, so we can recognize it in every reply
        /// without waiting for Kalshi to tell us its ID.
        client_order_id: String,
        ticker: String,
        side: OrderSide,
        price: Price,
        count: Qty,
        /// Reject the order instead of letting it trade immediately. This
        /// guarantees we only ever add to the book (we're the "maker").
        post_only: bool,
    },
    /// Cancel a resting order. Kalshi needs its own order ID for this.
    Cancel { ticker: String, order_id: String },
    /// Ask for fresh order book snapshots, after a sequence gap.
    Resync { sid: u64, tickers: Vec<String> },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_round_trip() {
        let a = Action::Place {
            client_order_id: "hft1-1".into(),
            ticker: "T".into(),
            side: OrderSide::Bid,
            price: 100,
            count: 100,
            post_only: true,
        };
        let json = serde_json::to_string(&a).unwrap();
        assert!(json.contains(r#""action":"place""#));
        assert!(json.contains(r#""side":"bid""#));
        assert_eq!(serde_json::from_str::<Action>(&json).unwrap(), a);
    }
}
