#!/bin/zsh
# Production launcher for latency-trader (weather strategy).
#
# Wrapped by launchd (com.predigy.latency-trader.plist). The plist's
# KeepAlive=true clause restarts this if it exits non-zero.
#
# All persistence flags are passed so a restart resumes:
#   --cid-store: cid sequence numbers (no duplicate-cid 409s on restart)
#   --oms-state: positions + daily P&L + kill-switch + orders
#   --nws-seen:  alert ids already processed (no re-fire of same alert)
#
# Required env (from ~/.zprofile, sourced by bash -lc):
#   KALSHI_KEY_ID, KALSHI_PEM (or default path), NWS_USER_AGENT
#   PREDIGY_LIVE=1 to enable real submission; default is dry-run

set -euo pipefail

PREDIGY_HOME="${PREDIGY_HOME:-$HOME/code/predigy}"
CONFIG_DIR="${HOME}/.config/predigy"
LOG_DIR="${HOME}/Library/Logs/predigy"
mkdir -p "$CONFIG_DIR" "$LOG_DIR"

KALSHI_KEY_ID="${KALSHI_KEY_ID:?KALSHI_KEY_ID env var required}"
KALSHI_PEM="${KALSHI_PEM:-${CONFIG_DIR}/kalshi.pem}"
NWS_USER_AGENT="${NWS_USER_AGENT:?NWS_USER_AGENT env var required, e.g. \"(predigy, you@example.com)\"}"
export KALSHI_KEY_ID KALSHI_PEM NWS_USER_AGENT

# Build the --nws-states arg list. Defaults to the union of states
# referenced by the curated rules — operators can override with
# PREDIGY_NWS_STATES, a space-separated list.
if [[ -z "${PREDIGY_NWS_STATES:-}" ]]; then
    if [[ -f "${CONFIG_DIR}/wx-rules.json" ]]; then
        PREDIGY_NWS_STATES=$(/usr/bin/python3 -c "
import json, sys
rules = json.load(open('${CONFIG_DIR}/wx-rules.json'))
states = set()
for r in rules:
    for s in (r.get('required_states') or []):
        if len(s) == 2:
            states.add(s)
print(' '.join(sorted(states)))
")
    fi
fi
PREDIGY_NWS_STATES="${PREDIGY_NWS_STATES:-TX FL CA GA NY IL}"

# Build the --nws-states args as an array so each "--nws-states X"
# pair is a discrete argv entry, not a single-string blob.
typeset -a STATE_ARGS
for s in ${=PREDIGY_NWS_STATES}; do
    STATE_ARGS+=(--nws-states "$s")
done

typeset -a EXTRA_ARGS
if [[ "${PREDIGY_LIVE:-0}" != "1" ]]; then
    EXTRA_ARGS+=(--dry-run)
fi

cd "$PREDIGY_HOME"

LIVE_LABEL="dry-run"
[[ "${PREDIGY_LIVE:-0}" == "1" ]] && LIVE_LABEL="LIVE"
echo "[$(date -Iseconds)] latency-trader: starting (mode=$LIVE_LABEL, states=$PREDIGY_NWS_STATES)"

# Defaults sized for a small ($50-$500) live account.
# Override via PREDIGY_* env vars if needed.
exec "./target/release/latency-trader" \
    --kalshi-key-id "$KALSHI_KEY_ID" \
    --kalshi-pem    "$KALSHI_PEM" \
    --rule-file     "${CONFIG_DIR}/wx-rules.json" \
    --nws-user-agent "$NWS_USER_AGENT" \
    --nws-poll-ms   "${PREDIGY_NWS_POLL_MS:-30000}" \
    --strategy-id   "wx" \
    --max-contracts-per-side       "${PREDIGY_MAX_CONTRACTS:-2}" \
    --max-notional-cents-per-side  "${PREDIGY_MAX_NOTIONAL_PER_SIDE:-200}" \
    --max-account-notional-cents   "${PREDIGY_MAX_ACCOUNT_NOTIONAL:-500}" \
    --max-daily-loss-cents         "${PREDIGY_MAX_DAILY_LOSS:-200}" \
    --max-orders-per-window        "${PREDIGY_MAX_ORDERS_PER_WINDOW:-5}" \
    --rate-window-ms               "${PREDIGY_RATE_WINDOW_MS:-1000}" \
    --cid-store     "${CONFIG_DIR}/oms-cids" \
    --oms-state     "${CONFIG_DIR}/oms-state.json" \
    --nws-seen      "${CONFIG_DIR}/wx-seen.json" \
    "${STATE_ARGS[@]}" \
    "${EXTRA_ARGS[@]}"
