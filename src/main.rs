//! Networking on one thread, state machine on another.
//!
//!   network thread:  Kalshi  ──►  logs/<time>.log   (appends events)
//!   main thread:     logs/<time>.log  ──►  sm.update(event)  ──►  actions
//!   actions:         main thread  ──(channel)──►  network thread  ──►  Kalshi
//!
//! Usage:
//!   cargo run -- live [--test-resync] [--place-cancel] <TICKER> [TICKER...]
//!   cargo run -- replay [--check-actions] [--place-cancel] <log_path>
//!   cargo run -- balance            (one signed REST request, for testing)
//!
//! Credentials come from secrets/kalshi.env (override with HFT_SECRETS=path).
//! The log goes to logs/<unix seconds>.log (override with HFT_LOG=path).
//!
//! Replay runs only the main thread's half, over a finished file. Because the
//! state machine depends on nothing but the events, replaying a session must
//! print exactly what the live run printed, actions included.

mod action;
mod book;
mod event;
mod eventlog;
mod fixed;
mod http;
mod kalshi;
mod net;
mod sm;
mod tail;
mod ws;

use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::Duration;

use serde_json::json;
use tungstenite::Message;

use action::Action;
use event::Event;
use eventlog::EventLog;
use kalshi::Credentials;
use sm::{StateMachine, Strategy, REST_CONN, WS_CONN};
use tail::Tailer;

/// "Any error at all", boxed. `Send + Sync` lets errors cross between threads.
pub type Error = Box<dyn std::error::Error + Send + Sync>;

const USAGE: &str = "usage: hft live [--test-resync] [--place-cancel] <TICKER> [TICKER...]\n       \
                     hft replay [--check-actions] [--place-cancel] <log_path>\n       hft balance";

/// Session settings chosen on the command line.
struct Options {
    tickers: Vec<String>,
    test_resync: bool,
    place_cancel: bool,
}

fn main() -> Result<(), Error> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("live") => {
            let opts = Options {
                tickers: args[1..].iter().filter(|a| !a.starts_with("--")).cloned().collect(),
                test_resync: args.iter().any(|a| a == "--test-resync"),
                place_cancel: args.iter().any(|a| a == "--place-cancel"),
            };
            if opts.tickers.is_empty() {
                return Err(USAGE.into());
            }
            live(opts)
        }
        Some("replay") => {
            let check = args.iter().any(|a| a == "--check-actions");
            // Try the strategy on a log recorded without it.
            let strategy = args.iter().any(|a| a == "--place-cancel").then_some(Strategy::PlaceCancel);
            let path = args[1..].iter().find(|a| !a.starts_with("--")).ok_or(USAGE)?;
            replay(path, check, strategy)
        }
        Some("balance") => balance(),
        _ => Err(USAGE.into()),
    }
}

/// What a run of the state machine produced.
struct RunResult {
    /// Actions the state machine returned, in order.
    emitted: Vec<Action>,
    /// Actions found in the log (written by the network thread), in order.
    logged: Vec<Action>,
}

/// Feed every line from the tailer into a fresh state machine, printing the
/// actions it returns and (when live) sending them to the network thread.
/// The same function serves live and replay; only the tailer differs.
/// Stops at end of file (replay) or when the strategy is finished.
fn run_state_machine(
    mut tailer: Tailer,
    actions_tx: Option<Sender<Action>>,
    strategy_override: Option<Strategy>,
) -> Result<RunResult, Error> {
    let mut sm = match strategy_override {
        Some(s) => StateMachine::with_strategy_override(s),
        None => StateMachine::new(),
    };
    let mut result = RunResult { emitted: Vec::new(), logged: Vec::new() };
    while let Some(line) = tailer.next_line()? {
        let ev = Event::parse(&line)?;
        if let event::Kind::Action(a) = &ev.kind {
            result.logged.push(a.clone());
        }
        for action in sm.update(&ev) {
            println!("{} ACTION {}", sm.now, serde_json::to_string(&action)?);
            result.emitted.push(action.clone());
            if let Some(tx) = &actions_tx {
                if tx.send(action).is_err() {
                    eprintln!("{} WARN network thread is gone; action dropped", sm.now);
                }
            }
        }
        if sm.finished() {
            break;
        }
    }
    Ok(result)
}

/// Replay: read a finished log from start to end. No network, no threads.
/// Actions are printed, never sent.
///
/// With `check_actions`, also compare the actions this replay produced with
/// the ones the live run logged. They must be identical (plan step 5).
fn replay(log_path: &str, check_actions: bool, strategy: Option<Strategy>) -> Result<(), Error> {
    let r = run_state_machine(Tailer::open(log_path, false)?, None, strategy)?;
    if !check_actions {
        return Ok(());
    }
    // A live run may stop before the network thread logs the last actions
    // it was sent, so the logged list can be a prefix of the emitted one.
    let n = r.logged.len();
    if r.emitted.len() >= n && r.emitted[..n] == r.logged[..] {
        eprintln!("PASS: replay produced the same {n} action(s) the live run logged");
        if r.emitted.len() > n {
            eprintln!("      (plus {} the live run hadn't logged yet when it stopped)", r.emitted.len() - n);
        }
        Ok(())
    } else {
        eprintln!("FAIL: actions differ");
        eprintln!("  logged:  {:?}", r.logged);
        eprintln!("  replay:  {:?}", r.emitted);
        Err("replay actions don't match the log".into())
    }
}

fn load_credentials() -> Result<Credentials, Error> {
    let path = std::env::var("HFT_SECRETS").unwrap_or_else(|_| "secrets/kalshi.env".into());
    Credentials::load(&path)
}

fn new_log(prefix: &str) -> Result<(EventLog, String), Error> {
    let path = std::env::var("HFT_LOG")
        .unwrap_or_else(|_| format!("logs/{prefix}{}.log", eventlog::now_nanos() / 1_000_000_000));
    if let Some(dir) = std::path::Path::new(&path).parent() {
        std::fs::create_dir_all(dir)?;
    }
    Ok((EventLog::create(&path)?, path))
}

/// Live: network thread writes the log while the main thread follows it.
fn live(opts: Options) -> Result<(), Error> {
    let creds = load_credentials()?;
    // Create the file BEFORE starting either thread, so the reader can't try
    // to open it before it exists.
    let (log, log_path) = new_log("")?;
    eprintln!("{:?} environment, logging to {log_path}", creds.env);

    // A channel is a queue between threads: `tx` sends, `rx` receives.
    let (tx, rx) = mpsc::channel::<Action>();

    // `move` gives the closure ownership of everything it uses. Exactly one
    // thread owns the log writer, so Rust can prove no two threads write at
    // once.
    thread::spawn(move || {
        if let Err(e) = run_network(log, &creds, &opts, rx) {
            eprintln!("network thread stopped: {e}");
        }
    });

    run_state_machine(Tailer::open(&log_path, true)?, Some(tx), None)?;
    // Give the network thread a moment to log the last actions it was sent.
    thread::sleep(Duration::from_millis(500));
    Ok(())
}

fn run_network(mut log: EventLog, creds: &Credentials, opts: &Options, actions: Receiver<Action>) -> Result<(), Error> {
    let tickers = &opts.tickers;
    // Everything the state machine needs to know about this session goes in
    // the first event, so replay sees exactly the same settings.
    let start = json!({
        "env": format!("{:?}", creds.env),
        "tickers": tickers,
        "test_resync": opts.test_resync,
        "strategy": if opts.place_cancel { "place_cancel" } else { "none" },
    });
    log.write(0, "start", start)?;

    // Sign the websocket upgrade. Reading the clock here is fine: this is
    // the network edge, not the state machine.
    let headers = creds.auth_headers(now_ms(), "GET", kalshi::WS_PATH);
    let mut ws = ws::connect(&mut log, WS_CONN, creds.env.ws_host(), kalshi::WS_PATH, &headers)?;

    // Public order books for our markets, plus two private channels about
    // our own orders: status changes (user_orders) and trades (fill).
    let subscriptions = [
        json!({ "channels": ["orderbook_delta"], "market_tickers": tickers }),
        json!({ "channels": ["user_orders"] }),
        json!({ "channels": ["fill"] }),
    ];
    // Kalshi wants a unique id on every command we send on a connection.
    let mut next_id = 1;
    for params in subscriptions {
        let cmd = json!({ "id": next_id, "cmd": "subscribe", "params": params });
        next_id += 1;
        ws::send_text(&mut ws, &mut log, WS_CONN, &cmd.to_string())?;
    }

    // Don't block on the websocket forever: we also need to check for actions.
    ws::set_read_timeout(&mut ws, Duration::from_millis(5))?;
    let mut rest = http::Client::new(creds.env.rest_host(), REST_CONN);

    loop {
        // 1. Anything from Kalshi? (waits at most 5 ms)
        match ws.read() {
            Ok(Message::Text(text)) => log.write(WS_CONN, "ws_in", json!(text.as_str()))?,
            // Kalshi pings after 10 s without data; tungstenite answers
            // with a pong automatically.
            Ok(Message::Ping(p)) => log.write(WS_CONN, "ws_ping", json!(p.len()))?,
            Ok(Message::Pong(p)) => log.write(WS_CONN, "ws_pong", json!(p.len()))?,
            Ok(Message::Binary(b)) => log.write(WS_CONN, "ws_in_binary", json!(b.len()))?,
            Ok(Message::Close(frame)) => log.write(WS_CONN, "ws_close", json!(format!("{frame:?}")))?,
            Ok(Message::Frame(_)) => {} // only produced when writing raw frames
            Err(e) if ws::is_timeout(&e) => {} // nothing arrived: fine
            Err(tungstenite::Error::ConnectionClosed) => {
                log.write(WS_CONN, "tcp_close", json!("closed"))?;
                return Ok(());
            }
            Err(e) => {
                log.write(WS_CONN, "tcp_close", json!(e.to_string()))?;
                return Err(e.into());
            }
        }

        // 2. Anything the state machine wants done?
        loop {
            match actions.try_recv() {
                Ok(action) => execute(&mut log, creds, &mut ws, &mut rest, &mut next_id, &action)?,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Ok(()), // state machine finished
            }
        }
    }
}

/// Carry out one action: log it first, then do it.
fn execute(
    log: &mut EventLog,
    creds: &Credentials,
    ws: &mut ws::Ws,
    rest: &mut http::Client,
    next_id: &mut u64,
    action: &Action,
) -> Result<(), Error> {
    log.write(0, "action", serde_json::to_value(action)?)?;

    if let Action::Resync { sid, tickers } = action {
        let cmd = json!({
            "id": *next_id,
            "cmd": "update_subscription",
            "params": { "sids": [sid], "market_tickers": tickers, "action": "get_snapshot" },
        });
        *next_id += 1;
        return ws::send_text(ws, log, WS_CONN, &cmd.to_string());
    }

    let Some(call) = kalshi::rest_call(action) else { return Ok(()) };
    // Every REST request is signed separately. The query string isn't signed.
    let headers = creds.auth_headers(now_ms(), call.method, &call.path);
    let path = format!("{}{}", call.path, call.query);
    // NOTE: this blocks the websocket for one round trip (~tens of ms).
    // Acceptable for now; step 7's event loop fixes it.
    if let Err(e) = rest.request(log, call.method, &path, &headers, call.body.as_deref()) {
        // The state machine must hear about every REST action exactly once:
        // either a response body or this.
        log.write(REST_CONN, "http_error", json!(e.to_string()))?;
    }
    Ok(())
}

/// One signed GET, to check REST signing and HTTP parsing with no risk.
fn balance() -> Result<(), Error> {
    let creds = load_credentials()?;
    let (mut log, log_path) = new_log("balance-")?;
    log.write(0, "start", json!({ "env": format!("{:?}", creds.env), "tickers": [] }))?;
    let call = kalshi::balance_call();
    let headers = creds.auth_headers(now_ms(), call.method, &call.path);
    let mut client = http::Client::new(creds.env.rest_host(), REST_CONN);
    let resp = client.request(&mut log, call.method, &call.path, &headers, None)?;
    println!("HTTP {}: {}", resp.status, resp.body_text());
    eprintln!("(logged to {log_path})");
    Ok(())
}

fn now_ms() -> u64 {
    eventlog::now_nanos() / 1_000_000
}
