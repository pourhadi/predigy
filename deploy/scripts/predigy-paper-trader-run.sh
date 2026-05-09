#!/bin/zsh
# One-shot paper-trader tick.
#
# Reads the latest stat-curator rules JSON, fetches live Kalshi
# touches, and records a paper_trades row for any rule whose
# computed edge clears its threshold. Then reconciles any
# settled-but-unscored paper trades.
#
# This does NOT submit any orders. It is the evidence-gathering
# layer that gates whether the `stat` strategy gets re-enabled
# for live trading.

set -euo pipefail

PREDIGY_HOME="${PREDIGY_HOME:-$HOME/code/predigy}"
CONFIG_DIR="${HOME}/.config/predigy"
LOG_DIR="${HOME}/Library/Logs/predigy"
mkdir -p "$CONFIG_DIR" "$LOG_DIR"

export DATABASE_URL="${DATABASE_URL:-postgresql:///predigy}"
RULES_FILE="${PREDIGY_PAPER_TRADER_RULES_FILE:-${CONFIG_DIR}/stat-rules.json}"

cd "$PREDIGY_HOME"

if [[ ! -f "$RULES_FILE" ]]; then
    echo "[$(date -Iseconds)] paper-trader: skipping — rules file not found at $RULES_FILE"
    exit 0
fi

echo "[$(date -Iseconds)] paper-trader: record from $RULES_FILE"
"./target/release/predigy-paper-trader" \
    --database-url "$DATABASE_URL" \
    record \
    --rules-file "$RULES_FILE" \
    --strategy stat \
    --source stat-curator

echo "[$(date -Iseconds)] paper-trader: reconcile settled paper trades"
"./target/release/predigy-paper-trader" \
    --database-url "$DATABASE_URL" \
    reconcile \
    --limit 200
