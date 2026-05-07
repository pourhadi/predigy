#!/bin/zsh
# Cron wrapper for the wx-stat-curator.
#
# Each invocation = one tick: scan Kalshi for daily-temperature
# markets (KXHIGH* / KXLOW*), pull the NBM probabilistic-quantile
# forecast at the relevant grid cells, compute model_p via CDF
# interpolation at the Kalshi-side threshold, and emit the
# resulting StatRule array to disk.
#
# **Phase 2 NBM is on by default.** The probabilistic path replaces
# the Phase 1 conviction-zone gate with calibrated probabilities —
# real model_p values across the full 0..1 range. To revert to
# Phase 1 (deterministic NWS hourly forecast + 5°F margin gate),
# set PREDIGY_WX_STAT_PHASE=1 in ~/.zprofile.
#
# Output file (wx-stat-rules.json) is NOT yet wired into the
# running stat-trader — rules are deliberately quarantined for
# the operator to review before promotion. When ready, copy or
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
DATA_DIR="${PREDIGY_HOME}/data"
LOG_DIR="${HOME}/Library/Logs/predigy"
mkdir -p "$CONFIG_DIR" "$LOG_DIR" "$DATA_DIR"

KALSHI_KEY_ID="${KALSHI_KEY_ID:?KALSHI_KEY_ID env var required}"
NWS_USER_AGENT="${NWS_USER_AGENT:?NWS_USER_AGENT env var required (set in ~/.zprofile)}"
KALSHI_PEM="${KALSHI_PEM:-${CONFIG_DIR}/kalshi.pem}"
export KALSHI_KEY_ID KALSHI_PEM NWS_USER_AGENT

cd "$PREDIGY_HOME"

PHASE="${PREDIGY_WX_STAT_PHASE:-2}"
echo "[$(date -Iseconds)] wx-stat-curate: tick (phase=${PHASE})"

if [[ "$PHASE" == "2" ]]; then
    exec "./target/release/wx-stat-curator" \
        --kalshi-key-id      "$KALSHI_KEY_ID" \
        --kalshi-pem         "$KALSHI_PEM" \
        --user-agent         "$NWS_USER_AGENT" \
        --output             "${CONFIG_DIR}/wx-stat-rules.json" \
        --min-edge-cents     "${PREDIGY_WX_STAT_MIN_EDGE_CENTS:-5}" \
        --nbm \
        --nbm-cache          "${DATA_DIR}/nbm_cache" \
        --nbm-predictions-dir "${DATA_DIR}/wx_stat_predictions" \
        --nbm-calibration    "${DATA_DIR}/wx_stat_calibration.json" \
        --write
else
    exec "./target/release/wx-stat-curator" \
        --kalshi-key-id  "$KALSHI_KEY_ID" \
        --kalshi-pem     "$KALSHI_PEM" \
        --user-agent     "$NWS_USER_AGENT" \
        --output         "${CONFIG_DIR}/wx-stat-rules.json" \
        --min-edge-cents "${PREDIGY_WX_STAT_MIN_EDGE_CENTS:-5}" \
        --min-margin-f   "${PREDIGY_WX_STAT_MIN_MARGIN_F:-5}" \
        --write
fi
