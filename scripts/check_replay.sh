#!/usr/bin/env bash
# Determinism check (plan step 3).
#
# Runs live for N seconds, saving what the state machine prints, then replays
# the log that run produced and checks the replay printed exactly the same.
#
#   scripts/check_replay.sh [seconds] [coin]      defaults: 30 BTC
#
# Use it after every change to the state machine.
set -euo pipefail
cd "$(dirname "$0")/.."

secs=${1:-30}
coin=${2:-BTC}
out=$(mktemp -d)
log="logs/check-$(date +%s).log"
mkdir -p logs

cargo build -q
echo "running live for ${secs}s on $coin -> $log"
./target/debug/hft live "$coin" "$log" > "$out/live.out" 2> "$out/live.err" &
pid=$!
sleep "$secs"
kill "$pid"
wait "$pid" 2>/dev/null || true

./target/debug/hft replay "$log" > "$out/replay.out" 2> "$out/replay.err"

echo "$(wc -l < "$log" | tr -d ' ') events, $(wc -l < "$out/live.out" | tr -d ' ') output lines"
if cmp -s "$out/live.out" "$out/replay.out"; then
    echo "PASS: replay output identical to live"
else
    echo "FAIL: replay differs from live. First differences:"
    diff "$out/live.out" "$out/replay.out" | head -20
    exit 1
fi
