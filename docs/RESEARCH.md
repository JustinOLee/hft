# Research journal: what we've found

Everything here is about Polymarket's 5-minute "Bitcoin Up or Down"
markets. The ideas behind each experiment are in
[STRATEGIES.md](STRATEGIES.md); the tools are described in [LAB.md](LAB.md).

**The short version so far:**

1. **Read the fine print with data.** The market doesn't settle on "end
   price vs start price". It compares the **average over the last minute**
   with the **average over the minute before the window started**. Fixing
   just that cut our model's gap to the market in half. No other change
   came close.
2. **The market is very good.** Its prices are well calibrated, and our
   model adds nothing to the market's own price (on minute-level data).
3. **Most of the remaining gap is timing.** With a fresh BTC price, the
   model is nearly as good as the market. To beat it you'd need
   information *before* the market has it, which is a speed race.
4. **Simple biases didn't survive testing.** One looked profitable on the
   training days and lost money on the test days.
5. No strategy so far makes money after fees. That's the expected result,
   and it's the honest baseline to build on.

---

## What the papers say

A short reading list, with what each means for us.

| Topic | Finding | What it means here |
|---|---|---|
| Plan step 15's paper: *Fragmentation, Price Formation, and Cross-Impact in Bitcoin Markets* ([arXiv 2108.09750](https://arxiv.org/pdf/2108.09750)) | BTC trades on many exchanges at once; order flow on one moves prices on others ("cross-impact"), and the biggest venues (Binance, Huobi) do most of the price discovery | Coinbase alone is only part of the picture. Adding Binance (or futures) to the recorder is a natural next signal |
| Order flow imbalance (Cont, Kukanov, Stoikov 2014, *The price impact of order book events*) | Price changes are roughly proportional to the imbalance between buy and sell orders, mostly at the *same* time rather than in advance | Our X3 test: does the Up book's imbalance predict its next move? |
| Price discovery between venues ([Plazuelo Pascual et al. 2025](https://arxiv.org/abs/2506.08718); Frino et al. 2025, *J. Futures Markets*) | CME futures tend to lead spot, by tens of milliseconds in one event study; leadership shifts day to day | Lead-lag exists but is fast; our X2 test measures Coinbase → Polymarket |
| Volatility patterns ([arXiv 2109.12142](https://arxiv.org/pdf/2109.12142v2.pdf); [arXiv 2402.11930](https://www.arxiv.org/pdf/2402.11930v2)) | Strong hour-of-day patterns, fat tails, and jumps | Our E8 confirmed the hour pattern (US open is 2× jumpier); E5 found it didn't improve forecasts |
| Kalshi's prices ([Bürgi, Deng, Whelan](https://www.ifo.de/en/cesifo/publications/2025/working-paper/makers-and-takers-economics-kalshi-prediction-market)) | Favorite-longshot bias: cheap contracts win less often than their price says; takers lose ~32% on average, makers ~10% | Our E3 found these BTC markets well calibrated, with no longshot bias |
| Manipulation of these exact markets ([Stanford/SMU study, reported by Cointelegraph](https://cointelegraph.com/news/stanford-study-5-minute-bitcoin-prediction-markets-enable-settlement-manipulation)) | Signs that some traders push BTC in the last seconds to move the settlement price; prices reverse ~10 s later; mostly gone in 15-minute markets | Our X5 looks for the pattern in our recordings |
| Fees ([Polymarket docs](https://docs.polymarket.com/trading/fees)) | Takers pay `shares × 0.05 × p × (1−p)`; makers pay nothing | That's 1.25¢ per share at 50¢: any edge must beat this plus the spread |

---

## Experiments on 30 days of history

**Data:** 8,616 settled windows (30 days), with the Up price about once a
minute, plus Coinbase 1-minute candles. Fetched by
`research/fetch_history.py`, analyzed by `research/analyze.py`.

**Honesty rules:** at time t we only use Coinbase prices known at t (a
candle's *open*, never its close). Anything tuned is tuned on the first 20
days and reported on the last 10.

### E1. Does one window predict the next?

Up won 48.7% of windows. The next window had the same result 50.3% of the
time: **no momentum between windows.**

### E2. What does the market really settle on?

The rules text says "TWAP", but TWAP over *what*? We tested each candidate
rule against the 8,600 official results, using Coinbase prices:

| Rule | Matches official |
|---|---|
| end price ≥ start price | 87.8% |
| whole-window average ≥ start price | 85.0% |
| last-minute average ≥ start price | 90.4% |
| end price ≥ minute-before-start average | 91.4% |
| last-minute average ≥ window's first-minute average | 87.1% |
| **last-minute average ≥ minute-before-start average** | **95.0%** |

The winner compares two **60-second averages** (Chainlink's 60-second TWAP
stream). The 5% that don't match are mostly tiny moves where Coinbase and
Chainlink disagree: when Coinbase moved less than $10, the simple rule
matched only 61% of the time.

### E3. Is the market's price a fair probability?

Yes, remarkably. Grouped by price, Up won about as often as the price said:
priced 0.2–0.3 → won 24%; priced 0.7–0.8 → won 75%. The only wrinkle: in
the middle (0.4–0.6), Up won ~2¢ less often than priced (see E9).

### E4. Our model vs the market

Brier score (lower is better; 0.25 = coin flip):

| Model | Brier | Market |
|---|---|---|
| end price vs start price | 0.1597 | 0.1319 |
| whole-window average | 0.1787 | 0.1319 |
| **two one-minute averages (E2's rule)** | **0.1452** | 0.1319 |

The market wins at every minute of the window.

### E5. Volatility levers

The volatility half-life barely matters (0.1467 at 5 min vs 0.1504 at 6 h).
Tuned on train (30 min half-life, ×0.8), the test score improved only from
0.1403 to 0.1393. Scaling volatility by hour of day: 0.1402 (no help).

### E6. Does the model know anything the market doesn't?

A logistic regression combining the market price and the model gives the
model a weight of 0.08 against the market's 0.98, and on the test days the
blend is no better than the market alone (Brier 0.1267 vs 0.1266). **At
minute resolution, the market already contains everything our model knows.**

### E7. Paper trading on history

Buying whenever the blend beat the price + 1¢ + fee lost money at every
edge threshold (−1.4¢ to −5¢ per share). There's nothing to trade.

### E8. When is BTC jumpy?

Average 1-minute move, by UTC hour: about 3.0 bp at 10–11 UTC, rising to
**6.4–7.0 bp at 14–15 UTC** (the US stock market open, 9:30 ET).

### E9. The "Up is a bit overpriced" bias

E3's small bias looked tradeable on the training days for one price range
(+0.4¢ per share), and **lost on the test days** (−1.4¢). Every range lost
on test. This is what overfitting looks like: given enough ranges to try,
one will look good by luck.

### E10. How much of the gap is our stale price?

Our candle-based BTC price is on average 21 s older than the market's
price. Grouping by how old it is:

| Our price is… | Model | Market | Gap |
|---|---|---|---|
| 0–15 s old | 0.1221 | 0.1152 | 0.007 |
| 15–30 s old | 0.1457 | 0.1358 | 0.010 |
| 30–45 s old | 0.1636 | 0.1360 | 0.028 |
| 45–60 s old | 0.1870 | 0.1501 | 0.037 |

The fresher our price, the closer we get. **Most of the gap is timing, not
knowledge.**

### E11. Is the model calibrated?

Mostly. It's a little overconfident at the extremes (says 2.3% → actually
3.8%; says 97.7% → actually 96.6%), consistent with fat tails or the
Coinbase/Chainlink difference.

### E12. Does a second exchange track the settlement price better?

Using E2's rule with different price sources (8,619 windows):

| Source | Matches official |
|---|---|
| Coinbase | 94.96% |
| Bitstamp | 95.20% |
| average of both | **95.50%** |

Small but consistent: Chainlink blends many exchanges, so a blend of ours
gets closer. That's why the recorder now also records Kraken, and the sim
has `spot_source: mean`.

---

## Experiments on live recordings

Recordings have every book change and Coinbase tick, so timing questions
become answerable. Exported with `hft lab export`, analyzed by
`research/analyze_recording.py`.

*First session (25 minutes, 5 windows): too small to conclude anything,
used to check the tools. The 8-hour session's results go here.*
