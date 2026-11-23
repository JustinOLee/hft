//! The lab's recorder: public market data only. No keys, no orders, no money.
//!
//! Two feeds, each on its own thread, each writing its own log file:
//!
//!   coinbase.log.gz    BTC-USD ticker from Coinbase (the "spot" price)
//!   polymarket.log.gz  order books for Polymarket's 5-minute
//!                   "Bitcoin Up or Down" markets, plus which market is which
//!                   ("market" events) and how each one ended ("resolution")
//!
//! Files are gzip-compressed (about 10x smaller; Polymarket's feed is
//! ~3 GB/hour raw). `gunzip -c file.gz | less` to read one.
//!
//! A session is a folder of these files. merge.rs reads them back as one
//! stream, in time order, so backtests see events exactly as they happened.
//!
//! These threads are the edge of the system, so they may read the clock.
//! The simulator (sim.rs) never does.
//!
//! Note: Polymarket's main exchange doesn't let US residents trade. Reading
//! its public data is fine, and that's all we do. For real money later,
//! Kalshi has a similar market (KXBTC15M, every 15 minutes).

use std::collections::BTreeSet;
use std::io;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tungstenite::{Message, WebSocket};

use crate::eventlog::{now_nanos, EventLog};
use crate::http;
use crate::net::{self, TlsStream};
use crate::Error;

pub const SESSION_CONN: u64 = 0;
pub const COINBASE_CONN: u64 = 10;
pub const POLY_WS_CONN: u64 = 20;
pub const POLY_REST_CONN: u64 = 21;

/// Each Polymarket market covers 5 minutes, starting on a multiple of 300 s.
pub const WINDOW_S: u64 = 300;

const COINBASE_HOST: &str = "ws-feed.exchange.coinbase.com";
const POLY_WS_HOST: &str = "ws-subscriptions-clob.polymarket.com";
const POLY_REST_HOST: &str = "gamma-api.polymarket.com";

/// Record until `minutes` have passed (or forever, until Ctrl-C).
pub fn record(dir: &str, minutes: Option<u64>) -> Result<(), Error> {
    std::fs::create_dir_all(dir)?;
    let deadline = minutes.map(|m| Instant::now() + Duration::from_secs(m * 60));
    let mut session = EventLog::create(&format!("{dir}/session.log"))?;
    session.write(SESSION_CONN, "start", json!({ "window_s": WINDOW_S, "minutes": minutes }))?;
    eprintln!("recording to {dir}/ {}", minutes.map_or("until Ctrl-C".into(), |m| format!("for {m} min")));

    let mut cb_log = EventLog::create(&format!("{dir}/coinbase.log.gz"))?;
    let mut pm_log = EventLog::create(&format!("{dir}/polymarket.log.gz"))?;
    let cb = thread::spawn(move || keep_running("coinbase", &mut cb_log, deadline, coinbase_once));
    let mut pm = Polymarket::new();
    let pm = thread::spawn(move || {
        keep_running("polymarket", &mut pm_log, deadline, |log, d| pm.run_once(log, d));
        pm.final_resolutions(&mut pm_log);
    });
    cb.join().map_err(|_| "coinbase thread panicked")?;
    pm.join().map_err(|_| "polymarket thread panicked")?;
    eprintln!("done: {dir}/");
    Ok(())
}

fn past(deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|d| Instant::now() >= d)
}

/// Call `once` again and again until the deadline. `once` returns Ok when it
/// stopped on purpose, Err when something broke; then wait and reconnect,
/// waiting longer each time it breaks in a row (exponential backoff).
fn keep_running(
    name: &str,
    log: &mut EventLog,
    deadline: Option<Instant>,
    mut once: impl FnMut(&mut EventLog, Option<Instant>) -> Result<(), Error>,
) {
    let mut backoff = Duration::from_secs(1);
    while !past(deadline) {
        match once(log, deadline) {
            Ok(()) => backoff = Duration::from_secs(1),
            Err(e) => {
                eprintln!("{name}: {e}; retrying in {backoff:?}");
                let _ = log.write(0, "feed_error", json!(e.to_string()));
                thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

type Ws = WebSocket<TlsStream>;

fn ws_connect(log: &mut EventLog, conn: u64, host: &str, path: &str) -> Result<Ws, Error> {
    let tls = net::tls_connect(log, conn, host)?;
    let url = format!("wss://{host}{path}");
    let (ws, resp) = tungstenite::client::client(url.as_str(), tls).map_err(|e| format!("ws upgrade: {e}"))?;
    log.write(conn, "ws_open", json!(resp.status().as_u16()))?;
    // Wake up at least once a second even if nothing arrives, so we can
    // check the deadline, send pings, and notice a new 5-minute window.
    ws.get_ref().sock.set_read_timeout(Some(Duration::from_secs(1)))?;
    Ok(ws)
}

fn ws_send(log: &mut EventLog, conn: u64, ws: &mut Ws, text: String) -> Result<(), Error> {
    log.write(conn, "ws_out", json!(text))?; // log BEFORE sending
    ws.send(Message::text(text))?;
    Ok(())
}

/// Next text message, or None if a second passed with nothing (or the
/// message wasn't text: tungstenite answers pings by itself).
fn ws_read(log: &mut EventLog, conn: u64, ws: &mut Ws) -> Result<Option<String>, Error> {
    match ws.read() {
        Ok(Message::Text(t)) => Ok(Some(t.to_string())),
        Ok(Message::Close(frame)) => {
            log.write(conn, "ws_close", json!(format!("{frame:?}")))?;
            Err("server closed the websocket".into())
        }
        Ok(_) => Ok(None),
        Err(tungstenite::Error::Io(e)) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
            let _ = ws.flush(); // send any queued pong
            Ok(None)
        }
        Err(e) => Err(e.into()),
    }
}

fn ws_close(ws: &mut Ws) {
    let _ = ws.close(None);
    let _ = ws.flush();
}

// ---- Coinbase --------------------------------------------------------------

fn coinbase_once(log: &mut EventLog, deadline: Option<Instant>) -> Result<(), Error> {
    let mut ws = ws_connect(log, COINBASE_CONN, COINBASE_HOST, "/")?;
    // "ticker" sends best bid/ask and last price after every trade.
    let sub = json!({ "type": "subscribe", "product_ids": ["BTC-USD"], "channels": ["ticker"] });
    ws_send(log, COINBASE_CONN, &mut ws, sub.to_string())?;
    while !past(deadline) {
        if let Some(text) = ws_read(log, COINBASE_CONN, &mut ws)? {
            log.write(COINBASE_CONN, "ws_in", json!(text))?;
        }
    }
    ws_close(&mut ws);
    Ok(())
}

// ---- Polymarket ------------------------------------------------------------

struct Polymarket {
    rest: http::Client,
    /// Windows we've already written a "market" event for.
    announced: BTreeSet<u64>,
    /// Windows we've written a "resolution" event for.
    resolved: BTreeSet<u64>,
}

fn now_s() -> u64 {
    now_nanos() / 1_000_000_000
}

impl Polymarket {
    fn new() -> Polymarket {
        Polymarket { rest: http::Client::new(POLY_REST_HOST, POLY_REST_CONN), announced: BTreeSet::new(), resolved: BTreeSet::new() }
    }

    /// One connection's life: look up the current and next markets,
    /// subscribe to their books, and stream until the window changes.
    /// Then return so the caller reconnects with the new markets.
    ///
    /// TODO(later): Polymarket can add subscriptions without reconnecting,
    /// which would avoid the short gap at each window boundary.
    fn run_once(&mut self, log: &mut EventLog, deadline: Option<Instant>) -> Result<(), Error> {
        let window = now_s() / WINDOW_S * WINDOW_S;
        // HTTP lookups happen here, while no websocket is open, so they
        // never delay (and so mis-timestamp) book messages.
        self.check_resolutions(log, now_s())?;
        let mut tokens = Vec::new();
        for start in [window, window + WINDOW_S] {
            if let Some(m) = self.fetch_market(log, start)? {
                tokens.push(m["up_token"].clone());
                tokens.push(m["down_token"].clone());
                if self.announced.insert(start) {
                    log.write(POLY_REST_CONN, "market", m)?;
                }
            }
        }
        if tokens.is_empty() {
            return Err(format!("no btc-updown-5m market found for window {window}").into());
        }

        let mut ws = ws_connect(log, POLY_WS_CONN, POLY_WS_HOST, "/ws/market")?;
        ws_send(log, POLY_WS_CONN, &mut ws, json!({ "assets_ids": tokens, "type": "market" }).to_string())?;
        let mut last_ping = Instant::now();
        while !past(deadline) && now_s() / WINDOW_S * WINDOW_S == window {
            // Polymarket drops connections that stay silent; it wants a
            // "PING" text message about every 10 seconds.
            if last_ping.elapsed() >= Duration::from_secs(10) {
                ws_send(log, POLY_WS_CONN, &mut ws, "PING".into())?;
                last_ping = Instant::now();
            }
            if let Some(text) = ws_read(log, POLY_WS_CONN, &mut ws)? {
                log.write(POLY_WS_CONN, "ws_in", json!(text))?;
            }
        }
        ws_close(&mut ws);
        log.write(POLY_WS_CONN, "tcp_close", json!("window rollover"))?;
        Ok(())
    }

    /// Look up one window's market. Returns the "market" event's data.
    fn fetch_market(&mut self, log: &mut EventLog, start: u64) -> Result<Option<Value>, Error> {
        let slug = format!("btc-updown-5m-{start}");
        let resp = self.rest.request(log, "GET", &format!("/events?slug={slug}"), &[], None)?;
        if resp.status != 200 {
            return Ok(None);
        }
        let v: Value = serde_json::from_slice(&resp.body)?;
        let Some(m) = v.get(0).and_then(|e| e["markets"].get(0)) else { return Ok(None) };
        // These two fields are JSON arrays packed inside strings.
        let tokens: Vec<String> = serde_json::from_str(m["clobTokenIds"].as_str().unwrap_or("[]"))?;
        let outcomes: Vec<String> = serde_json::from_str(m["outcomes"].as_str().unwrap_or("[]"))?;
        let prices: Vec<String> = serde_json::from_str(m["outcomePrices"].as_str().unwrap_or("[]"))?;
        let find = |name: &str| outcomes.iter().position(|o| o == name).and_then(|i| tokens.get(i));
        let (Some(up), Some(down)) = (find("Up"), find("Down")) else {
            return Err(format!("{slug}: unexpected outcomes {outcomes:?}").into());
        };
        let up_i = outcomes.iter().position(|o| o == "Up").unwrap_or(0);
        Ok(Some(json!({
            "slug": slug,
            "start": start,
            "end": start + WINDOW_S,
            "question": m["question"],
            "up_token": up,
            "down_token": down,
            "closed": m["closed"].as_bool().unwrap_or(false),
            "up_price": prices.get(up_i),
        })))
    }

    /// Write a "resolution" event for every finished window we know about
    /// that has been settled. Polymarket settles about 5 minutes after the
    /// window ends, so only ask about windows that ended a while ago.
    fn check_resolutions(&mut self, log: &mut EventLog, now: u64) -> Result<(), Error> {
        let due: Vec<u64> = self
            .announced
            .iter()
            .copied()
            .filter(|s| !self.resolved.contains(s) && s + WINDOW_S + 60 <= now)
            .collect();
        for start in due {
            let Some(m) = self.fetch_market(log, start)? else { continue };
            let up_price = m["up_price"].as_str().unwrap_or("");
            // Once settled, the winner's price is exactly 1 and the loser's 0.
            if m["closed"].as_bool() == Some(true) && (up_price == "1" || up_price == "0") {
                log.write(POLY_REST_CONN, "resolution", json!({ "start": start, "up_won": up_price == "1" }))?;
                self.resolved.insert(start);
            }
        }
        Ok(())
    }

    /// After the deadline: wait (up to ~8 min) for the last windows to settle,
    /// so a backtest of this session knows how they ended.
    fn final_resolutions(&mut self, log: &mut EventLog) {
        let last_end = self.announced.iter().map(|s| s + WINDOW_S).filter(|&e| e <= now_s()).max();
        let Some(last_end) = last_end else { return };
        let give_up = Instant::now() + Duration::from_secs(480);
        // Settlement takes about 5 minutes after a window ends (measured).
        eprintln!("polymarket: waiting for the last windows to settle (up to 8 min)...");
        while Instant::now() < give_up {
            let pending = self.announced.iter().any(|s| s + WINDOW_S <= last_end && !self.resolved.contains(s));
            if !pending {
                break;
            }
            // Pretend it's later so recently ended windows are asked about too.
            if let Err(e) = self.check_resolutions(log, now_s() + 60) {
                eprintln!("polymarket: {e}");
            }
            thread::sleep(Duration::from_secs(20));
        }
    }
}
