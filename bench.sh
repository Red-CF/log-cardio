#!/bin/sh
# bench.sh — relative perf of cardio's modes on a synthetic 1M-line log.
# No framework dependency: hyperfine when present, a plain 3-run mean
# otherwise. Runs on the release build; nothing is tracked or stored.
#
#   ./bench.sh            # 1M lines
#   N=100000 ./bench.sh   # smaller run
set -eu
cd "$(dirname "$0")"
cargo build --release -q 2>&1 | grep -v Compiling || true
BIN="target/release/cardio"

LOG=$(mktemp /tmp/cardio-bench-XXXXXX.log)
trap 'rm -f "$LOG"' EXIT

N=${N:-1000000}
awk -v n="$N" 'BEGIN {
    # realistic mix: ~20% exact repeats (the dedup targets), digit-varying
    # ids (the stats folding target), occasional huge XML bodies (the
    # --max-field-len target), all with canonical timestamps
    for (i = 0; i < n; i++) {
        ts = sprintf("2026-10-%02dT%02d:%02d:%02dZ", (int(i/86400) % 28) + 1, int(i/3600) % 24, int(i/60) % 60, i % 60)
        if (i % 5 == 0)
            printf "%s INFO app: boot ok\n", ts
        else if (i % 10 == 2)
            printf "%s WARN app: db pool %d warmup\n", ts, i % 7
        else if (i % 50 == 13) {
            body = "<xml><payload>"; for (j = 0; j < 200; j++) body = body "x"
            printf "%s ERROR svc: %s</payload>\n", ts, body
        } else
            printf "%s INFO svc sid=%d request %d msg-%d\n", ts, i % 97, i % 13, i % 1000
    }
}' > "$LOG"
LINES=$(wc -l < "$LOG" | tr -d ' ')
SIZE=$(du -h "$LOG" | cut -f1 | tr -d '\t')
echo "input: $LINES lines ($SIZE)"

run_once() {
    # $@ = mode args; both channels to /dev/null (this is about throughput)
    "$@" >/dev/null 2>/dev/null
}

printf '%-28s %s\n' "MODE" "TIME"
printf '%-28s %s\n' "---" "----"

if command -v hyperfine >/dev/null; then
    # -N = default shell (no user sh rc); style basic = the standard table
    # BENCH_JSON=path: machine-readable results (the CI bench badge reads it)
    EXPORT=""
    [ -n "${BENCH_JSON:-}" ] && EXPORT="--export-json $BENCH_JSON"
    hyperfine --warmup 1 --min-runs 3 $EXPORT --style basic -N \
        "/bin/cat $LOG" \
        "$BIN $LOG" \
        "$BIN $LOG --raw" \
        "$BIN $LOG --stats" \
        "$BIN $LOG --query ERROR" \
        "$BIN $LOG --raw --trace 'sid=([0-9]+)'" || true
else
    echo "hyperfine not installed (brew install hyperfine better) — plain timing:"
    for spec in \
        "cat baseline|/bin/cat $LOG" \
        "lean (trim+dedup+cap)|$BIN $LOG" \
        "raw (no pipeline)|$BIN $LOG --raw" \
        "stats survey|$BIN $LOG --stats" \
        "query filter|$BIN $LOG --query ERROR" \
        "trace grouping (raw)|$BIN $LOG --raw --trace 'sid=([a-z0-9]+)'"; do
        label=${spec%%|*}; cmd=${spec#*|}
        # 3 warm + 3 timed runs, monotonic clock via python3 (date +%s is
        # 1s-resolution on macOS — useless at this scale)
        for _ in 1 2 3; do
            eval "$cmd" >/dev/null 2>/dev/null || true
        done
        t0=$(python3 -c 'import time; print(time.monotonic())')
        for _ in 1 2 3; do
            # || true: a mode erroring is a bench result too, not a script
            # death (set -e would abort the whole table)
            eval "$cmd" >/dev/null 2>/dev/null || true
        done
        t1=$(python3 -c 'import time; print(time.monotonic())')
        printf '%-28s %.3f s/3 runs\n' "$label" "$(python3 -c "print($t1 - $t0)")"
    done
fi