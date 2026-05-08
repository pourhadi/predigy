#!/bin/zsh
# One-shot calibration evidence tick.
#
# Backfills public Kalshi outcomes for predicted tickers, reconciles
# settled venue-flat stale DB positions, then writes reliability
# reports. This does not enable any stat rules or submit orders.

set -euo pipefail

PREDIGY_HOME="${PREDIGY_HOME:-$HOME/code/predigy}"
CONFIG_DIR="${HOME}/.config/predigy"
LOG_DIR="${HOME}/Library/Logs/predigy"
mkdir -p "$CONFIG_DIR" "$LOG_DIR"

export DATABASE_URL="${DATABASE_URL:-postgresql:///predigy}"
KALSHI_KEY_ID="${KALSHI_KEY_ID:?KALSHI_KEY_ID env var required for venue-flat reconciliation}"
KALSHI_PEM="${KALSHI_PEM:-${CONFIG_DIR}/kalshi.pem}"
export KALSHI_KEY_ID KALSHI_PEM
WINDOW_DAYS="${PREDIGY_CALIBRATION_WINDOW_DAYS:-90}"
SYNC_LIMIT="${PREDIGY_CALIBRATION_SYNC_LIMIT:-200}"
RECONCILE_LIMIT="${PREDIGY_CALIBRATION_RECONCILE_LIMIT:-100}"

cd "$PREDIGY_HOME"

echo "[$(date -Iseconds)] predigy-calibration: sync settlements"
"./target/release/predigy-calibration" \
    --database-url "$DATABASE_URL" \
    sync-settlements \
    --window-days "$WINDOW_DAYS" \
    --limit "$SYNC_LIMIT"

echo "[$(date -Iseconds)] predigy-calibration: reconcile settled venue-flat DB positions"
"./target/release/predigy-calibration" \
    --database-url "$DATABASE_URL" \
    --kalshi-key-id "$KALSHI_KEY_ID" \
    --kalshi-pem "$KALSHI_PEM" \
    reconcile-venue-flat \
    --limit "$RECONCILE_LIMIT" \
    --write

for strategy in stat wx-stat; do
    echo "[$(date -Iseconds)] predigy-calibration: report ${strategy}"
    "./target/release/predigy-calibration" \
        --database-url "$DATABASE_URL" \
        report \
        --strategy "$strategy" \
        --window-days "$WINDOW_DAYS"
done
