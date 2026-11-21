//! Networking on one thread, state machine on another, connected only
//! through the log file.
//!
//!   network thread:  Hyperliquid  ──►  logs/<time>.log   (appends events)
//!   main thread:     logs/<time>.log  ──►  sm.update(event)  ──►  prints mid
//!
//! Usage:
//!   cargo run -- live [coin] [log_path]   defaults: BTC logs/<unix seconds>.log
//!   cargo run -- replay <log_path>
//!
//! Replay runs only the main thread's half, over a finished file. Because the
//! state machine depends on nothing but the events, replaying a session must
//! print exactly what the live run printed (plan step 3).

mod event;
mod eventlog;
mod sm;
mod tail;
mod ws;

use std::thread;

use serde_json::json;
use tungstenite::Message;

use event::Event;
use eventlog::EventLog;
use sm::StateMachine;
use tail::Tailer;

/// "Any error at all", boxed. `Send + Sync` lets errors cross between threads.
pub type Error = Box<dyn std::error::Error + Send + Sync>;

const HOST: &str = "api.hyperliquid.xyz";
const WS_PATH: &str = "/ws";

const USAGE: &str = "usage: hft live [coin] [log_path] | hft replay <log_path>";

fn main() -> Result<(), Error> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("live") => live(args.get(2).cloned(), args.get(3).cloned()),
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
fn live(coin: Option<String>, log_path: Option<String>) -> Result<(), Error> {
    let coin = coin.unwrap_or_else(|| "BTC".to_string());
    let log_path = match log_path {
        Some(p) => p,
        None => {
            std::fs::create_dir_all("logs")?;
            format!("logs/{}.log", eventlog::now_nanos() / 1_000_000_000)
        }
    };
    eprintln!("logging to {log_path}");

    // Create the file BEFORE starting either thread, so the reader can't try
    // to open it before it exists.
    let log = EventLog::create(&log_path)?;

    // `move` gives the closure ownership of `log` and `coin`. Exactly one
    // thread owns the writer, so Rust can prove no two threads write at once.
    thread::spawn(move || {
        if let Err(e) = run_network(log, &coin) {
            eprintln!("network thread stopped: {e}");
        }
    });

    run_state_machine(Tailer::open(&log_path, true)?)
}

/// Step 1's code, unchanged except that it now runs on its own thread.
fn run_network(mut log: EventLog, coin: &str) -> Result<(), Error> {
    log.write(0, "start", json!({ "coin": coin }))?;

    let conn = 1; // our first (and for now only) connection
    let mut ws = ws::connect(&mut log, conn, HOST, WS_PATH)?;

    // Hyperliquid subscriptions:
    //   l2Book  full snapshot of the book's depth, but only every ~12 s
    //   bbo     best bid/ask, sent whenever they change (~every 100 ms)
    //   trades  every public trade
    for sub in ["l2Book", "bbo", "trades"] {
        let msg = json!({ "method": "subscribe", "subscription": { "type": sub, "coin": coin } });
        ws::send_text(&mut ws, &mut log, conn, &msg.to_string())?;
    }

    loop {
        match ws.read() {
            Ok(Message::Text(text)) => log.write(conn, "ws_in", json!(text.as_str()))?,
            // tungstenite answers pings with pongs automatically.
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
