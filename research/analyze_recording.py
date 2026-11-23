"""Experiments on a live recording, exported to CSV by `hft lab export`.

Unlike the minute-level history (analyze.py), a recording has every book
change and every Coinbase tick, so timing questions become answerable.

    hft lab export data/s2 lab/configs/export_fast.json > s2.csv
    uv run python research/analyze_recording.py s2.csv research/data/windows.jsonl

The second file supplies official results (re-run fetch_history.py to
include the newest windows).
"""

import csv
import json
import math
import sys
from bisect import bisect_left
from collections import defaultdict


def brier(pairs):
    return sum((p - y) ** 2 for p, y in pairs) / len(pairs) if pairs else float("nan")


def corr(xs, ys):
    n = len(xs)
    if n < 3:
        return float("nan")
    mx, my = sum(xs) / n, sum(ys) / n
    sxy = sum((x - mx) * (y - my) for x, y in zip(xs, ys))
    sxx = sum((x - mx) ** 2 for x in xs)
    syy = sum((y - my) ** 2 for y in ys)
    return sxy / math.sqrt(sxx * syy) if sxx > 0 and syy > 0 else float("nan")


def num(s):
    return float(s) if s != "" else None


def load(csv_path, windows_path):
    results = {}
    with open(windows_path) as f:
        for line in f:
            w = json.loads(line)
            if w.get("up_won") is not None:
                results[w["start"]] = 1.0 if w["up_won"] else 0.0
    rows, spots = [], []
    with open(csv_path) as f:
        for r in csv.DictReader(f):
            bid, ask = num(r["up_bid"]), num(r["up_ask"])
            spots.append((int(r["ts_ns"]) / 1e9, float(r["spot"])))
            # Near the end the Up book is often one-sided (nobody sells at
            # 100c): no mid, so leave those rows out of the price analysis.
            if bid is None or ask is None:
                continue
            rows.append(dict(
                t=int(r["ts_ns"]) / 1e9, window=int(r["window"]), left=float(r["secs_left"]),
                spot=float(r["spot"]), ref=float(r["reference"]), p=float(r["p_up"]),
                bid=bid, ask=ask, mid=(bid + ask) / 2,
                bsz=num(r["up_bid_size"]) or 0.0, asz=num(r["up_ask_size"]) or 0.0,
                y=results.get(int(r["window"])),
            ))
    return rows, spots


def series_at(rows, key):
    """Value of `key` at (or just before) any time, by binary search."""
    ts = [r["t"] for r in rows]
    vals = [r[key] for r in rows]
    return lookup(ts, vals)


def lookup(ts, vals):
    def at(t):
        i = bisect_left(ts, t + 1e-9) - 1
        return vals[i] if i >= 0 else None

    return at


def main():
    rows, spots = load(sys.argv[1], sys.argv[2])
    rows.sort(key=lambda r: r["t"])
    wins = sorted({r["window"] for r in rows})
    have = sorted({r["window"] for r in rows if r["y"] is not None})
    hours = (rows[-1]["t"] - rows[0]["t"]) / 3600
    print(f"# Recording: {len(rows)} rows, {hours:.1f} h, {len(wins)} windows ({len(have)} with official results)\n")

    # X1 -----------------------------------------------------------------
    print("# X1. Model vs market with exact timing (Brier, lower is better)")
    by = defaultdict(list)
    for r in rows:
        if r["y"] is not None:
            by[min(int((300 - r["left"]) // 60), 4)].append(r)
    for k in sorted(by):
        g = by[k]
        print(f"  minute {k+1}: model {brier([(r['p'], r['y']) for r in g]):.4f}  "
              f"market {brier([(r['mid'], r['y']) for r in g]):.4f}  n={len(g)}")
    allr = [r for r in rows if r["y"] is not None]
    print(f"  all     : model {brier([(r['p'], r['y']) for r in allr]):.4f}  "
          f"market {brier([(r['mid'], r['y']) for r in allr]):.4f}")
    print(f"  (independent windows: {len(have)}; differences under ~0.01 are noise at this size)\n")

    # X2 -----------------------------------------------------------------
    print("# X2. Who moves first? Correlation of model-price changes with market-mid changes")
    print("  (positive lag = the market moves AFTER our Coinbase-based model)")
    step = 1.0
    mid_at, p_at = series_at(rows, "mid"), series_at(rows, "p")
    grid = []
    t = rows[0]["t"] + 5
    while t < rows[-1]["t"] - 5:
        grid.append(t)
        t += step
    for lag in (-3, -2, -1, 0, 1, 2, 3, 5):
        xs, ys = [], []
        for t in grid:
            a0, a1 = p_at(t - step), p_at(t)
            b0, b1 = mid_at(t - step + lag), mid_at(t + lag)
            if None not in (a0, a1, b0, b1):
                xs.append(a1 - a0)
                ys.append(b1 - b0)
        print(f"  lag {lag:+d} s: corr {corr(xs, ys):+.3f}  (n={len(xs)})")
    print()

    # X3 -----------------------------------------------------------------
    print("# X3. Does book imbalance predict the next move of the Up mid?")
    print("  imbalance = bid size / (bid size + ask size) at the top of the Up book")
    for h in (1, 5, 15):
        xs, ys = [], []
        for r in rows[:: max(1, len(rows) // 50000)]:
            if r["bsz"] + r["asz"] <= 0 or r["left"] < h + 5:
                continue
            fut = mid_at(r["t"] + h)
            if fut is None:
                continue
            xs.append(r["bsz"] / (r["bsz"] + r["asz"]) - 0.5)
            ys.append(fut - r["mid"])
        print(f"  next {h:>2} s: corr {corr(xs, ys):+.3f}  (n={len(xs)})")
    print()

    # X4 -----------------------------------------------------------------
    print("# X4. When model and market disagree, who moves toward whom?")
    print("  gap = model - market mid; does the market mid then move by a share of the gap?")
    for h in (2, 5, 15, 30):
        xs, ys = [], []
        for r in rows[:: max(1, len(rows) // 50000)]:
            if r["left"] < h + 5:
                continue
            fut = mid_at(r["t"] + h)
            if fut is None:
                continue
            xs.append(r["p"] - r["mid"])
            ys.append(fut - r["mid"])
        # slope of a least-squares line through the origin
        sxx = sum(x * x for x in xs)
        slope = sum(x * y for x, y in zip(xs, ys)) / sxx if sxx else float("nan")
        print(f"  next {h:>2} s: market closes {slope*100:5.1f}% of the gap  (corr {corr(xs, ys):+.3f}, n={len(xs)})")
    print()

    # X5 -----------------------------------------------------------------
    print("# X5. Last seconds of a window (the manipulation study's pattern)")
    spots.sort()
    spot_at = lookup([t for t, _ in spots], [v for _, v in spots])
    moves_end, moves_after, moves_mid = [], [], []
    for w in wins:
        end = w + 300
        a, b, c = spot_at(end - 10), spot_at(end), spot_at(end + 10)
        m0, m1 = spot_at(end - 160), spot_at(end - 150)
        if None not in (a, b, c):
            moves_end.append(math.log(b / a) * 1e4)
            moves_after.append(math.log(c / b) * 1e4)
        if None not in (m0, m1):
            moves_mid.append(abs(math.log(m1 / m0) * 1e4))
    if moves_end:
        avg_abs = lambda v: sum(abs(x) for x in v) / len(v)
        print(f"  avg |move| in the last 10 s:       {avg_abs(moves_end):.2f} bp")
        print(f"  avg |move| in a mid-window 10 s:   {sum(moves_mid)/len(moves_mid):.2f} bp")
        print(f"  corr(last 10 s, next 10 s): {corr(moves_end, moves_after):+.3f}  "
              f"(negative = reversal; n={len(moves_end)}, very noisy)")


if __name__ == "__main__":
    main()
