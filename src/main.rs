//! Networking on one thread, state machine on another.
//!
//!   network thread:  Kalshi  ──►  logs/<time>.log   (appends events)
//!   main thread:     logs/<time>.log  ──►  sm.update(event)  ──►  actions
//!   actions:         main thread  ──(channel + wake)──►  network thread  ──►  Kalshi
//!
//! The network thread is an event loop ("reactor", see reactor.rs).
//!
//! Usage:
//!   cargo run -- live [--test-resync] [--place-cancel] [--test-disconnect] <TICKER>...
//!   cargo run -- replay [--check-actions] [--place-cancel] <log_path>
//!   cargo run -- balance            (one signed REST request, for testing)
//!   cargo run -- lab ...            the paper-trading playground (src/lab/)
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
mod lab;
mod net;
mod queue;
mod reactor;
mod sm;
mod tail;

use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use serde_json::json;

use action::Action;
use eventlog::EventLog;
use kalshi::Credentials;
use sm::{StateMachine, Strategy, REST_CONN};
use queue::{EventReader, FileReader};

/// "Any error at all", boxed. `Send + Sync` lets errors cross between threads.
pub type Error = Box<dyn std::error::Error + Send + Sync>;

const USAGE: &str = "usage: hft live [--test-resync] [--place-cancel] [--test-disconnect] <TICKER>...\n       \
                     hft replay [--check-actions] [--place-cancel] <log_path>\n       hft balance\n       \
                     hft lab record|backtest ... (see src/lab/mod.rs)";

/// Session settings chosen on the command line.
struct Options {
    tickers: Vec<String>,
    test_resync: bool,
    place_cancel: bool,
    /// Testing aid: force one websocket disconnect this long after it opens.
    test_disconnect: Option<Duration>,
}

fn main() -> Result<(), Error> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("live") => {
            let opts = Options {
                tickers: args[1..].iter().filter(|a| !a.starts_with("--")).cloned().collect(),
                test_resync: args.iter().any(|a| a == "--test-resync"),
                place_cancel: args.iter().any(|a| a == "--place-cancel"),
                test_disconnect: args.iter().any(|a| a == "--test-disconnect").then(|| Duration::from_secs(10)),
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
        Some("lab") => lab::cli(&args[1..]),
        _ => Err(USAGE.into()),
    }
}

/// What a run of the state machine produced.
struct RunResult {
    /// Actions the state machine returned, in order, with the timestamp of
    /// the event that triggered each one.
    emitted: Vec<(u64, Action)>,
    /// Actions found in the log (written by the network thread), in order,
    /// with the time the network thread received each one.
    logged: Vec<(u64, Action)>,
}

/// Feed every line from the tailer into a fresh state machine, printing the
/// actions it returns and (when live) sending them to the network thread.
/// The same function serves live and replay; only the tailer differs.
/// Stops at end of file (replay) or when the strategy is finished.
fn run_state_machine(
    mut events: impl EventReader,
    actions_tx: Option<ActionSink>,
    strategy_override: Option<Strategy>,
) -> Result<RunResult, Error> {
    let mut sm = match strategy_override {
        Some(s) => StateMachine::with_strategy_override(s),
        None => StateMachine::new(),
    };
    let mut result = RunResult { emitted: Vec::new(), logged: Vec::new() };
    while let Some(ev) = events.next_event()? {
        if let event::Kind::Action(a) = &ev.kind {
            result.logged.push((ev.ts, a.clone()));
        }
        for action in sm.update(&ev) {
            println!("{} ACTION {}", sm.now, serde_json::to_string(&action)?);
            result.emitted.push((sm.now, action.clone()));
            if let Some(sink) = &actions_tx {
                if let Err(e) = sink.send(action) {
                    eprintln!("{} WARN action dropped: {e}", sm.now);
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
    let r = run_state_machine(FileReader::open(log_path, false)?, None, strategy)?;
    if !check_actions {
        return Ok(());
    }
    // A live run may stop before the network thread logs the last actions
    // it was sent, so the logged list can be a prefix of the emitted one.
    let n = r.logged.len();
    let same = r.emitted.len() >= n && r.emitted.iter().zip(&r.logged).all(|((_, a), (_, b))| a == b);
    if same {
        eprintln!("PASS: replay produced the same {n} action(s) the live run logged");
        if r.emitted.len() > n {
            eprintln!("      (plus {} the live run hadn't logged yet when it stopped)", r.emitted.len() - n);
        }
        // How long each action took from "the event that caused it arrived"
        // to "the network thread has it": our own internal delay.
        for ((decided, a), (logged, _)) in r.emitted.iter().zip(&r.logged) {
            let kind = serde_json::to_value(a)?["action"].as_str().unwrap_or("?").to_string();
            eprintln!("      {kind:<7} reached the network thread {} us after its trigger event", (logged - decided) / 1000);
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

/// The action queue (state machine -> network thread): an in-memory channel,
/// plus a wake-up so the reactor doesn't wait for its next timeout before
/// noticing. Actions still reach the log, because the reactor writes each
/// one down before carrying it out.
struct ActionSink {
    tx: Sender<Action>,
    waker: Arc<mio::Waker>,
}

impl ActionSink {
    fn send(&self, action: Action) -> Result<(), Error> {
        self.tx.send(action).map_err(|_| "network thread is gone")?;
        self.waker.wake()?;
        Ok(())
    }
}

/// Live: network thread writes the log while the main thread follows it.
fn live(opts: Options) -> Result<(), Error> {
    let creds = load_credentials()?;
    // Create the file BEFORE starting either thread, so the reader can't try
    // to open it before it exists.
    let (mut log, log_path) = new_log("")?;
    eprintln!("{:?} environment, logging to {log_path}", creds.env);

    // Everything the state machine needs to know about this session goes in
    // the first event, so replay sees exactly the same settings.
    let start = json!({
        "env": format!("{:?}", creds.env),
        "tickers": opts.tickers,
        "test_resync": opts.test_resync,
        "strategy": if opts.place_cancel { "place_cancel" } else { "none" },
    });
    log.write(0, "start", start)?;

    // A channel is a queue between threads: `tx` sends, `rx` receives.
    let (tx, rx) = mpsc::channel::<Action>();
    let (reactor, waker) = reactor::Reactor::new(Box::new(log), creds, &opts.tickers, rx, opts.test_disconnect)?;

    // `move` gives the closure ownership of everything it uses. Exactly one
    // thread owns the log writer, so Rust can prove no two threads write at
    // once.
    thread::spawn(move || {
        if let Err(e) = reactor.run() {
            eprintln!("network thread stopped: {e}");
        }
    });

    let sink = ActionSink { tx, waker };
    run_state_machine(FileReader::open(&log_path, true)?, Some(sink), None)?;
    // Give the network thread a moment to log the last actions it was sent.
    thread::sleep(Duration::from_millis(500));
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
