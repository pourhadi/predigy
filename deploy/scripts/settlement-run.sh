#!/bin/zsh
# Production launcher for settlement-trader (settlement-time sports).
#
# Wrapped by launchd (com.predigy.settlement.plist). The trader
# discovers live sports markets dynamically via Kalshi REST — no
# markets file needed. Override --series via PREDIGY_SETT_SERIES
# (space-separated) if you want a custom set; otherwise the default
# basket of per-event sports series is used.
#
# Required env (from ~/.zprofile, sourced by zsh -lc):
#   KALSHI_KEY_ID, KALSHI_PEM (or default path)
#   PREDIGY_LIVE=1 to enable real submission (default dry-run)

set -euo pipefail

PREDIGY_HOME="${PREDIGY_HOME:-$HOME/code/predigy}"
CONFIG_DIR="${HOME}/.config/predigy"
LOG_DIR="${HOME}/Library/Logs/predigy"
mkdir -p "$CONFIG_DIR" "$LOG_DIR"

KALSHI_KEY_ID="${KALSHI_KEY_ID:?KALSHI_KEY_ID env var required}"
KALSHI_PEM="${KALSHI_PEM:-${CONFIG_DIR}/kalshi.pem}"
export KALSHI_KEY_ID KALSHI_PEM

typeset -a SERIES_ARGS
if [[ -n "${PREDIGY_SETT_SERIES:-}" ]]; then
    for s in ${(z)PREDIGY_SETT_SERIES}; do
        SERIES_ARGS+=(--series "$s")
    done
fi

typeset -a EXTRA_ARGS
if [[ "${PREDIGY_LIVE:-0}" != "1" ]]; then
    EXTRA_ARGS+=(--dry-run)
fi

cd "$PREDIGY_HOME"

LIVE_LABEL="dry-run"
[[ "${PREDIGY_LIVE:-0}" == "1" ]] && LIVE_LABEL="LIVE"
echo "[$(date -Iseconds)] settlement: starting (mode=$LIVE_LABEL, series_overrides=${#SERIES_ARGS[@]:-0})"

exec "./target/release/settlement-trader" \
    --kalshi-key-id "$KALSHI_KEY_ID" \
    --kalshi-pem    "$KALSHI_PEM" \
    --strategy-id   "settlement" \
    --discovery-interval-secs      "${PREDIGY_SETT_DISCOVERY_SECS:-60}" \
    --max-secs-to-settle           "${PREDIGY_SETT_MAX_SECS_TO_SETTLE:-1800}" \
    --close-window-secs            "${PREDIGY_SETT_CLOSE_WINDOW_SECS:-600}" \
    --min-price-cents              "${PREDIGY_SETT_MIN_PRICE:-88}" \
    --max-price-cents              "${PREDIGY_SETT_MAX_PRICE:-96}" \
    --bid-to-ask-ratio             "${PREDIGY_SETT_BID_RATIO:-5}" \
    --size                         "${PREDIGY_SETT_SIZE:-1}" \
    --cooldown-ms                  "${PREDIGY_SETT_COOLDOWN_MS:-60000}" \
    --max-contracts-per-side       "${PREDIGY_SETT_MAX_CONTRACTS:-3}" \
    --max-notional-cents-per-side  "${PREDIGY_SETT_MAX_NOTIONAL_PER_SIDE:-300}" \
    --max-account-notional-cents   "${PREDIGY_SETT_MAX_ACCOUNT_NOTIONAL:-300}" \
    --max-daily-loss-cents         "${PREDIGY_SETT_MAX_DAILY_LOSS:-200}" \
    --max-orders-per-window        "${PREDIGY_SETT_MAX_ORDERS:-5}" \
    --rate-window-ms               "${PREDIGY_SETT_RATE_WINDOW_MS:-1000}" \
    --cid-store     "${CONFIG_DIR}/oms-cids-settlement" \
    --oms-state     "${CONFIG_DIR}/oms-state-settlement.json" \
    "${SERIES_ARGS[@]}" \
    "${EXTRA_ARGS[@]}"
