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
# Output file (wx-stat-rules.json) is consumed directly by the
# consolidated engine's wx-stat strategy when PREDIGY_WX_STAT_RULE_FILE
# points at it. Same-day/past temperature markets are gated through ASOS
# observed extremes before forecast/NBM scoring. Each run also writes a
# coverage/skip report for scanner/calibration surfacing. Accepted rules are
# also shadow-written to Postgres as disabled wx-stat rules plus model_p
# snapshots; the engine still consumes only the JSON rule file for live wx-stat.
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
    "./target/release/wx-stat-curator" \
        --database-url       "${DATABASE_URL:-postgresql:///predigy}" \
        --kalshi-key-id      "$KALSHI_KEY_ID" \
        --kalshi-pem         "$KALSHI_PEM" \
        --user-agent         "$NWS_USER_AGENT" \
        --output             "${CONFIG_DIR}/wx-stat-rules.json" \
        --min-edge-cents     "${PREDIGY_WX_STAT_MIN_EDGE_CENTS:-5}" \
        --nbm \
        --nbm-cache          "${DATA_DIR}/nbm_cache" \
        --observations-cache "${DATA_DIR}/wx_stat_observations" \
        --nbm-predictions-dir "${DATA_DIR}/wx_stat_predictions" \
        --nbm-calibration    "${DATA_DIR}/wx_stat_calibration.json" \
        --coverage-report-out "${DATA_DIR}/wx_stat_coverage_latest.json" \
        --shadow-db \
        --write
else
    "./target/release/wx-stat-curator" \
        --database-url   "${DATABASE_URL:-postgresql:///predigy}" \
        --kalshi-key-id  "$KALSHI_KEY_ID" \
        --kalshi-pem     "$KALSHI_PEM" \
        --user-agent     "$NWS_USER_AGENT" \
        --output         "${CONFIG_DIR}/wx-stat-rules.json" \
        --min-edge-cents "${PREDIGY_WX_STAT_MIN_EDGE_CENTS:-5}" \
        --min-margin-f   "${PREDIGY_WX_STAT_MIN_MARGIN_F:-5}" \
        --observations-cache "${DATA_DIR}/wx_stat_observations" \
        --coverage-report-out "${DATA_DIR}/wx_stat_coverage_latest.json" \
        --shadow-db \
        --write
fi
