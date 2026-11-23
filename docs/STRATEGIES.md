# How people try to predict markets: a field guide

This is a map of the main kinds of trading strategy, in plain English, and
what each one would look like on the market we're playing with:
**"Will Bitcoin be up or down at the end of these 5 minutes?"**

Nothing here is advice. Most of these ideas are well known, so the easy
versions stopped working long ago. That's fine: the goal is to learn how
they work.

---

## First, the one idea everything rests on: edge

A contract that pays $1 if Up wins is worth exactly **the probability that
Up wins**. If Up has a 60% chance, the contract is worth 60¢.

- If you can buy it for 55¢, you have **edge**: on average you make 5¢ per
  contract.
- On any single bet you either win 45¢ or lose 55¢. Edge only shows up
  **on average, over many bets**.

So every strategy below comes down to one of two things:

1. **Know the probability better than the price does** (prediction), or
2. **Get a better price than other traders** (execution: being faster,
   patient, or in the right place).

And after both, you have to beat the **costs**: fees, plus the gap between
the buying price and the selling price (the **spread**).

### How many bets until you know it's skill?

A 5¢ edge on a coin-flip-ish bet is small next to the noise: each share wins
or loses about 50¢. A rough rule: to be fairly sure (two "standard
errors") that an edge of `e` is real, you need about `(1 / e)²` bets.

| Edge per share | Bets needed | 5-minute windows (one bet each) |
|---|---|---|
| 10¢ | ~100 | ~8 hours |
| 5¢ | ~400 | ~1.4 days |
| 2¢ | ~2,500 | ~9 days |
| 1¢ | ~10,000 | ~35 days |

This is the most important table in the file. **A strategy that made money
over an afternoon has proven almost nothing.**

---

## The families of strategies

### 1. Fair value from a model

**Idea:** write down a formula for what the contract *should* cost, and buy
when the market is cheaper than that.

**On our market:** if BTC moves randomly, the chance of finishing up
depends only on how far it is from the start price, how much time is left,
and how jumpy BTC is. That's `src/lab/model.rs`, and the
`fair_value_taker` strategy uses it.

**Levers:** how you measure jumpiness (volatility), how much safety margin
(edge) you demand, and what you assume about big sudden jumps.

**The catch:** the people quoting these markets have the same formula,
probably a better one. A pure model gives you a fair price to compare
against. It isn't an edge by itself. That's why it's step 1 in the lab: you
need it before anything else makes sense.

### 2. Market making

**Idea:** offer to buy a little below fair value and sell a little above
it, at the same time. If people trade with you on both sides, you earn the
difference (the spread) without caring where the price goes.

**On our market:** keep a bid and an ask on Up around the model's fair
value. This is plan step 10.

**The catch: adverse selection.** The traders who hit your order are often
the ones who know something you don't. When BTC jumps $50, the first
traders to notice buy your now-too-cheap Up contracts before you can move
your order. Market making is a race to update your prices before
the informed traders reach them.

**Levers:** spread width, size, how fast you update, how much inventory you
allow, and how you lean your prices when you hold too much (step 10's
skew formula).

### 3. Lead-lag (who moves first?)

**Idea:** when two prices are linked, one usually reacts first. If you can
see the leader move, you know where the follower is about to go.

**On our market:** Coinbase BTC moves → a moment later, the Up price on
Polymarket moves. How long is "a moment"? 50 ms? 2 seconds? The lab can
measure it from the recordings. Other leaders worth watching: Binance,
futures markets, and the exact price feed the market settles on
(Chainlink).

**The catch:** this is a speed game. The fastest bots sit close to the
exchange's servers, and the fees on these short crypto markets were largely
designed to make pure speed trades unprofitable. Worth *measuring*, even if
we can't win the race.

### 4. Order book and trade flow signals ("microstructure")

**Idea:** the order book itself hints at the next move. Some examples:

- **Imbalance:** far more shares waiting to buy than to sell often means
  the price is about to tick up.
- **Trade flow:** a burst of aggressive buys tends to continue for a bit.
- **Quote changes:** a big order that suddenly disappears says something.

This is what the paper in plan step 15 (arXiv 2108.09750) is about:
predicting the mid price a few seconds ahead from book and trade data.

**On our market:** do Polymarket's own book and trades predict the next
few seconds of its price, beyond what Coinbase already tells us?

**How you test it:** build the feature (say, imbalance), then check how
well it predicts the price change N seconds later (correlation, or the
"r²" from a regression). Do it on one day, then check the result holds on
a *different* day.

### 5. Momentum and mean reversion

**Idea:** prices that just moved either keep going (**momentum**) or
snap back (**mean reversion**). Which one depends on the market and the
time scale.

**On our market:** if BTC rose $40 in the last minute, does the next minute
lean up, down, or neither? The random-walk model assumes "neither". If the
data disagrees, that's an edge over the simple model.

**The catch:** at short time scales BTC is very close to random. Any effect
is tiny, and easy to imagine in noisy data.

### 6. Relative value / arbitrage

**Idea:** find two prices that *must* agree, and trade when they don't.

**On our market and its neighbors:**

- Up + Down must pay exactly $1. If you can buy both for 97¢ total, that's
  free money (it rarely happens, and fees usually eat it).
- Polymarket's 5-minute windows and Kalshi's 15-minute window
  (`KXBTC15M`) are about the same Bitcoin. Three 5-minute "Up"s don't add
  up to one 15-minute "Up", but they're linked: if they disagree a lot,
  one of them is mispriced.
- Kalshi's hourly "above $X" ladder (`KXBTCD`) implies a whole
  probability curve. Does it agree with the 5-minute prices?

This is plan step 12 (two venues), applied to prediction markets.

### 7. Settlement rules and market quirks

**Idea:** read the fine print. Know exactly how the market settles, and
look for systematic habits in how people trade.

**On our market:**

- It settles on **Chainlink's** BTC price, averaged over time, not
  Coinbase's last trade. When BTC is within a few dollars of the start
  price near the end, knowing the exact settlement number matters a lot.
- **Favorite-longshot bias:** in betting markets, long shots (a 3¢
  contract) tend to be overpriced and favorites underpriced. Is that true
  here in the last 30 seconds?
- **Time of day:** volatility jumps at the US stock market open and around
  economic news. A model with one fixed volatility will be wrong then.

### 8. Fundamental and event forecasting

**Idea:** know more about the real world than the market does: polls,
weather models, sports data, economic releases.

**On our market:** hardly applies: 5 minutes of Bitcoin is mostly noise
plus speed. But this is the biggest category on Kalshi as a whole
(elections, the Fed, weather, gas prices), and the lab's pieces (recorder,
paper account, scoring) work the same for it.

---

## Traps that make a strategy look better than it is

| Trap | What happens | How the lab guards against it |
|---|---|---|
| **Look-ahead** | The backtest uses information that wasn't available yet (e.g., the final result) | The sim replays events in time order; strategies only see a `View` of the present |
| **Free fills** | Assuming you could buy at the price shown, in any size | Paper fills walk the real recorded book, only up to the size really offered |
| **Instant orders** | Ignoring the time an order takes to arrive | `latency_ms`: fills use the book as it was when the order would have arrived |
| **Forgetting fees** | Small edges vanish after fees | `fee_rate` on every fill |
| **Overfitting** | Trying 200 settings and keeping the best one finds luck, not skill | Pick settings on one session, then test on a *different* one (lab stage 2) |
| **Too few bets** | An afternoon of profit means little | The sample-size table above; count windows, not dollars |
| **Wrong settlement price** | Using Coinbase when the market settles on Chainlink | The report shows how often the Coinbase proxy matched the official result |

---

## How to measure a forecast: the Brier score

A model that says "70%" should be right about 70% of the time. The
**Brier score** measures this: average `(forecast − outcome)²`, where the
outcome is 1 if Up won and 0 if not.

- 0.00 = perfect (always 100% sure and always right)
- 0.25 = no information (always saying 50%)
- lower is better

The lab computes it for **our model** and for **the market's own price**,
using the same moments. If the market's score is lower (usually it is),
the market knows more than our model does. Beating the market's Brier score
is a strong sign of real edge, much stronger than an afternoon of profit.

---

## Where to read more

- *The Elements of Statistical Learning* (Hastie, Tibshirani, Friedman):
  regression, overfitting, cross-validation. Free PDF from the authors.
- arXiv 2108.09750 (plan step 15): signals from order book data.
- Search terms: "digital option pricing", "order flow imbalance",
  "adverse selection market making", "Kelly criterion", "favorite-longshot
  bias", "Brier score calibration".
