"""Download history for Polymarket's 5-minute BTC Up/Down markets.

For every 5-minute window in the last N days, saves:
  - the official result (did Up win?) and the market's volume
  - the Up price about once a minute (Polymarket's /prices-history)
and separately, Coinbase BTC-USD 1-minute candles covering the same days.

This is much coarser than a live recording (one price per minute instead of
every book change), but it covers thousands of windows using a few MB of
disk instead of gigabytes. Good for testing MODELS (does the forecast match
reality?). For testing EXECUTION (latency, fills), use `hft lab record`.

Usage:
    uv run --with certifi python research/fetch_history.py --days 7

Re-running continues where it stopped (windows already saved are skipped).
Public data only: no keys.
"""

import argparse
import json
import os
import ssl
import sys
import time
import urllib.request
from concurrent.futures import ThreadPoolExecutor

import certifi

WINDOW = 300
CTX = ssl.create_default_context(cafile=certifi.where())
HEADERS = {"User-Agent": "research-script", "Accept": "application/json"}


def get(url, tries=5):
    """GET a URL and parse JSON, retrying with growing waits."""
    for i in range(tries):
        try:
            req = urllib.request.Request(url, headers=HEADERS)
            with urllib.request.urlopen(req, context=CTX, timeout=20) as r:
                return json.loads(r.read())
        except Exception as e:  # network hiccup or rate limit
            if i == tries - 1:
                raise
            time.sleep(1.5 * 2**i)
            last = e
    raise last  # unreachable


def fetch_window(start):
    try:
        return _fetch_window(start)
    except Exception as e:
        print(f"  window {start}: {e} (will retry next run)", file=sys.stderr)
        return None


def _fetch_window(start):
    ev = get(f"https://gamma-api.polymarket.com/events?slug=btc-updown-5m-{start}")
    if not ev or not ev[0].get("markets"):
        return {"start": start, "missing": True}
    m = ev[0]["markets"][0]
    outcomes = json.loads(m.get("outcomes") or "[]")
    tokens = json.loads(m.get("clobTokenIds") or "[]")
    prices = json.loads(m.get("outcomePrices") or "[]")
    if "Up" not in outcomes or len(tokens) != len(outcomes):
        return {"start": start, "missing": True}
    i = outcomes.index("Up")
    up_won = None
    if m.get("closed") and prices[i] in ("0", "1"):
        up_won = prices[i] == "1"
    hist = get(
        "https://clob.polymarket.com/prices-history"
        f"?market={tokens[i]}&startTs={start - 120}&endTs={start + WINDOW + 60}&fidelity=1"
    ).get("history", [])
    return {
        "start": start,
        "up_won": up_won,
        "volume": float(m.get("volume") or 0),
        "up_hist": [[h["t"], h["p"]] for h in hist],
    }


def fetch_candles(first, last, path):
    """Coinbase gives at most 300 candles per request."""
    have = set()
    if os.path.exists(path):
        with open(path) as f:
            have = {json.loads(l)[0] for l in f}
    with open(path, "a") as out:
        t = first
        while t < last:
            end = min(t + 300 * 60, last)
            if not all(s in have for s in range(t, end, 60)):
                rows = get(
                    "https://api.exchange.coinbase.com/products/BTC-USD/candles?granularity=60"
                    f"&start={time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime(t))}"
                    f"&end={time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime(end))}"
                )
                # Each row: [time, low, high, open, close, volume]
                for r in sorted(rows):
                    if r[0] not in have and t <= r[0] < end:
                        out.write(json.dumps(r) + "\n")
                        have.add(r[0])
                time.sleep(0.15)  # stay well under Coinbase's rate limit
            t = end


def fetch_bitstamp(first, last, path):
    """Bitstamp 1-minute candles, saved in Coinbase's row format
    [time, low, high, open, close, volume]. A second exchange, to test
    whether a multi-exchange price tracks Chainlink better (RESEARCH.md E12)."""
    have = set()
    if os.path.exists(path):
        with open(path) as f:
            have = {json.loads(l)[0] for l in f}
    with open(path, "a") as out:
        t = first
        while t < last:
            if not all(s in have for s in range(t, min(t + 1000 * 60, last), 60)):
                rows = get(f"https://www.bitstamp.net/api/v2/ohlc/btcusd/?step=60&limit=1000&start={t}")
                for c in rows["data"]["ohlc"]:
                    ts = int(c["timestamp"])
                    if ts not in have and ts < last:
                        r = [ts, float(c["low"]), float(c["high"]), float(c["open"]), float(c["close"]), float(c["volume"])]
                        out.write(json.dumps(r) + "\n")
                        have.add(ts)
                time.sleep(0.2)
            t += 1000 * 60


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--days", type=float, default=7)
    ap.add_argument("--out", default="research/data")
    ap.add_argument("--workers", type=int, default=6)
    a = ap.parse_args()
    os.makedirs(a.out, exist_ok=True)

    now = int(time.time())
    last_start = now // WINDOW * WINDOW - 2 * WINDOW  # finished at least 5 min ago
    first_start = last_start - int(a.days * 86400) // WINDOW * WINDOW
    starts = range(first_start, last_start + 1, WINDOW)

    wpath = f"{a.out}/windows.jsonl"
    done = set()
    if os.path.exists(wpath):
        with open(wpath) as f:
            done = {json.loads(l)["start"] for l in f}
    todo = [s for s in starts if s not in done]
    print(f"{len(starts)} windows, {len(todo)} to fetch", file=sys.stderr)
    with open(wpath, "a") as out, ThreadPoolExecutor(a.workers) as pool:
        for n, w in enumerate(pool.map(fetch_window, todo), 1):
            if w is not None:
                out.write(json.dumps(w) + "\n")
            if n % 100 == 0:
                out.flush()
                print(f"  {n}/{len(todo)}", file=sys.stderr)

    print("candles...", file=sys.stderr)
    fetch_candles(first_start - 3600 * 6, last_start + WINDOW + 60, f"{a.out}/candles.jsonl")
    print("bitstamp candles...", file=sys.stderr)
    fetch_bitstamp(first_start - 3600 * 6, last_start + WINDOW + 60, f"{a.out}/bitstamp.jsonl")
    print("done", file=sys.stderr)


if __name__ == "__main__":
    main()
