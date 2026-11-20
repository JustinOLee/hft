//! Plan step 1: connect to Hyperliquid and log all traffic to a file.
//!
//! Usage: cargo run -- [log_path] [coin]
//!   defaults: capture.log BTC
//!
//! Everything runs on one thread with blocking I/O: no async, no threads.

mod eventlog;
mod ws;

use serde_json::json;
use tungstenite::Message;

use eventlog::EventLog;

/// "Any error at all", boxed. Fine for now; we'll get more precise later.
pub type Error = Box<dyn std::error::Error>;

const HOST: &str = "api.hyperliquid.xyz";
const WS_PATH: &str = "/ws";

fn main() -> Result<(), Error> {
    let args: Vec<String> = std::env::args().collect();
    let log_path = args.get(1).map(String::as_str).unwrap_or("capture.log");
    let coin = args.get(2).map(String::as_str).unwrap_or("BTC");

    let mut log = EventLog::open(log_path)?;
    log.write(0, "start", json!({ "coin": coin }))?;
    eprintln!("logging to {log_path}");

    let conn = 1; // our first (and for now only) connection
    let mut ws = ws::connect(&mut log, conn, HOST, WS_PATH)?;

    // Hyperliquid subscriptions: the full order book (top 20 levels per side,
    // re-sent on every change) and the stream of public trades.
    for sub in ["l2Book", "trades"] {
        let msg = json!({ "method": "subscribe", "subscription": { "type": sub, "coin": coin } });
        ws::send_text(&mut ws, &mut log, conn, &msg.to_string())?;
    }

    // Blocking read loop: `ws.read()` waits until a whole websocket message has
    // arrived (however many TCP reads that takes), then returns it.
    let mut count: u64 = 0;
    loop {
        match ws.read() {
            Ok(Message::Text(text)) => {
                log.write(conn, "ws_in", json!(text.as_str()))?;
                count += 1;
                if count % 100 == 0 {
                    eprintln!("{count} messages");
                }
            }
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
