"""Experiments on Kalshi's 15-minute BTC markets (KXBTC15M).

    uv run python research/analyze_kalshi.py [research/data]

Kalshi is cleaner to test than Polymarket's history:
  - the reference price (floor_strike) is published, so it's exact
  - candles give the real YES bid and ask at each minute's end, which is
    exactly when a Coinbase candle opens: no stale-price handicap
  - the settlement rule is written plainly: the 60-second BRTI average
    before close vs the 60-second average before open

Same honesty rules as analyze.py: no look-ahead, tune on the first 2/3 of
the days, report on the last 1/3.
"""

import json
import math
import sys
from collections import defaultdict

from analyze import Spot, brier, fit_logistic, logit, logloss, model_twap60, phi, predict  # noqa: F401

FEE = 0.07  # Kalshi taker fee: 0.07 * p * (1 - p) per contract (rounded up per order)


def load(d):
    ms = []
    with open(f"{d}/kalshi15.jsonl") as f:
        for line in f:
            m = json.loads(line)
            if m.get("strike") and m.get("candles"):
                ms.append(m)
    ms.sort(key=lambda m: m["open"])
    candles = {}
    with open(f"{d}/candles.jsonl") as f:
        for line in f:
            t, lo, hi, op, cl, vol = json.loads(line)
            candles[t] = (lo, hi, op, cl, vol)
    return ms, Spot(candles)


def rows_for(ms, spot, halflife=30, mult=0.8, basis_fix=True):
    out = []
    for m in ms:
        y = 1.0 if m["yes_won"] else 0.0
        cb_ref = spot.mean_of(m["open"] - 60)
        if cb_ref is None:
            continue
        # BRTI (Kalshi's index) vs Coinbase: the gap at the open, assumed to
        # persist for the next 15 minutes.
        basis = m["strike"] - cb_ref if basis_fix else 0.0
        for e, bid, ask, _last, _vol in m["candles"]:
            left = m["close"] - e
            if left < 15 or bid is None or ask is None or ask <= bid or ask >= 1 or bid <= 0:
                continue
            px, sig = spot.open_at(e), spot.sigma_per_sqrt_s(e, halflife)
            if px is None or not sig:
                continue
            p = model_twap60(px + basis, m["strike"], sig * mult, left - 60)
            out.append(dict(open=m["open"], e=e, left=left, y=y, bid=bid, ask=ask, mid=(bid + ask) / 2, mod=p))
    return out


def main():
    d = sys.argv[1] if len(sys.argv) > 1 else "research/data"
    ms, spot = load(d)
    cut = ms[int(len(ms) * 2 / 3)]["open"]
    train, test = [m for m in ms if m["open"] < cut], [m for m in ms if m["open"] >= cut]
    days = (ms[-1]["open"] - ms[0]["open"]) / 86400
    yes = sum(m["yes_won"] for m in ms)
    vol = sum(m["volume"] for m in ms) / len(ms)
    print(f"# Data\n{len(ms)} settled KXBTC15M markets over {days:.1f} days ({len(train)} train / {len(test)} test)")
    print(f"YES won {yes/len(ms):.3f}; average volume {vol:,.0f} contracts per market\n")

    # K1 ------------------------------------------------------------------
    print("# K1. Settlement rule and the Coinbase/BRTI gap")
    agree = n = 0
    gaps = []
    for m in ms:
        a, b = spot.mean_of(m["close"] - 60), spot.mean_of(m["open"] - 60)
        if a is None or b is None:
            continue
        agree += (a >= b) == m["yes_won"]
        n += 1
        gaps.append(m["strike"] - b)
        if m.get("final"):
            pass
    gaps.sort()
    print(f"  Coinbase 'last-minute avg >= minute-before-open avg' matches Kalshi's result: {agree/n:.3f} of {n}")
    print(f"  strike (BRTI) - Coinbase average: median {gaps[len(gaps)//2]:+.2f} $, "
          f"middle 80% {gaps[len(gaps)//10]:+.2f} .. {gaps[9*len(gaps)//10]:+.2f} $\n")

    # K2 ------------------------------------------------------------------
    rows = rows_for(ms, spot)
    print("# K2. Spread and calibration of Kalshi's price")
    spreads = sorted(r["ask"] - r["bid"] for r in rows)
    print(f"  bid-ask spread: median {spreads[len(spreads)//2]*100:.1f}c, 90th percentile {spreads[9*len(spreads)//10]*100:.1f}c")
    cal = defaultdict(lambda: [0.0, 0.0, 0])
    for r in rows:
        b = min(int(r["mid"] * 10), 9)
        cal[b][0] += r["mid"]
        cal[b][1] += r["y"]
        cal[b][2] += 1
    print("  mid bucket   avg mid   YES actually won    n")
    for b in sorted(cal):
        s, yy, k = cal[b]
        print(f"  {b/10:.1f}-{(b+1)/10:.1f}      {s/k:.3f}        {yy/k:.3f}         {k}")
    print()

    # K3 ------------------------------------------------------------------
    print("# K3. Model vs Kalshi's mid, at the SAME moment (Brier)")
    raw = rows_for(ms, spot, basis_fix=False)
    print(f"  all: model {brier([(r['mod'], r['y']) for r in rows]):.4f} (basis-fixed)  "
          f"{brier([(r['mod'], r['y']) for r in raw]):.4f} (raw Coinbase)  "
          f"Kalshi mid {brier([(r['mid'], r['y']) for r in rows]):.4f}")
    by = defaultdict(list)
    for r in rows:
        by[min(int(r["left"] // 180), 4)].append(r)
    for k in sorted(by, reverse=True):
        g = by[k]
        print(f"  {k*3:>2}-{k*3+3:<2} min left: model {brier([(r['mod'], r['y']) for r in g]):.4f}  "
              f"Kalshi {brier([(r['mid'], r['y']) for r in g]):.4f}  n={len(g)}")
    print()

    # K4 ------------------------------------------------------------------
    print("# K4. Blend: does the model know anything Kalshi's price doesn't? (fit on train)")
    tr, te = rows_for(train, spot), rows_for(test, spot)
    feats = lambda r: [1.0, logit(r["mid"]), logit(r["mod"])]
    b = fit_logistic([feats(r) for r in tr], [r["y"] for r in tr])
    bm = fit_logistic([[1.0, logit(r["mid"])] for r in tr], [r["y"] for r in tr])
    pm = [(predict(bm, [1.0, logit(r["mid"])]), r["y"]) for r in te]
    pb = [(predict(b, feats(r)), r["y"]) for r in te]
    print(f"  weights: Kalshi {b[1]:.3f}, model {b[2]:.3f}")
    print(f"  test Brier: Kalshi alone {brier(pm):.4f}   Kalshi+model {brier(pb):.4f}")
    print(f"  test log loss: Kalshi alone {logloss(pm):.4f}   Kalshi+model {logloss(pb):.4f}\n")

    # K5 ------------------------------------------------------------------
    print("# K5. Paper trading on test days: pay the real ask, plus Kalshi's taker fee")
    print("  buy YES at ask if blend - ask > edge + fee; buy NO at (1 - bid) likewise")
    print("  edge    trades  markets   win%   pnl/contract   (first trade per market only)")
    for edge in (0.0, 0.01, 0.02, 0.03, 0.05, 0.08):
        seen, res = set(), []
        for r in te:
            if r["open"] in seen:
                continue
            fair = predict(b, feats(r))
            for f_, price, win in ((fair, r["ask"], r["y"]), (1 - fair, 1 - r["bid"], 1 - r["y"])):
                fee = FEE * price * (1 - price)
                if f_ - price > edge + fee:
                    res.append(win - price - fee)
                    seen.add(r["open"])
                    break
        if res:
            n = len(res)
            se = (sum((x - sum(res) / n) ** 2 for x in res) / n) ** 0.5 / n ** 0.5
            print(f"  {edge:.2f}   {n:>6}   {len(seen):>6}   {sum(x > 0 for x in res)/n*100:5.1f}   "
                  f"{sum(res)/n:+.4f} +/- {se:.4f}")
    print("  (+/- is one standard error: a result within ~2 of them is indistinguishable from zero)\n")

    # K6 ------------------------------------------------------------------
    print("# K6. K2's YES bias as a trade: buy NO at (1 - bid) when the mid is in a range")
    print("  range       set     markets  NO win%   avg NO price   pnl/contract (after fee)")
    for lo, hi in ((0.55, 0.80), (0.60, 0.80), (0.50, 0.70)):
        for name, part in (("train", train), ("test", test)):
            seen, res = set(), []
            for r in rows_for(part, spot):
                if r["open"] in seen or not (lo <= r["mid"] <= hi):
                    continue
                price = 1 - r["bid"]
                res.append(((1 - r["y"]) - price - FEE * price * (1 - price), price, 1 - r["y"]))
                seen.add(r["open"])
            if res:
                n = len(res)
                mean = sum(x[0] for x in res) / n
                se = (sum((x[0] - mean) ** 2 for x in res) / n) ** 0.5 / n ** 0.5
                print(f"  {lo:.2f}-{hi:.2f}   {name:<5}   {n:>6}   {sum(x[2] for x in res)/n*100:5.1f}     "
                      f"{sum(x[1] for x in res)/n:.3f}        {mean:+.4f} +/- {se:.4f}")


if __name__ == "__main__":
    main()
