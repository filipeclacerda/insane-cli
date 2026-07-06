#!/usr/bin/env bash
# Reproducible micro-benchmarks for insane-cli (POSIX / Linux / macOS).
#
# Measures:
#   1. Startup latency of `insane --help` and `insane config path`, N runs,
#      reports median/min/max (SPEC §9 target: < 50ms for --help).
#   2. Peak memory via `/usr/bin/time -v` (Linux) or `/usr/bin/time -l`
#      (macOS) when available; otherwise skipped with a note.
#
# Requires a release build: run `cargo build --release` first.
#
# Usage:
#   ./scripts/bench.sh [N]

set -euo pipefail

N="${1:-20}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${BIN_PATH:-$REPO_ROOT/target/release/insane}"

if [ ! -x "$BIN" ]; then
    echo "error: release binary not found at $BIN -- run 'cargo build --release' first." >&2
    exit 1
fi

now_ms() {
    # Nanosecond-precision wall clock, converted to milliseconds (float).
    python3 -c 'import time; print(time.time()*1000)' 2>/dev/null || \
        date +%s%3N
}

measure_startup() {
    local label="$1"; shift
    local times=()
    for _ in $(seq 1 "$N"); do
        local start end
        start=$(now_ms)
        "$BIN" "$@" >/dev/null
        end=$(now_ms)
        times+=("$(echo "$end - $start" | bc)")
    done
    local sorted
    sorted=$(printf '%s\n' "${times[@]}" | sort -n)
    local min median max
    min=$(echo "$sorted" | head -1)
    max=$(echo "$sorted" | tail -1)
    median=$(echo "$sorted" | awk -v n="$N" 'NR==int(n/2)+1{print; exit}')
    echo "$label: min=${min}ms median=${median}ms max=${max}ms (n=$N)"
}

echo "insane-cli benchmark -- $(date -Is 2>/dev/null || date)"
echo "Binary: $BIN"
echo "Runs per measurement: $N"
echo

echo "--- Startup latency ---"
measure_startup "insane --help" --help
measure_startup "insane config path" config path
echo

echo "--- Peak memory (single 'insane --help' run) ---"
if command -v /usr/bin/time >/dev/null 2>&1; then
    if /usr/bin/time -v true >/dev/null 2>&1; then
        /usr/bin/time -v "$BIN" --help >/dev/null 2>/tmp/insane_bench_time.$$ || true
        grep -i "maximum resident set size" /tmp/insane_bench_time.$$ || true
        rm -f /tmp/insane_bench_time.$$
    elif /usr/bin/time -l true >/dev/null 2>&1; then
        /usr/bin/time -l "$BIN" --help >/dev/null 2>/tmp/insane_bench_time.$$ || true
        grep -i "maximum resident set size" /tmp/insane_bench_time.$$ || true
        rm -f /tmp/insane_bench_time.$$
    else
        echo "note: /usr/bin/time present but supports neither -v nor -l; skipping memory measurement."
    fi
else
    echo "note: /usr/bin/time not available; skipping memory measurement."
fi

echo
echo "--- Summary (paste into docs/BENCHMARKS.md) ---"
echo "Re-run the two 'measure_startup' lines above and the memory line for the numbers table."
