//! Kalshi-specific settings and request signing.
//!
//! Kalshi has no passwords for its API. You create a key pair on the website:
//! Kalshi keeps the public half, you keep the private half (a .pem file). For
//! each request (and each websocket connection) you sign the string
//!     <timestamp in ms><METHOD><path>
//! e.g. "1795212422993GET/trade-api/ws/v2", and send the signature in headers.
//! Kalshi checks it with your public key. The timestamp must be within 30 s
//! of Kalshi's clock, so a captured signature soon becomes useless.

use std::fs;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use ed25519_dalek::pkcs8::DecodePrivateKey;
use ed25519_dalek::{Signer, SigningKey};

use serde_json::json;

use crate::action::{Action, OrderSide};
use crate::fixed;
use crate::net::Header;
use crate::Error;

pub const WS_PATH: &str = "/trade-api/ws/v2";
pub const REST_PREFIX: &str = "/trade-api/v2";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Env {
    /// Fake money, separate accounts and keys: demo.kalshi.co
    Demo,
    /// Real money.
    Prod,
}

impl Env {
    pub fn ws_host(self) -> &'static str {
        match self {
            Env::Demo => "external-api-ws.demo.kalshi.co",
            Env::Prod => "external-api-ws.kalshi.com",
        }
    }

    pub fn rest_host(self) -> &'static str {
        match self {
            Env::Demo => "external-api.demo.kalshi.co",
            Env::Prod => "external-api.kalshi.com",
        }
    }
}

/// One REST request, described but not yet sent.
#[derive(Debug, PartialEq)]
pub struct RestCall {
    pub method: &'static str,
    /// Path WITHOUT the query string: this is what gets signed.
    pub path: String,
    /// "" or "?key=value..."
    pub query: String,
    pub body: Option<String>,
}

impl RestCall {
    fn new(method: &'static str, path: String) -> RestCall {
        RestCall { method, path, query: String::new(), body: None }
    }
}

/// Translate an action into the REST request that carries it out, or None
/// if the action doesn't go over REST. A pure function: easy to test.
pub fn rest_call(action: &Action) -> Option<RestCall> {
    match action {
        Action::Place { client_order_id, ticker, side, price, count, post_only } => {
            let body = json!({
                "ticker": ticker,
                "side": match side { OrderSide::Bid => "bid", OrderSide::Ask => "ask" },
                "count": fixed::format(*count, fixed::QTY_DECIMALS),
                "price": fixed::format_price(*price),
                // Rest until filled or canceled.
                "time_in_force": "good_till_canceled",
                // If we'd trade against our own resting order, cancel the
                // resting one rather than trade with ourselves.
                "self_trade_prevention_type": "maker",
                "client_order_id": client_order_id,
                "post_only": post_only,
            });
            let mut call = RestCall::new("POST", format!("{REST_PREFIX}/portfolio/events/orders"));
            call.body = Some(body.to_string());
            Some(call)
        }
        Action::Cancel { ticker, order_id } => {
            let mut call = RestCall::new("DELETE", format!("{REST_PREFIX}/portfolio/events/orders/{order_id}"));
            // Kalshi needs the ticker to find which part of its system holds
            // the order.
            call.query = format!("?market_ticker={ticker}");
            Some(call)
        }
        Action::Resync { .. } => None,
    }
}

/// Our account balance: a harmless signed request for testing.
pub fn balance_call() -> RestCall {
    RestCall::new("GET", format!("{REST_PREFIX}/portfolio/balance"))
}

/// API credentials. Deliberately NOT `Debug`, so `{:?}` can never print the
/// private key into a log or error message by accident.
pub struct Credentials {
    pub env: Env,
    key_id: String,
    key: SigningKey,
}

impl Credentials {
    /// Load from a small KEY=VALUE file (see kalshi.env.example):
    ///   KALSHI_ENV=demo
    ///   KALSHI_KEY_ID=...
    ///   KALSHI_KEY_FILE=secrets/kalshi-demo.pem
    pub fn load(path: &str) -> Result<Credentials, Error> {
        let text = fs::read_to_string(path)
            .map_err(|e| format!("can't read {path}: {e} (copy kalshi.env.example to get started)"))?;
        let get = |name: &str| -> Result<String, Error> {
            text.lines()
                .filter_map(|l| l.split_once('='))
                .find(|(k, _)| k.trim() == name)
                .map(|(_, v)| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .ok_or_else(|| format!("{path}: missing {name}").into())
        };
        let env = match get("KALSHI_ENV")?.as_str() {
            "demo" => Env::Demo,
            "prod" => Env::Prod,
            other => return Err(format!("KALSHI_ENV must be demo or prod, not {other:?}").into()),
        };
        let key_file = get("KALSHI_KEY_FILE")?;
        let pem = fs::read_to_string(&key_file).map_err(|e| format!("can't read {key_file}: {e}"))?;
        let key = SigningKey::from_pkcs8_pem(&pem).map_err(|_| {
            format!("{key_file} is not an Ed25519 private key (RSA keys aren't supported yet)")
        })?;
        Ok(Credentials { env, key_id: get("KALSHI_KEY_ID")?, key })
    }

    /// The three auth headers for one request. `ts_ms` is passed in rather
    /// than read here, so this function is deterministic and testable; the
    /// caller (network code, at the edge) reads the clock.
    pub fn auth_headers(&self, ts_ms: u64, method: &str, path: &str) -> Vec<Header> {
        let signature = self.key.sign(format!("{ts_ms}{method}{path}").as_bytes());
        vec![
            Header::secret("KALSHI-ACCESS-KEY", &self.key_id),
            Header::secret("KALSHI-ACCESS-SIGNATURE", &BASE64.encode(signature.to_bytes())),
            Header::public("KALSHI-ACCESS-TIMESTAMP", &ts_ms.to_string()),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signature, Verifier};

    #[test]
    fn place_and_cancel_requests() {
        let place = Action::Place {
            client_order_id: "hft1-1".into(),
            ticker: "KXT-1".into(),
            side: OrderSide::Bid,
            price: 100,  // $0.0100
            count: 100,  // 1.00 contract
            post_only: true,
        };
        let call = rest_call(&place).unwrap();
        assert_eq!(call.method, "POST");
        assert_eq!(call.path, "/trade-api/v2/portfolio/events/orders");
        let body: serde_json::Value = serde_json::from_str(call.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["price"], "0.0100");
        assert_eq!(body["count"], "1.00");
        assert_eq!(body["side"], "bid");
        assert_eq!(body["post_only"], true);

        let cancel = Action::Cancel { ticker: "KXT-1".into(), order_id: "abc".into() };
        let call = rest_call(&cancel).unwrap();
        assert_eq!(call.method, "DELETE");
        assert_eq!(call.path, "/trade-api/v2/portfolio/events/orders/abc");
        assert_eq!(call.query, "?market_ticker=KXT-1");
        assert_eq!(call.body, None);
    }

    #[test]
    fn signs_timestamp_method_path() {
        // A throwaway key made from fixed bytes, so the test is repeatable.
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let public = key.verifying_key();
        let creds = Credentials { env: Env::Demo, key_id: "abc".into(), key };

        let headers = creds.auth_headers(1700000000000, "GET", WS_PATH);
        let value = |name: &str| headers.iter().find(|h| h.name == name).unwrap().value.clone();
        assert_eq!(value("KALSHI-ACCESS-KEY"), "abc");
        assert_eq!(value("KALSHI-ACCESS-TIMESTAMP"), "1700000000000");

        // The signature must verify against exactly "<ts><METHOD><path>".
        let sig_bytes = BASE64.decode(value("KALSHI-ACCESS-SIGNATURE")).unwrap();
        let sig = Signature::from_slice(&sig_bytes).unwrap();
        assert!(public.verify(b"1700000000000GET/trade-api/ws/v2", &sig).is_ok());
        assert!(public.verify(b"1700000000001GET/trade-api/ws/v2", &sig).is_err());
    }
}
