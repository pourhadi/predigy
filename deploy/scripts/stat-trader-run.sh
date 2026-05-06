#!/bin/zsh
# Stat-trader launcher.
#
# Long-running daemon: load StatRule list from disk, subscribe to
# the named Kalshi markets via WS, fire when model probability
# vs market quote clears the per-rule edge threshold (sized by
# Kelly).  Persistent OMS state across restarts.
#
# Required env from ~/.zprofile:
#   KALSHI_KEY_ID
#
# Risk caps default to small values for shake-down ($5 account-
# wide gross, $2 daily loss).  Raise via env override after
# validation.

set -euo pipefail

PREDIGY_HOME="${PREDIGY_HOME:-$HOME/code/predigy}"
CONFIG_DIR="${HOME}/.config/predigy"
LOG_DIR="${HOME}/Library/Logs/predigy"
mkdir -p "$CONFIG_DIR" "$LOG_DIR"

KALSHI_KEY_ID="${KALSHI_KEY_ID:?KALSHI_KEY_ID env var required}"
KALSHI_PEM="${KALSHI_PEM:-${CONFIG_DIR}/kalshi.pem}"
export KALSHI_KEY_ID KALSHI_PEM

cd "$PREDIGY_HOME"

# Refuse to start if no rules file exists — running without rules
# would just busy-loop.  The stat-curator writes this file after
# its first successful run.
if [ ! -s "${CONFIG_DIR}/stat-rules.json" ] || [ "$(cat "${CONFIG_DIR}/stat-rules.json")" = "[]" ]; then
    echo "[$(date -Iseconds)] stat-trader: no rules in ${CONFIG_DIR}/stat-rules.json — exiting"
    exit 1
fi

exec "./target/release/stat-trader" \
    --kalshi-key-id "$KALSHI_KEY_ID" \
    --kalshi-pem    "$KALSHI_PEM" \
    --rule-file     "${CONFIG_DIR}/stat-rules.json" \
    --strategy-id   "stat" \
    --bankroll-cents "${PREDIGY_STAT_BANKROLL_CENTS:-500}" \
    --kelly-factor  "${PREDIGY_STAT_KELLY_FACTOR:-0.25}" \
    --max-size      "${PREDIGY_STAT_MAX_SIZE:-3}" \
    --cooldown-ms   "${PREDIGY_STAT_COOLDOWN_MS:-60000}" \
    --max-contracts-per-side    "${PREDIGY_STAT_MAX_CONTRACTS_PER_SIDE:-3}" \
    --max-notional-cents-per-side "${PREDIGY_STAT_MAX_NOTIONAL_PER_SIDE:-200}" \
    --max-account-notional-cents  "${PREDIGY_STAT_MAX_ACCOUNT_NOTIONAL:-500}" \
    --max-daily-loss-cents        "${PREDIGY_STAT_MAX_DAILY_LOSS:-200}" \
    --max-orders-per-window       "${PREDIGY_STAT_MAX_ORDERS_PER_WINDOW:-5}" \
    --rate-window-ms              "${PREDIGY_STAT_RATE_WINDOW_MS:-1000}" \
    --cid-store    "${CONFIG_DIR}/oms-cids-stat" \
    --oms-state    "${CONFIG_DIR}/oms-state-stat.json"
