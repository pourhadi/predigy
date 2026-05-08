#!/bin/zsh
# One-shot calibration evidence tick.
#
# Backfills public Kalshi outcomes for predicted tickers, then writes
# reliability reports. This is evidence/surfacing only; it does not
# enable any stat rules or submit orders.

set -euo pipefail

PREDIGY_HOME="${PREDIGY_HOME:-$HOME/code/predigy}"
LOG_DIR="${HOME}/Library/Logs/predigy"
mkdir -p "$LOG_DIR"

export DATABASE_URL="${DATABASE_URL:-postgresql:///predigy}"
WINDOW_DAYS="${PREDIGY_CALIBRATION_WINDOW_DAYS:-90}"
SYNC_LIMIT="${PREDIGY_CALIBRATION_SYNC_LIMIT:-200}"

cd "$PREDIGY_HOME"

echo "[$(date -Iseconds)] predigy-calibration: sync settlements"
"./target/release/predigy-calibration" \
    --database-url "$DATABASE_URL" \
    sync-settlements \
    --window-days "$WINDOW_DAYS" \
    --limit "$SYNC_LIMIT"

for strategy in stat wx-stat; do
    echo "[$(date -Iseconds)] predigy-calibration: report ${strategy}"
    "./target/release/predigy-calibration" \
        --database-url "$DATABASE_URL" \
        report \
        --strategy "$strategy" \
        --window-days "$WINDOW_DAYS"
done
