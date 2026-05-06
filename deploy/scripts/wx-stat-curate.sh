#!/bin/zsh
# Cron wrapper for the wx-stat-curator.
#
# Each invocation = one tick: scan Kalshi for daily-temperature
# markets (KXHIGH* / KXLOW*), pull the NWS hourly forecast for each
# market's airport, compute model_p from the forecast vs threshold,
# and emit the resulting StatRule array to disk.
#
# Phase 1: inspection-only.  The output file is NOT yet wired into
# the running stat-trader — rules are deliberately quarantined for
# the operator to review before promotion.  When ready, copy or
# merge into ~/.config/predigy/stat-rules.json and let the regular
# stat-trader pick them up.
#
# Driven by launchd's StartCalendarInterval (com.predigy.wx-stat-curate).
# Required env from ~/.zprofile:
#   KALSHI_KEY_ID, NWS_USER_AGENT
# (No ANTHROPIC_API_KEY needed — wx-stat doesn't call Claude.)

set -euo pipefail

PREDIGY_HOME="${PREDIGY_HOME:-$HOME/code/predigy}"
CONFIG_DIR="${HOME}/.config/predigy"
LOG_DIR="${HOME}/Library/Logs/predigy"
mkdir -p "$CONFIG_DIR" "$LOG_DIR"

KALSHI_KEY_ID="${KALSHI_KEY_ID:?KALSHI_KEY_ID env var required}"
NWS_USER_AGENT="${NWS_USER_AGENT:?NWS_USER_AGENT env var required (set in ~/.zprofile)}"
KALSHI_PEM="${KALSHI_PEM:-${CONFIG_DIR}/kalshi.pem}"
export KALSHI_KEY_ID KALSHI_PEM NWS_USER_AGENT

cd "$PREDIGY_HOME"

echo "[$(date -Iseconds)] wx-stat-curate: tick"

exec "./target/release/wx-stat-curator" \
    --kalshi-key-id "$KALSHI_KEY_ID" \
    --kalshi-pem    "$KALSHI_PEM" \
    --user-agent    "$NWS_USER_AGENT" \
    --output        "${CONFIG_DIR}/wx-stat-rules.json" \
    --min-edge-cents     "${PREDIGY_WX_STAT_MIN_EDGE_CENTS:-5}" \
    --min-margin-f       "${PREDIGY_WX_STAT_MIN_MARGIN_F:-5}" \
    --write
