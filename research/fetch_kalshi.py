"""Download history for Kalshi's 15-minute BTC Up/Down markets (KXBTC15M).

For each settled market: the reference price ("floor_strike", the 60-second
BRTI average before the open), the final 60-second average, the result,
and 1-minute candles with the YES bid/ask at the end of each minute.

Kalshi is the US-legal venue, so this is where any real-money idea would
have to work. Public market data: no keys needed.

    uv run --with certifi python research/fetch_kalshi.py --days 14
"""

import argparse
import json
import os
import sys
import time
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime

from fetch_history import get  # same retrying JSON GET

API = "https://api.elections.kalshi.com/trade-api/v2"


def ts(iso):
    return int(datetime.fromisoformat(iso.replace("Z", "+00:00")).timestamp())


def list_markets(days):
    """Settled KXBTC15M markets that closed in the last `days` days."""
    since = int(time.time() - days * 86400)
    out, cursor = [], ""
    while True:
        page = get(f"{API}/markets?series_ticker=KXBTC15M&status=settled&limit=1000"
                   f"&min_close_ts={since}" + (f"&cursor={cursor}" if cursor else ""))
        out += page.get("markets", [])
        cursor = page.get("cursor")
        if not cursor or not page.get("markets"):
            return out


def fetch_one(m):
    try:
        o, c = ts(m["open_time"]), ts(m["close_time"])
        page = get(f"{API}/series/KXBTC15M/markets/{m['ticker']}/candlesticks"
                   f"?start_ts={o}&end_ts={c}&period_interval=1")
        f = lambda d, k: float(d[k]) if d and d.get(k) is not None else None
        candles = [[x["end_period_ts"], f(x.get("yes_bid"), "close_dollars"), f(x.get("yes_ask"), "close_dollars"),
                    f(x.get("price"), "close_dollars"), float(x.get("volume_fp") or 0)]
                   for x in page.get("candlesticks", [])]
        return {"ticker": m["ticker"], "open": o, "close": c, "strike": m.get("floor_strike"),
                "final": float(m["expiration_value"]) if m.get("expiration_value") else None,
                "yes_won": m.get("result") == "yes", "volume": float(m.get("volume_fp") or 0), "candles": candles}
    except Exception as e:
        print(f"  {m['ticker']}: {e}", file=sys.stderr)
        return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--days", type=float, default=14)
    ap.add_argument("--out", default="research/data")
    a = ap.parse_args()
    os.makedirs(a.out, exist_ok=True)
    path = f"{a.out}/kalshi15.jsonl"
    done = set()
    if os.path.exists(path):
        with open(path) as f:
            done = {json.loads(l)["ticker"] for l in f}
    markets = [m for m in list_markets(a.days) if m["ticker"] not in done and m.get("result") in ("yes", "no")]
    print(f"{len(markets)} markets to fetch", file=sys.stderr)
    with open(path, "a") as out, ThreadPoolExecutor(4) as pool:
        for n, r in enumerate(pool.map(fetch_one, markets), 1):
            if r:
                out.write(json.dumps(r) + "\n")
            if n % 200 == 0:
                print(f"  {n}/{len(markets)}", file=sys.stderr)
    print("done", file=sys.stderr)


if __name__ == "__main__":
    main()
