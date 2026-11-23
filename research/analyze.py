"""Experiments on Polymarket 5-minute BTC Up/Down history.

Reads what fetch_history.py saved and runs a series of small experiments,
each answering one question. See docs/RESEARCH.md for the write-up.

    uv run python research/analyze.py [research/data]

Pure standard library, so it's easy to read. Everything is deterministic.

Honesty rules used throughout:
  - No look-ahead. At time t we only use Coinbase prices from candles that
    STARTED at or before t (the candle's open price), never its close.
  - Train/test split. Anything we tune is tuned on the first 2/3 of the
    days and reported on the last 1/3, which the tuning never saw.
"""

import json
import math
import sys
from collections import defaultdict

WINDOW = 300


def phi(x):
    return 0.5 * (1.0 + math.erf(x / math.sqrt(2.0)))


def brier(pairs):
    """Mean of (forecast - outcome)^2. 0 = perfect, 0.25 = coin flip."""
    return sum((p - y) ** 2 for p, y in pairs) / len(pairs) if pairs else float("nan")


def logloss(pairs):
    eps = 1e-6
    return -sum(y * math.log(max(p, eps)) + (1 - y) * math.log(max(1 - p, eps)) for p, y in pairs) / len(pairs)


# ---------------------------------------------------------------- loading --


def load(d):
    windows = []
    with open(f"{d}/windows.jsonl") as f:
        for line in f:
            w = json.loads(line)
            if not w.get("missing") and w.get("up_won") is not None:
                windows.append(w)
    windows.sort(key=lambda w: w["start"])
    candles = {}
    with open(f"{d}/candles.jsonl") as f:
        for line in f:
            t, lo, hi, op, cl, vol = json.loads(line)
            candles[t] = (lo, hi, op, cl, vol)
    return windows, candles


class Spot:
    """Coinbase 1-minute candles, with a per-minute EWMA volatility."""

    def __init__(self, candles):
        self.c = candles
        self.mins = sorted(candles)
        self._vol_cache = {}

    def open_at(self, m):
        """Price at the START of minute m (no look-ahead)."""
        c = self.c.get(m)
        return c[2] if c else None

    def close_of(self, m):
        c = self.c.get(m)
        return c[3] if c else None

    def mean_of(self, m):
        """Rough average price during minute m: (open + high + low + close) / 4."""
        c = self.c.get(m)
        return (c[0] + c[1] + c[2] + c[3]) / 4 if c else None

    def sigma_per_sqrt_s(self, m, halflife_min):
        """EWMA of squared 1-minute log returns, using prices known at m
        (open-to-open returns up to minute m). Returned per sqrt(second)."""
        if halflife_min not in self._vol_cache:
            lam = 0.5 ** (1.0 / halflife_min)
            series, var, prev = {}, None, None
            for k in self.mins:
                o = self.c[k][2]
                if prev is not None and prev[0] == k - 60:
                    r2 = math.log(o / prev[1]) ** 2
                    var = r2 if var is None else lam * var + (1 - lam) * r2
                prev = (k, o)
                if var:
                    series[k] = math.sqrt(var / 60.0)
            self._vol_cache[halflife_min] = series
        return self._vol_cache[halflife_min].get(m)


# ------------------------------------------------------------------ models --


def model_point(spot, open_, sigma, secs_left):
    """Settles on the price at the END vs the start (a 'digital option')."""
    if secs_left <= 0:
        return 1.0 if spot >= open_ else 0.0
    return phi(math.log(spot / open_) / (sigma * math.sqrt(secs_left)))


def model_twap(spot, open_, sigma, elapsed, secs_left, running_avg):
    """Settles on the AVERAGE price over the whole window vs the start.
    The finished part of the average is known; the rest is still random,
    and an average of a random walk moves only 1/sqrt(3) as much as the
    walk's endpoint."""
    T = elapsed + secs_left
    mean = (elapsed * running_avg + secs_left * spot) / T
    sd = sigma * spot * math.sqrt(secs_left**3 / 3.0) / T
    if sd <= 0:
        return 1.0 if mean >= open_ else 0.0
    return phi((mean - open_) / sd)


def model_twap60(spot, ref_avg, sigma, secs_to_last_minute):
    """What the market really settles on (found in E2): the AVERAGE price over
    the window's last minute vs the AVERAGE over the minute before it began.

    The reference average is already known when the window starts. The final
    average is still random: BTC first wanders for `secs_to_last_minute`,
    then we average over 60 s. Averaging a random walk over 60 s adds only
    a third of 60 s worth of variance (that's where the 60/3 comes from)."""
    var_s = max(secs_to_last_minute, 0.0) + 60.0 / 3.0
    return phi(math.log(spot / ref_avg) / (sigma * math.sqrt(var_s)))


# ------------------------------------------------------------- the samples --


def samples(windows, spot, halflife=60, sigma_mult=1.0, rule="twap60", hour_scale=None):
    """One row per (window, Polymarket price point inside the window)."""
    rows = []
    for w in windows:
        S = w["start"]
        open_ = spot.open_at(S)
        if open_ is None:
            continue
        y = 1.0 if w["up_won"] else 0.0
        for t, p_mkt in w["up_hist"]:
            if not (S + 20 <= t <= S + WINDOW - 15):
                continue
            m = t // 60 * 60  # the minute containing t: its open is known at t
            px = spot.open_at(m)
            sig = spot.sigma_per_sqrt_s(m, halflife)
            if px is None or not sig:
                continue
            sig *= sigma_mult
            if hour_scale:
                sig *= hour_scale[(m // 3600) % 24]
            left = S + WINDOW - t
            if rule == "twap60":
                ref = spot.mean_of(S - 60)
                if ref is None:
                    continue
                p_mod = model_twap60(px, ref, sig, S + WINDOW - 60 - m)
            elif rule == "point":
                p_mod = model_point(px, open_, sig, left)
            else:
                elapsed_min = list(range(S, m, 60))
                avgs = [spot.mean_of(k) for k in elapsed_min]
                if any(a is None for a in avgs):
                    continue
                run = sum(avgs) / len(avgs) if avgs else px
                p_mod = model_twap(px, open_, sig, m - S, S + WINDOW - m, run)
            rows.append(dict(start=S, t=t, left=left, y=y, mkt=p_mkt, mod=p_mod, px=px, open=open_, sig=sig))
    return rows


def hour_scales(spot, until):
    """How jumpy each UTC hour is compared with the average hour, measured
    only on minutes before `until` (the training data)."""
    hours = defaultdict(list)
    for m in spot.mins:
        a = spot.c.get(m - 60)
        if a and m < until:
            hours[(m // 3600) % 24].append(math.log(spot.c[m][2] / a[2]) ** 2)
    overall = sum(sum(v) for v in hours.values()) / sum(len(v) for v in hours.values())
    return {h: math.sqrt(sum(v) / len(v) / overall) for h, v in hours.items()}


def split(windows):
    cut = windows[int(len(windows) * 2 / 3)]["start"]
    return [w for w in windows if w["start"] < cut], [w for w in windows if w["start"] >= cut]


def clamp(p):
    return min(max(p, 1e-4), 1 - 1e-4)


def logit(p):
    p = clamp(p)
    return math.log(p / (1 - p))


def fit_logistic(X, y, iters=50, l2=1e-3):
    """Logistic regression by Newton's method (few features, so it's quick)."""
    k = len(X[0])
    b = [0.0] * k
    for _ in range(iters):
        g = [0.0] * k
        H = [[0.0] * k for _ in range(k)]
        for xi, yi in zip(X, y):
            z = sum(bj * xj for bj, xj in zip(b, xi))
            p = 1 / (1 + math.exp(-max(min(z, 35), -35)))
            for a in range(k):
                g[a] += (p - yi) * xi[a]
                for c in range(k):
                    H[a][c] += p * (1 - p) * xi[a] * xi[c]
        for a in range(k):
            g[a] += l2 * b[a]
            H[a][a] += l2
        b = [bj - dj for bj, dj in zip(b, solve(H, g))]
    return b


def solve(A, v):
    n = len(v)
    M = [row[:] + [v[i]] for i, row in enumerate(A)]
    for i in range(n):
        piv = max(range(i, n), key=lambda r: abs(M[r][i]))
        M[i], M[piv] = M[piv], M[i]
        for r in range(n):
            if r != i:
                f = M[r][i] / M[i][i]
                M[r] = [a - f * c for a, c in zip(M[r], M[i])]
    return [M[i][n] / M[i][i] for i in range(n)]


def predict(b, x):
    z = sum(bj * xj for bj, xj in zip(b, x))
    return 1 / (1 + math.exp(-max(min(z, 35), -35)))


def fmt(x):
    return f"{x:.4f}"


# ------------------------------------------------------------- experiments --


def main():
    d = sys.argv[1] if len(sys.argv) > 1 else "research/data"
    windows, candles = load(d)
    spot = Spot(candles)
    train, test = split(windows)
    days = (windows[-1]["start"] - windows[0]["start"]) / 86400
    print(f"# Data\n{len(windows)} settled windows over {days:.1f} days "
          f"({len(train)} train / {len(test)} test); {len(candles)} Coinbase minutes\n")

    # E1: base rate and streaks -------------------------------------------
    ups = sum(w["up_won"] for w in windows)
    print("# E1. Base rate and momentum between windows")
    print(f"Up won {ups}/{len(windows)} = {ups/len(windows):.3f}  (+/- {1/math.sqrt(len(windows)):.3f} is noise)")
    prev = {w["start"]: w["up_won"] for w in windows}
    same = [w["up_won"] == prev[w["start"] - WINDOW] for w in windows if w["start"] - WINDOW in prev]
    print(f"Same result as the previous window: {sum(same)/len(same):.3f} of {len(same)} (0.5 = no momentum)\n")

    # E2: what does the market actually settle on? -------------------------
    print("# E2. Reading the fine print: which price rule matches the official results?")
    rules = {
        "end price >= start price": lambda S: spot.close_of(S + 240) >= spot.open_at(S),
        "window average >= start price": lambda S: sum(spot.mean_of(S + 60 * k) for k in range(5)) / 5 >= spot.open_at(S),
        "last-minute avg >= first-minute-before avg": lambda S: spot.mean_of(S + 240) >= spot.mean_of(S - 60),
        "last-minute avg >= start price": lambda S: spot.mean_of(S + 240) >= spot.open_at(S),
        "end price >= minute-before avg": lambda S: spot.close_of(S + 240) >= spot.mean_of(S - 60),
        "last-minute avg >= first-minute avg": lambda S: spot.mean_of(S + 240) >= spot.mean_of(S),
    }
    for name, rule in rules.items():
        agree = n = 0
        for w in windows:
            try:
                agree += rule(w["start"]) == w["up_won"]
                n += 1
            except TypeError:  # missing candle
                pass
        print(f"  {name:<44} agrees {agree/n:.3f} of {n}")
    # Disagreement vs size of the move, for the point rule
    buckets = defaultdict(lambda: [0, 0])
    for w in windows:
        S = w["start"]
        o, c = spot.open_at(S), spot.close_of(S + 240)
        if o and c:
            b = min(int(abs(c - o) // 10) * 10, 100)
            buckets[b][0] += (c >= o) == w["up_won"]
            buckets[b][1] += 1
    print("  'end >= start' agreement by size of the Coinbase move:")
    for b in sorted(buckets):
        a, n = buckets[b]
        print(f"    |move| {'>=' if b == 100 else ''}{b:>3}-{b+10 if b < 100 else '':<3} $: {a/n:.3f} of {n}")
    print()

    # E3: is the market calibrated? ------------------------------------------
    rows = samples(windows, spot)
    print("# E3. Is the market's price a good probability? (calibration)")
    cal = defaultdict(lambda: [0.0, 0.0, 0])
    for r in rows:
        b = min(int(r["mkt"] * 10), 9)
        cal[b][0] += r["mkt"]
        cal[b][1] += r["y"]
        cal[b][2] += 1
    print("  price bucket   avg price   Up actually won    n")
    for b in sorted(cal):
        s, yy, n = cal[b]
        print(f"  {b/10:.1f}-{(b+1)/10:.1f}        {s/n:.3f}        {yy/n:.3f}       {n}")
    print()

    # E4: model vs market, by time left --------------------------------------
    print("# E4. Our model vs the market (Brier score, lower is better)")
    for rule in ("point", "twap", "twap60"):
        rws = samples(windows, spot, rule=rule)
        print(f"  model settles on: {rule}")
        by = defaultdict(list)
        for r in rws:
            by[min(int((WINDOW - r["left"]) // 60), 4)].append(r)
        for k in sorted(by):
            g = by[k]
            print(f"    minute {k+1}: model {fmt(brier([(r['mod'], r['y']) for r in g]))}"
                  f"  market {fmt(brier([(r['mkt'], r['y']) for r in g]))}  n={len(g)}")
        print(f"    all     : model {fmt(brier([(r['mod'], r['y']) for r in rws]))}"
              f"  market {fmt(brier([(r['mkt'], r['y']) for r in rws]))}")
    stale = sum(r["t"] % 60 for r in rows) / len(rows)
    print(f"  (handicap: our spot price is on average {stale:.0f} s older than the market's price)\n")

    # E5: tune the volatility estimate (on train, report on test) --------------
    print("# E5. Volatility levers: half-life and a multiplier (tuned on train, shown on test)")
    best = None
    for hl in (5, 15, 30, 60, 120, 360):
        for mult in (0.8, 1.0, 1.2, 1.4, 1.7):
            b = brier([(r["mod"], r["y"]) for r in samples(train, spot, hl, mult)])
            if best is None or b < best[0]:
                best = (b, hl, mult)
            if mult == 1.0:
                print(f"  half-life {hl:>3} min, x1.0: train {fmt(b)}")
    _, hl, mult = best
    te = samples(test, spot, hl, mult)
    te0 = samples(test, spot, 60, 1.0)
    print(f"  best on train: half-life {hl} min, x{mult}")
    print(f"  test: tuned {fmt(brier([(r['mod'], r['y']) for r in te]))}  "
          f"default {fmt(brier([(r['mod'], r['y']) for r in te0]))}  "
          f"market {fmt(brier([(r['mkt'], r['y']) for r in te]))}")
    hs = hour_scales(spot, test[0]["start"])
    teh = samples(test, spot, hl, mult, hour_scale=hs)
    print(f"  + time-of-day volatility (hour factors {min(hs.values()):.2f}..{max(hs.values()):.2f}): "
          f"test {fmt(brier([(r['mod'], r['y']) for r in teh]))}\n")

    # E6: does the model add anything to the market price? -------------------
    print("# E6. Blend: does our model know anything the market doesn't?")
    tr = samples(train, spot, hl, mult)
    feats = lambda r: [1.0, logit(r["mkt"]), logit(r["mod"])]
    te = samples(test, spot, hl, mult)
    b = fit_logistic([feats(r) for r in tr], [r["y"] for r in tr])
    bm = fit_logistic([[1.0, logit(r["mkt"])] for r in tr], [r["y"] for r in tr])
    print(f"  fitted weights: market {b[1]:.3f}, model {b[2]:.3f}  (model weight ~0 = no extra info)")
    pm = [(predict(bm, [1.0, logit(r["mkt"])]), r["y"]) for r in te]
    pb = [(predict(b, feats(r)), r["y"]) for r in te]
    print(f"  test Brier: market alone (recalibrated) {fmt(brier(pm))}   market+model {fmt(brier(pb))}")
    print(f"  test log loss: market alone {logloss(pm):.4f}   market+model {logloss(pb):.4f}\n")

    # E7: a rough paper-trading sweep on history -------------------------------
    print("# E7. Rough paper trading on test days: buy when blend - price > edge")
    print("  (assumes you pay the recorded price + 1c, plus the 0.05*p*(1-p) fee; one buy per point)")
    print("  edge    trades   win%    pnl/share   total pnl (1 share each)")
    for edge in (0.0, 0.02, 0.04, 0.06, 0.08, 0.10, 0.15):
        pnl = []
        for r in te:
            fair = predict(b, feats(r))
            for side_fair, price, win in ((fair, r["mkt"] + 0.01, r["y"]), (1 - fair, 1 - r["mkt"] + 0.01, 1 - r["y"])):
                if 0.02 < price < 0.98 and side_fair - price > edge:
                    fee = 0.05 * price * (1 - price)
                    pnl.append(win - price - fee)
        if pnl:
            wins = sum(p > 0 for p in pnl) / len(pnl)
            print(f"  {edge:.2f}   {len(pnl):>6}   {wins*100:5.1f}   {sum(pnl)/len(pnl):+.4f}     {sum(pnl):+.2f}")
    print()

    # E9: the small "Up is overpriced" bias from E3, as a trade --------------
    print("# E9. E3 showed Up winning ~2c less than its price in the middle. Tradeable?")
    print("  rule: buy Down at (1 - Up price) + 1c whenever Up price is in [lo, hi]; fee 0.05*p*(1-p)")
    print("  range        set     trades  Down win%   avg Down price   pnl/share")
    for lo, hi in ((0.40, 0.60), (0.45, 0.65), (0.50, 0.60)):
        for name, ws in (("train", train), ("test", test)):
            res = []
            for r in samples(ws, spot):
                if lo <= r["mkt"] <= hi:
                    price = 1 - r["mkt"] + 0.01
                    res.append(((1 - r["y"]) - price - 0.05 * price * (1 - price), price, 1 - r["y"]))
            if res:
                n = len(res)
                print(f"  {lo:.2f}-{hi:.2f}    {name:<5}  {n:>6}   {sum(x[2] for x in res)/n*100:5.1f}       "
                      f"{sum(x[1] for x in res)/n:.3f}          {sum(x[0] for x in res)/n:+.4f}")
    print()

    # E10: how much of the gap is just our stale price? ----------------------
    print("# E10. Gap to the market vs how old our spot price is")
    print("  (our price is the candle open at the start of the minute; the market's price is at t)")
    rows = samples(windows, spot, hl, mult)
    by = defaultdict(list)
    for r in rows:
        by[min(int((r["t"] % 60) // 15), 3)].append(r)
    for k in sorted(by):
        g = by[k]
        bm, bk = brier([(r["mod"], r["y"]) for r in g]), brier([(r["mkt"], r["y"]) for r in g])
        print(f"  {15*k:>2}-{15*k+15:<2} s old: model {fmt(bm)}  market {fmt(bk)}  gap {bm-bk:+.4f}  n={len(g)}")
    print()

    # E11: is the new model calibrated? ---------------------------------------
    print("# E11. Calibration of the twap60 model (all data)")
    cal = defaultdict(lambda: [0.0, 0.0, 0])
    for r in rows:
        b = min(int(r["mod"] * 10), 9)
        cal[b][0] += r["mod"]
        cal[b][1] += r["y"]
        cal[b][2] += 1
    print("  model says   Up actually won    n")
    for b in sorted(cal):
        sm, yy, n = cal[b]
        print(f"  {sm/n:.3f}        {yy/n:.3f}           {n}")
    print()

    # E8: time of day ---------------------------------------------------------
    print("# E8. When is BTC jumpy? (average 1-minute move by UTC hour)")
    hours = defaultdict(list)
    for m in spot.mins:
        a, bb = spot.c.get(m - 60), spot.c[m]
        if a:
            hours[(m // 3600) % 24].append(abs(math.log(bb[2] / a[2])))
    line = "  "
    for h in range(24):
        v = hours.get(h)
        if v:
            line += f"{h:02d}:{sum(v)/len(v)*1e4:4.1f} "
        if h % 8 == 7:
            print(line)
            line = "  "
    print("  (units: basis points per minute; 1 bp = 0.01%)")


if __name__ == "__main__":
    main()
