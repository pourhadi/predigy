#!/bin/zsh
# Production launcher for predigy-dashboard.
#
# Wires the dashboard to all 3 strategy daemons' state files +
# logs, and to the shared kill-switch flag file. Bind on 0.0.0.0
# so the phone can reach it via LAN or Tailscale.
#
# Required env (from ~/.zprofile, sourced by zsh -lc):
#   KALSHI_KEY_ID, KALSHI_PEM (or default path)

set -euo pipefail

PREDIGY_HOME="${PREDIGY_HOME:-$HOME/code/predigy}"
CONFIG_DIR="${HOME}/.config/predigy"
LOG_DIR="${HOME}/Library/Logs/predigy"

KALSHI_KEY_ID="${KALSHI_KEY_ID:?KALSHI_KEY_ID env var required}"
KALSHI_PEM="${KALSHI_PEM:-${CONFIG_DIR}/kalshi.pem}"
export KALSHI_KEY_ID KALSHI_PEM

cd "$PREDIGY_HOME"

exec "./target/release/predigy-dashboard" \
    --bind          "${PREDIGY_DASH_BIND:-0.0.0.0:8080}" \
    --kalshi-key-id "$KALSHI_KEY_ID" \
    --kalshi-pem    "$KALSHI_PEM" \
    --kill-flag     "${CONFIG_DIR}/kill-switch.flag" \
    --strategy "weather=${CONFIG_DIR}/oms-state.json:${LOG_DIR}/latency-trader.stderr.log" \
    --strategy "settlement=${CONFIG_DIR}/oms-state-settlement.json:${LOG_DIR}/settlement.stderr.log" \
    --strategy "cross-arb=${CONFIG_DIR}/oms-state-cross-arb.json:${LOG_DIR}/cross-arb.stderr.log"
