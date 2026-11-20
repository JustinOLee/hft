//! Plan step 2: networking on one thread, state machine on another,
//! connected only through the log file.
//!
//!   network thread:  Hyperliquid  ──►  logs/<time>.log   (appends events)
//!   main thread:     logs/<time>.log  ──►  sm.update(event)  ──►  prints mid
//!
//! Usage: cargo run -- [coin] [log_path]
//!   defaults: BTC logs/<unix seconds>.log

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

fn main() -> Result<(), Error> {
    let args: Vec<String> = std::env::args().collect();
    let coin = args.get(1).cloned().unwrap_or_else(|| "BTC".to_string());
    let log_path = match args.get(2) {
        Some(p) => p.clone(),
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

    let mut tailer = Tailer::open(&log_path, true)?;
    let mut sm = StateMachine::new();
    while let Some(line) = tailer.next_line()? {
        let ev = Event::parse(&line)?;
        sm.update(&ev);
    }
    Ok(())
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
