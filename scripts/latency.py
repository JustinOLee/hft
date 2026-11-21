#!/usr/bin/env python3
"""Where does the time go? (plan step 8, macOS edition)

    scripts/latency.py <log file>

macOS doesn't timestamp TCP packets in the kernel (SO_TIMESTAMP is UDP-only
there), so instead of plan step 8's kernel timestamps this measures what we
can from the log itself:

  1. Kalshi -> us: Kalshi stamps many messages with `sending_ts_ms`. Our
     receive time minus that = network travel + whatever our clock and
     Kalshi's disagree by. The clock part is unknown but roughly constant,
     so watch the SPREAD (how much it varies) more than the middle value.
  2. REST round trips: request written -> response complete.

Our own internal delay (event -> action reaches the network thread) is
printed by `hft replay --check-actions`.
"""
import json
import sys


def pct(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(p / 100 * len(xs)))]


def show(name, xs):
    if not xs:
        print(f"{name}: no samples")
        return
    print(f"{name}: {len(xs)} samples   "
          f"min {min(xs):.1f}  median {pct(xs, 50):.1f}  p90 {pct(xs, 90):.1f}  max {max(xs):.1f}  (ms)")


def main(path):
    exch, rest = [], []
    pending = {}  # conn -> time of the request waiting for a response
    for line in open(path):
        e = json.loads(line)
        ts_ms = e["ts"] / 1e6
        kind, conn = e["kind"], e["conn"]
        if kind == "ws_in":
            msg = json.loads(e["data"])
            sent = msg.get("sending_ts_ms")
            if sent:
                exch.append(ts_ms - sent)
        elif kind == "http_request" and conn == 2:
            pending[conn] = ts_ms
        elif kind == "http_response" and conn in pending:
            rest.append(ts_ms - pending.pop(conn))
    show("Kalshi -> us (incl. clock offset)", exch)
    show("REST round trip                  ", rest)


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    main(sys.argv[1])
