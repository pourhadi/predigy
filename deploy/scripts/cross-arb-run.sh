#!/bin/zsh
# Production launcher for cross-arb-trader (Kalshi vs Polymarket).
#
# **Currently a scaffold — you need to author $CROSS_ARB_PAIRS_FILE
# before this can run.** See `docs/SESSIONS.md` for hints on choosing
# Kalshi/Polymarket pairs.
#
# Wrapped by launchd (com.predigy.cross-arb.plist) when installed.
# Reads from $HOME/.config/predigy/cross-arb-pairs.txt — one
# `KALSHI_TICKER=POLYMARKET_ASSET_ID` per line.
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

PAIRS_FILE="${CONFIG_DIR}/cross-arb-pairs.txt"
if [[ ! -s "$PAIRS_FILE" ]]; then
    cat >&2 <<EOF
[$(date -Iseconds)] cross-arb-run: no pairs configured at $PAIRS_FILE
  Format: one "KALSHI_TICKER=POLYMARKET_ASSET_ID" per line.
  See docs/SESSIONS.md for hints on what pairs to start with.
EOF
    exit 1
fi

typeset -a PAIR_ARGS
while IFS= read -r line; do
    line="${line%%#*}"          # strip comments
    line="${line//[[:space:]]/}"
    [[ -z "$line" ]] && continue
    PAIR_ARGS+=(--pair "$line")
done < "$PAIRS_FILE"

typeset -a EXTRA_ARGS
if [[ "${PREDIGY_LIVE:-0}" != "1" ]]; then
    EXTRA_ARGS+=(--dry-run)
fi

cd "$PREDIGY_HOME"

LIVE_LABEL="dry-run"
[[ "${PREDIGY_LIVE:-0}" == "1" ]] && LIVE_LABEL="LIVE"
echo "[$(date -Iseconds)] cross-arb: starting (mode=$LIVE_LABEL, pairs=${#PAIR_ARGS[@]:-0})"

# Defaults sized for a small live account. Override via PREDIGY_*.
exec "./target/release/cross-arb-trader" \
    --kalshi-key-id "$KALSHI_KEY_ID" \
    --kalshi-pem    "$KALSHI_PEM" \
    --strategy-id   "cross-arb" \
    --max-contracts-per-side       "${PREDIGY_CROSS_MAX_CONTRACTS:-2}" \
    --max-notional-cents-per-side  "${PREDIGY_CROSS_MAX_NOTIONAL_PER_SIDE:-200}" \
    --max-account-notional-cents   "${PREDIGY_CROSS_MAX_ACCOUNT_NOTIONAL:-300}" \
    --max-daily-loss-cents         "${PREDIGY_CROSS_MAX_DAILY_LOSS:-200}" \
    --max-orders-per-window        "${PREDIGY_CROSS_MAX_ORDERS_PER_WINDOW:-5}" \
    --rate-window-ms               "${PREDIGY_CROSS_RATE_WINDOW_MS:-1000}" \
    --min-edge-cents               "${PREDIGY_CROSS_MIN_EDGE:-3}" \
    --max-size                     "${PREDIGY_CROSS_MAX_SIZE:-2}" \
    --cooldown-ms                  "${PREDIGY_CROSS_COOLDOWN_MS:-1000}" \
    --cid-store     "${CONFIG_DIR}/oms-cids-cross-arb" \
    "${PAIR_ARGS[@]}" \
    "${EXTRA_ARGS[@]}"
