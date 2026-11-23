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
5. **Kalshi too.** On Kalshi's 15-minute BTC markets (the US-legal
   venue), 27 days of data showed a promising +1¢ to +3¢ per contract.
   With 68 days it **vanished** (+0.4¢ ± 0.7¢). The most important lesson
   of the day: small samples lie.
6. No strategy so far makes money after fees. That's the expected result,
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

## Kalshi's 15-minute BTC markets (KXBTC15M)

**Data:** 6,490 settled markets over 68 days (all the API returned),
fetched by `research/fetch_kalshi.py`, analyzed by
`research/analyze_kalshi.py`. Kalshi's history is cleaner than
Polymarket's:
- the reference price (`floor_strike`) is published, so it's exact;
- candles give the real YES **bid and ask** at each minute's end, which is
  exactly when a Coinbase candle opens, so there's **no stale-price
  handicap**;
- average volume is ~3.2 million contracts per market, and the spread is
  1¢ almost always.

| | 27 days (first look) | **68 days** |
|---|---|---|
| K1. Coinbase rule matches Kalshi's result | 97.6% | 97.0% |
| K1. Kalshi's index (BRTI) minus Coinbase, median | +$3.61 | +$2.57 |
| K3. Brier: model / Kalshi mid, same moment | 0.1596 / 0.1587 | 0.1598 / **0.1564** |
| K4. Blend weight of the model (vs Kalshi's ~1.0) | **0.41** | **0.002** |
| K4. Test log loss: Kalshi alone / + model | 0.4616 / 0.4599 | 0.4698 / 0.4697 |
| K5. Paper taker profit per contract (real ask + fee) | +1.3¢ … +3.0¢ (≈1σ each) | **+0.4¢ ± 0.7¢** |
| K6. "Buy NO when YES is 55–80¢", train / test | +0.7¢ / +1.6¢ | **−1.8¢** / +0.9¢ |

**What happened.** On 27 days, the model seemed to add real information
to Kalshi's price, and every trading threshold made money. Each result
was only about one standard error from zero, which means "could easily be
luck", and the bigger sample showed it was. This is the trap from
STRATEGIES.md's sample-size table, caught in the act: a plausible story
(Kalshi is less efficient than Polymarket), a backtest that agreed, and
then nothing.

**What survived.**
- With fair timing, our simple model is close to Kalshi's price, but Kalshi
  is still better, especially in the last minutes.
- Kalshi slightly overprices YES around 50–75¢ (K2: priced 0.549 → won
  0.531), but after the spread and the 0.07·p(1−p) fee, it's not
  tradeable as a taker.
- Kalshi's index runs a couple of dollars above Coinbase. Because the
  market compares two averages from the same index, a steady difference
  cancels out; only changes in it matter.

---

## Experiments on live recordings

Recordings have every book change and Coinbase tick, so timing questions
become answerable. Exported with `hft lab export`, analyzed by
`research/analyze_recording.py`.

**Session s2 (planned 8 h), and a lesson.** Half an hour in, both BTC
feeds went silent without disconnecting (see PRIMER, "silence isn't
success"). Only the first 0.5 h (7 windows) is usable. Its hints, far too
small to trust:

- **X2, who moves first:** price changes line up mostly within the same
  second (correlation 0.63), with a smaller echo 1–2 s later (0.17, 0.12):
  Polymarket follows Coinbase partly with a short delay.
- **X3, book imbalance:** a heavy bid side predicts the next 1 s move of
  the Up price (correlation 0.29), fading by 15 s (0.06), the same shape
  the order-flow papers describe.
- **X4, disagreement:** when our model and the market disagree, the market
  moves about 60% of the way toward the model within 5 s.
- **X5, last seconds:** too few windows to say anything.

X2 and X4 together say the market lags Coinbase by about a second. That's
exactly the speed game from STRATEGIES.md, and the history says that,
after fees, it isn't enough to profit from. The 6-hour session s3 (with the
watchdog) will check these with ~70 windows.
