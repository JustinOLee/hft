//! The lab: a playground for prediction ideas. Real data, paper money.
//!
//!   hft lab record [--minutes N] [DIR]      save live public data to DIR
//!   hft lab backtest [-v] DIR CONFIG...     run each config over DIR
//!
//! The pieces, in the order data flows through them:
//!
//!   feeds.rs     record Coinbase BTC + Polymarket 5-minute books to files
//!   merge.rs     read a session's files back as one time-ordered stream
//!   sim.rs       the deterministic state machine that runs a backtest
//!     vol.rs       how jumpy BTC is right now
//!     model.rs     fair value: P(BTC ends the window up)
//!     pmbook.rs    the Polymarket order books
//!     strategy.rs  look at it all, decide what to buy
//!     paper.rs     fill orders against the real book, with fake money
//!   config.rs    every lever, loaded from a JSON file (see lab/configs/)
//!
//! See docs/LAB.md for the plan and docs/STRATEGIES.md for the ideas.

pub mod config;
pub mod feeds;
pub mod merge;
pub mod model;
pub mod paper;
pub mod pmbook;
pub mod sim;
pub mod strategy;
pub mod vol;

use crate::eventlog::now_nanos;
use crate::queue::EventReader;
use crate::Error;

const USAGE: &str = "usage: hft lab record [--minutes N] [DIR]\n       hft lab backtest [-v] DIR CONFIG.json...";

pub fn cli(args: &[String]) -> Result<(), Error> {
    match args.first().map(String::as_str) {
        Some("record") => {
            let mut minutes = None;
            let mut dir = None;
            let mut it = args[1..].iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--minutes" => minutes = Some(it.next().ok_or(USAGE)?.parse()?),
                    d => dir = Some(d.to_string()),
                }
            }
            let dir = dir.unwrap_or_else(|| format!("data/{}", now_nanos() / 1_000_000_000));
            feeds::record(&dir, minutes)
        }
        Some("backtest") => {
            let verbose = args.iter().any(|a| a == "-v");
            let rest: Vec<&String> = args[1..].iter().filter(|a| *a != "-v").collect();
            let (dir, configs) = rest.split_first().ok_or(USAGE)?;
            if configs.is_empty() {
                return Err(USAGE.into());
            }
            backtest(dir, configs, verbose)
        }
        _ => Err(USAGE.into()),
    }
}

fn backtest(dir: &str, configs: &[&String], verbose: bool) -> Result<(), Error> {
    let mut reports = Vec::new();
    for path in configs {
        let mut cfg = config::Config::load(path)?;
        cfg.verbose |= verbose;
        let mut sim = sim::Sim::new(cfg)?;
        // Each config re-reads the session from disk, from the start.
        let mut events = merge::MergedReader::open_dir(dir)?;
        while let Some(ev) = events.next_event()? {
            sim.update(&ev);
        }
        reports.push(sim.finish());
    }

    let r0 = &reports[0];
    println!(
        "session {dir}: {} windows with results ({} official), Coinbase proxy matched official {}/{}\n",
        r0.windows, r0.official, r0.proxy_agreed, r0.proxy_checked
    );
    println!("{}", sim::Report::header());
    for r in &reports {
        println!("{}", r.row());
        if r.unsettled > 0 {
            println!("{:<22} ({} window(s) with shares couldn't be settled: no result)", "", r.unsettled);
        }
    }
    println!("\nbrier: 0 = perfect, 0.25 = coin flip. 'brier' is our model, 'market' is the Up mid price.");
    Ok(())
}
