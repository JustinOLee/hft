# The lab: real data, paper money

The lab is a playground for testing prediction ideas on **real market data**
with **fake money**. It's our version of plan step 15 and beyond.

The first market is Polymarket's **"Bitcoin Up or Down"**, a new one every
5 minutes:

> Will Bitcoin's price at the end of this 5-minute window be at or above
> its price at the start?

It's a good first playground: it repeats 288 times a day (lots of
results to learn from), it has one clear driver (the BTC price), and the
data is public, so there are no keys and no risk.

> **About Polymarket and the US:** Polymarket's main exchange doesn't
> allow US residents to trade. The lab only **reads public data** and never
> trades there. For real money later, Kalshi lists a similar market,
> `KXBTC15M` ("BTC up or down in 15 minutes"), which is legal here and
> which our trading core already speaks.

For the strategy ideas themselves, see [STRATEGIES.md](STRATEGIES.md).

---

## How it works

```
      LIVE (record)                         OFFLINE (backtest)
                                                  
  Coinbase ws ──► coinbase.log ─┐        ┌─► sim ──► strategy ──► intents
                                ├─ data/ ┤     ▲                     │
  Polymarket ws ─► polymarket.log┘ <dir> └─ merge                    ▼
  (+ which market is which,                     paper account: fills against
     + how each one ended)                       the recorded book, fees,
                                                 settlement ──► report
```

1. **Record** once: `hft lab record --minutes 60`. It saves everything to a
   session folder, `data/<time>/`.
2. **Backtest** as often as you like: `hft lab backtest data/<time>
   lab/configs/*.json`. Every config runs on exactly the same data, and the
   same config always gives the same answer, because the simulator follows
   the same rules as the trading state machine (no clock, no randomness).
   The session is read once and fed to every config, so sweeping many
   configs costs little more than one. To split train/test, set
   `LAB_FIRST_WINDOW` / `LAB_LAST_WINDOW` (window start times), or use
   `research/sweep.py`, which builds a grid of configs and reports a
   train column and a test column from one pass.
3. **Export** for analysis: `hft lab export DIR CONFIG > out.csv` writes
   one row per evaluation (model, spot, both books' tops).
4. **History**: `research/fetch_history.py` downloads 30+ days of
   minute-level prices and results; `research/analyze.py` runs model
   experiments on them. Results: [RESEARCH.md](RESEARCH.md).

Recordings are gzip-compressed (`*.log.gz`, ~350 MB/hour).

### What a backtest does, second by second

- **Spot price:** the Coinbase BTC-USD mid price.
- **Reference price:** the average spot price over the minute *before*
  the window began. (The market settles on last-minute average vs this;
  found from data, RESEARCH.md E2.) A window we joined late has no
  reference, so it's skipped.
- **Volatility:** how jumpy BTC is now (`vol.rs`).
- **Fair value:** the model's chance that Up wins (`model.rs`).
- **Order books:** the Polymarket Up and Down books (`pmbook.rs`).
- **Strategy:** sees all of the above, plus its own holdings, and returns
  "buy N shares of Up/Down at up to P" (`strategy.rs`).
- **Fill:** after `latency_ms`, the order fills against the book *as it
  was then*, buying from the cheapest seller up, never more than was really
  offered, minus fees (`paper.rs`).
- **Settle:** when the window's official result arrives, each winning share
  pays $1.

### The report

```
config                     pnl $  fills  fees $  windows  traded   brier  market forecasts
fair_value                 +3.10     14    0.31        4       3  0.1810  0.1502       950
always_up                  -1.20      4    0.07        4       4  0.1810  0.1502       950
```

(The numbers above are only an example. Our first real 25-minute session
is in [RESEARCH.md](RESEARCH.md).)

- **pnl $**: profit or loss in paper dollars.
- **windows / traded**: how many windows had a known result, and how many
  we traded in.
- **brier / market**: how good the forecasts were. `brier` is our model,
  `market` is the Up price itself, scored at the same moments. Lower is
  better, and 0.25 is a coin flip. See STRATEGIES.md.

The header line also says how often **"Coinbase close ≥ open"** agreed
with the **official** result. The market settles on Chainlink's price, not
Coinbase's, and when the move is tiny the two can disagree. That gap is
called **basis**.

---

## The levers

All in a JSON file (see `lab/configs/`). Anything left out keeps its default.

| Lever | Default | What it does | Try |
|---|---|---|---|
| `strategy` | `fair_value_taker` | which strategy | `fair_value_maker`, `always_up`, `none` |
| `spot_source` | `coinbase` | BTC price from `coinbase`, `kraken`, or `mean` of both | `mean` (closer to Chainlink, RESEARCH.md E12) |
| `maker_fee_rate` | 0 | fee for resting orders (Polymarket: 0, plus rebates we ignore) | |
| `budget` | 1000 | paper dollars to start | 100, unlimited-ish 1e9 |
| `fee_rate` | 0.05 | fee = rate × shares × p × (1−p); Polymarket's official taker rate for these markets (makers pay 0) | 0 to see the pre-fee edge; 0.07 = Kalshi |
| `edge` | 0.05 | buy only if fair value − price ≥ this | 0.01 … 0.15 |
| `shares` | 10 | shares per order | |
| `max_shares` | 100 | most shares of one side per window | |
| `no_trade_last_s` | 10 | stop trading this close to the end | 0, 30, 60 |
| `model` | `avg60` | settlement rule: `avg60` (real) or `point` (end vs start price) | compare both |
| `vol_halflife_s` | 1800 | how fast the volatility estimate reacts | 60, 300, 7200 |
| `vol_prior` | 1e-4 | starting volatility guess per √second (~50%/year) | |
| `latency_ms` | 100 | pretend order travel time | 0, 500, 2000 |
| `eval_ms` | 1000 | how often the strategy is asked | 100, 5000 |
| `max_spot_age_ms` | 5000 | sit out (and cancel resting orders) if the BTC price is older than this | |
| `first_window` / `last_window` | 0 | only trade/score windows in this range (unix s; 0 = all) | train/test split |
| `verbose` | false | print every decision and fill (or pass `-v`) | |

Good first experiments:

1. **Is there any edge before fees?** Run `fair_value` with `fee_rate: 0`,
   then with the default. How much of the profit was fees?
2. **Does speed matter?** Same config with `latency_ms` 0, 100, 500, 2000.
   If profit falls fast as latency grows, you've found a speed game.
3. **Model vs market:** does `brier` beat `market`? On which windows?
4. **Baseline:** does `fair_value` beat `always_up`? If not, it isn't smart.

---

## The plan, in stages

Each stage is small, and each ends with something you can run.

### Stage 0: the skeleton ✅ (this commit)

- [x] recorder for Coinbase BTC + Polymarket 5-minute books, results included
- [x] merged, time-ordered, deterministic replay of a session folder
- [x] random-walk fair value model + volatility estimate
- [x] paper account: taker fills against the real book, fees, settlement
- [x] three strategies: `none`, `always_up` (baseline), `fair_value_taker`
- [x] config files for the levers, and a results table with Brier scores

### Stage 1: look at the data ✅

- [x] `hft lab export DIR CONFIG` → CSV, one row per evaluation
- [x] calibration tables (RESEARCH.md E3, E11)
- [x] gzip recordings, read directly (~350 MB/hour instead of ~3 GB)
- [x] 30 days of minute-level history for model research
- [ ] charts: plot a window (spot, reference, model, market) as a page

### Stage 2: honest testing (partly done)

- [x] train/test split by date in the history research; window ranges
      (`first_window`/`last_window`) for recordings
- [x] one-pass sweeps: many configs over one read of a session
- [ ] **Train/test split:** choose settings on one session, report results
      on another. The `sweep` should try a grid of levers and show the best
      *on the training data* next to how it did *on the test data*.
- [ ] Confidence intervals on pnl (how big is the luck?).

### Stage 3: a better model (partly done)

- [x] Settlement on one-minute **averages** (found from data: RESEARCH.md
      E2), now the default model (`avg60`)
- [ ] **The real settlement price.** Polymarket's live data socket carries
      Chainlink prices, but needs a Polymarket account plus a Chainlink API
      key (not available to US residents). Alternatives: approximate
      Chainlink with a multi-exchange average.
- [ ] Fat tails (a t-distribution instead of the normal), and
      time-of-day volatility.

### Stage 4: signals

Each new signal is a new field in `View`, a measurement of how well it
predicts, and only then a strategy that uses it.

- [ ] **Lead-lag:** how many milliseconds after a Coinbase move does the
      Polymarket price follow? (cross-correlation at different delays)
- [ ] Book imbalance and recent trades (`last_trade_price` messages are
      already recorded but not used).
- [x] A second spot feed (Kraken), `spot_source: mean`
- [ ] More spot feeds (Bitstamp, Binance.US) to get closer to Chainlink and
      the CF Benchmarks index Kalshi uses.

### Stage 5: resting orders and market making (started)

- [x] Simulate maker orders (`fair_value_maker`): a bid 1¢ above the best
      bid, so nobody is ahead of us in line. It fills when a taker sells
      Up at or below it, when a taker buys Down at or above 1 − bid
      (Polymarket pairs those), or when someone offers at or below it.
      Placing and canceling both take `latency_ms`, so stale quotes can be
      picked off. Post-only: an order that would cross on arrival is
      rejected.
- [ ] Joining the queue at the best bid (needs a queue-position guess)
- [ ] The step 10 skew when inventory builds up
- [ ] Measure adverse selection: what happens to the price right after
      each of our fills?

### Stage 6: Kalshi and live paper trading

- [ ] Record Kalshi `KXBTC15M` (15-minute windows) with the trading core's
      feed. Same model, different window length and settlement index.
- [ ] Compare venues: do Polymarket's three 5-minute windows and Kalshi's
      15-minute window agree?
- [ ] **Live paper mode:** run the sim while recording, on live data, and
      watch decisions as they happen. (The merge needs to wait for every
      feed to catch up, so it never reads events out of order.)

### Stage 7: graduate (maybe)

If something survives stages 2–6 on Kalshi data, run it with real cents
on Kalshi using the trading core (plan steps 10–13), and reconcile every
real fill against what the paper account predicted (plan step 14).
