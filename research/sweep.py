"""Sweep strategy levers over a recording, with an honest train/test split.

Builds a grid of configs, gives each a TRAIN copy (first half of the
session's windows) and a TEST copy (second half), runs them all in ONE
backtest pass, and prints them side by side. Pick settings by the train
column; believe only the test column.

    uv run python research/sweep.py data/s2 [--grid taker|maker|source|all]
"""

import argparse
import glob
import json
import os
import subprocess
import sys
import tempfile

WINDOW = 300


def session_span(d):
    """(first window, last window) from the session's start file(s)."""
    starts, ends = [], []
    for p in glob.glob(f"{d}/session*.log"):
        with open(p) as f:
            e = json.loads(f.readline())
        t = e["ts"] // 1_000_000_000
        starts.append(t)
        if e["data"].get("minutes"):
            ends.append(t + e["data"]["minutes"] * 60)
    first = (min(starts) // WINDOW + 1) * WINDOW
    last = (min(ends) // WINDOW - 1) * WINDOW if ends else None
    return first, last


def grids(which):
    g = []
    if which in ("taker", "all"):
        for edge in (0.02, 0.04, 0.06, 0.10):
            for lat in (0, 100, 500, 2000):
                g.append({"strategy": "fair_value_taker", "edge": edge, "latency_ms": lat})
    if which in ("maker", "all"):
        for edge in (0.0, 0.01, 0.02, 0.04):
            for lat in (50, 250, 1000):
                for ev in (250, 1000):
                    g.append({"strategy": "fair_value_maker", "edge": edge, "latency_ms": lat, "eval_ms": ev})
    if which in ("source", "all"):
        for strat, edge in (("fair_value_taker", 0.04), ("fair_value_maker", 0.01)):
            for src in ("coinbase", "kraken", "mean"):
                for model in ("avg60", "point"):
                    g.append({"strategy": strat, "edge": edge, "spot_source": src, "model": model})
    return g


def label(c):
    short = {"fair_value_taker": "T", "fair_value_maker": "M"}
    parts = [short.get(c["strategy"], c["strategy"])]
    for k, v in c.items():
        if k != "strategy":
            parts.append(f"{k.replace('_ms','').replace('spot_source','src')}={v}")
    return " ".join(parts)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("dir")
    ap.add_argument("--grid", default="all")
    ap.add_argument("--hours", type=float, help="treat the session as this long (if still recording)")
    a = ap.parse_args()

    first, last = session_span(a.dir)
    if a.hours:
        last = first + int(a.hours * 3600) // WINDOW * WINDOW - WINDOW
    if last is None:
        sys.exit("session has no planned end; pass --hours")
    split = first + ((last - first) // WINDOW // 2) * WINDOW
    print(f"windows {first}..{last}; train < {split} <= test", file=sys.stderr)

    tmp = tempfile.mkdtemp(prefix="sweep-")
    paths, names = [], []
    for i, c in enumerate(grids(a.grid)):
        for half, (lo, hi) in (("train", (first, split - WINDOW)), ("test", (split, last))):
            cfg = dict(c, name=f"{i:03d}{half[:2]}", first_window=lo, last_window=hi)
            p = os.path.join(tmp, f"{i:03d}_{half}.json")
            with open(p, "w") as f:
                json.dump(cfg, f)
            paths.append(p)
        names.append(label(c))

    bin_ = os.path.join(os.path.dirname(__file__), "..", "target", "release", "hft")
    out = subprocess.run([bin_, "lab", "backtest", a.dir, *paths], capture_output=True, text=True, check=True).stdout

    rows = {}
    for line in out.splitlines():
        parts = line.split()
        if parts and len(parts[0]) == 5 and parts[0][:3].isdigit():
            rows[parts[0]] = dict(pnl=float(parts[1]), fills=int(parts[2]), fees=float(parts[3]),
                                  traded=int(parts[5]), brier=parts[6], market=parts[7])
    print(f"{'config':<52} {'train pnl':>10} {'fills':>6} {'test pnl':>10} {'fills':>6} {'test traded':>11}")
    for i, n in enumerate(names):
        tr, te = rows.get(f"{i:03d}tr"), rows.get(f"{i:03d}te")
        if tr and te:
            print(f"{n:<52} {tr['pnl']:>+10.2f} {tr['fills']:>6} {te['pnl']:>+10.2f} {te['fills']:>6} {te['traded']:>11}")
    any_row = next(iter(rows.values()), None)
    if any_row:
        print(f"\nforecast Brier on the test half: model {rows[f'{0:03d}te']['brier']}, market {rows[f'{0:03d}te']['market']}")


if __name__ == "__main__":
    main()
