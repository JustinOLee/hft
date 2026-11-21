#!/usr/bin/env bash
# Determinism check (plan step 3).
#
# Runs live for up to N seconds (less if the strategy finishes), saving what
# the state machine prints, then replays the log that run produced and checks
# that the replay printed exactly the same and produced the same actions.
#
#   scripts/check_replay.sh <seconds> [live options] <TICKER> [TICKER...]
#
# Use it after every change to the state machine.
set -euo pipefail
cd "$(dirname "$0")/.."

if [ $# -lt 2 ]; then
    echo "usage: $0 <seconds> <TICKER> [TICKER...]" >&2
    exit 2
fi
secs=$1
shift
out=$(mktemp -d)
log="logs/check-$(date +%s).log"
mkdir -p logs

cargo build -q
echo "running live for ${secs}s on $* -> $log"
HFT_LOG="$log" ./target/debug/hft live "$@" > "$out/live.out" 2> "$out/live.err" &
pid=$!
# Wait up to $secs seconds, or until the program exits by itself.
for _ in $(seq 1 "$secs"); do
    kill -0 "$pid" 2>/dev/null || break
    sleep 1
done
kill "$pid" 2>/dev/null || true
wait "$pid" 2>/dev/null || true

./target/debug/hft replay --check-actions "$log" > "$out/replay.out" 2> "$out/replay.err" || {
    cat "$out/replay.err"
    exit 1
}
grep -E "^(PASS|FAIL)" "$out/replay.err" || true
grep " WARN " "$out/live.err" | head -5 || true

echo "$(wc -l < "$log" | tr -d ' ') events, $(wc -l < "$out/live.out" | tr -d ' ') output lines"
if cmp -s "$out/live.out" "$out/replay.out"; then
    echo "PASS: replay output identical to live"
else
    echo "FAIL: replay differs from live. First differences:"
    diff "$out/live.out" "$out/replay.out" | head -20
    exit 1
fi
