#!/bin/zsh
# Production launcher for settlement-trader (settlement-time sports).
#
# Wrapped by launchd (com.predigy.settlement.plist). Reads market
# tickers from $HOME/.config/predigy/settlement-markets.txt — one
# Kalshi ticker per line; lines starting with `#` are comments.
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

MARKETS_FILE="${CONFIG_DIR}/settlement-markets.txt"
if [[ ! -s "$MARKETS_FILE" ]]; then
    cat >&2 <<EOF
[$(date -Iseconds)] settlement-run: no markets configured at $MARKETS_FILE
  Format: one Kalshi ticker per line. Lines starting with # are comments.
  Pick markets within ~24h of close, sports preferred (NBA, MLB, NHL).
  Example file:
      # NBA closes within 4h
      KXNBASERIES-26PHINYKR2-NYK
      KXNBASERIES-26LALOKCR2-OKC
EOF
    exit 1
fi

typeset -a MARKET_ARGS
while IFS= read -r line; do
    line="${line%%#*}"          # strip comments
    line="${line//[[:space:]]/}"
    [[ -z "$line" ]] && continue
    MARKET_ARGS+=(--market "$line")
done < "$MARKETS_FILE"

typeset -a EXTRA_ARGS
if [[ "${PREDIGY_LIVE:-0}" != "1" ]]; then
    EXTRA_ARGS+=(--dry-run)
fi

cd "$PREDIGY_HOME"

LIVE_LABEL="dry-run"
[[ "${PREDIGY_LIVE:-0}" == "1" ]] && LIVE_LABEL="LIVE"
echo "[$(date -Iseconds)] settlement: starting (mode=$LIVE_LABEL, markets=${#MARKET_ARGS[@]:-0})"

exec "./target/release/settlement-trader" \
    --kalshi-key-id "$KALSHI_KEY_ID" \
    --kalshi-pem    "$KALSHI_PEM" \
    --strategy-id   "settlement" \
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
    "${MARKET_ARGS[@]}" \
    "${EXTRA_ARGS[@]}"
