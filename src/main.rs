//! Networking on one thread, state machine on another, connected only
//! through the log file.
//!
//!   network thread:  Kalshi  ──►  logs/<time>.log   (appends events)
//!   main thread:     logs/<time>.log  ──►  sm.update(event)  ──►  prints prices
//!
//! Usage:
//!   cargo run -- live <TICKER> [TICKER...]     writes logs/<unix seconds>.log
//!   cargo run -- replay <log_path>
//!
//! Credentials come from secrets/kalshi.env (override with HFT_SECRETS=path).
//!
//! Replay runs only the main thread's half, over a finished file. Because the
//! state machine depends on nothing but the events, replaying a session must
//! print exactly what the live run printed.

mod book;
mod event;
mod eventlog;
mod fixed;
mod kalshi;
mod sm;
mod tail;
mod ws;

use std::thread;

use serde_json::json;
use tungstenite::Message;

use event::Event;
use eventlog::EventLog;
use kalshi::Credentials;
use sm::StateMachine;
use tail::Tailer;

/// "Any error at all", boxed. `Send + Sync` lets errors cross between threads.
pub type Error = Box<dyn std::error::Error + Send + Sync>;

const USAGE: &str = "usage: hft live <TICKER> [TICKER...] | hft replay <log_path>";

fn main() -> Result<(), Error> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("live") if args.len() > 2 => live(args[2..].to_vec()),
        Some("replay") => replay(args.get(2).ok_or(USAGE)?),
        _ => Err(USAGE.into()),
    }
}

/// Feed every line from the tailer into a fresh state machine.
/// The same function serves live and replay; only the tailer differs.
fn run_state_machine(mut tailer: Tailer) -> Result<(), Error> {
    let mut sm = StateMachine::new();
    while let Some(line) = tailer.next_line()? {
        let ev = Event::parse(&line)?;
        sm.update(&ev);
    }
    Ok(())
}

/// Replay: read a finished log from start to end. No network, no threads.
fn replay(log_path: &str) -> Result<(), Error> {
    run_state_machine(Tailer::open(log_path, false)?)
}

/// Live: network thread writes the log while the main thread follows it.
fn live(tickers: Vec<String>) -> Result<(), Error> {
    let secrets = std::env::var("HFT_SECRETS").unwrap_or_else(|_| "secrets/kalshi.env".into());
    let creds = Credentials::load(&secrets)?;

    let log_path = std::env::var("HFT_LOG").unwrap_or_else(|_| {
        format!("logs/{}.log", eventlog::now_nanos() / 1_000_000_000)
    });
    if let Some(dir) = std::path::Path::new(&log_path).parent() {
        std::fs::create_dir_all(dir)?;
    }
    eprintln!("{:?} environment, logging to {log_path}", creds.env);

    // Create the file BEFORE starting either thread, so the reader can't try
    // to open it before it exists.
    let log = EventLog::create(&log_path)?;

    // `move` gives the closure ownership of `log`, `creds` and `tickers`.
    // Exactly one thread owns the writer, so Rust can prove no two threads
    // write at once.
    thread::spawn(move || {
        if let Err(e) = run_network(log, &creds, &tickers) {
            eprintln!("network thread stopped: {e}");
        }
    });

    run_state_machine(Tailer::open(&log_path, true)?)
}

fn run_network(mut log: EventLog, creds: &Credentials, tickers: &[String]) -> Result<(), Error> {
    log.write(0, "start", json!({ "env": format!("{:?}", creds.env), "tickers": tickers }))?;

    // Sign the websocket upgrade. Reading the clock here is fine: this is
    // the network edge, not the state machine.
    let ts_ms = eventlog::now_nanos() / 1_000_000;
    let headers = creds.auth_headers(ts_ms, "GET", kalshi::WS_PATH);

    let conn = 1; // our first (and for now only) connection
    let mut ws = ws::connect(&mut log, conn, creds.env.ws_host(), kalshi::WS_PATH, &headers)?;

    // One subscription for all requested markets' order books. Kalshi sends
    // a full snapshot of each, then a delta for every change.
    let subscribe = json!({
        "id": 1,
        "cmd": "subscribe",
        "params": { "channels": ["orderbook_delta"], "market_tickers": tickers },
    });
    ws::send_text(&mut ws, &mut log, conn, &subscribe.to_string())?;

    loop {
        match ws.read() {
            Ok(Message::Text(text)) => log.write(conn, "ws_in", json!(text.as_str()))?,
            // Kalshi pings after 10 s without data; tungstenite answers
            // with a pong automatically.
            Ok(Message::Ping(p)) => log.write(conn, "ws_ping", json!(p.len()))?,
            Ok(Message::Pong(p)) => log.write(conn, "ws_pong", json!(p.len()))?,
            Ok(Message::Binary(b)) => log.write(conn, "ws_in_binary", json!(b.len()))?,
            Ok(Message::Close(frame)) => {
                log.write(conn, "ws_close", json!(format!("{frame:?}")))?;
            }
            Ok(Message::Frame(_)) => {} // only produced when writing raw frames
            Err(tungstenite::Error::ConnectionClosed) => {
                log.write(conn, "tcp_close", json!("closed"))?;
                return Ok(());
            }
            Err(e) => {
                log.write(conn, "tcp_close", json!(e.to_string()))?;
                return Err(e.into());
            }
        }
    }
}
