#!/bin/zsh
# One-shot read-only opportunity scanner tick.
#
# Writes ONLY opportunity_observations. It never links to the OMS and
# never writes intents/orders/fills/positions.

set -euo pipefail

PREDIGY_HOME="${PREDIGY_HOME:-$HOME/code/predigy}"
CONFIG_DIR="${HOME}/.config/predigy"
DATA_DIR="${PREDIGY_HOME}/data"
LOG_DIR="${PREDIGY_LOG_DIR:-${HOME}/Library/Logs/predigy}"
mkdir -p "$CONFIG_DIR" "$LOG_DIR" "$DATA_DIR"

export DATABASE_URL="${DATABASE_URL:-postgresql:///predigy}"
export PREDIGY_IMPLICATION_ARB_CONFIG="${PREDIGY_IMPLICATION_ARB_CONFIG:-${CONFIG_DIR}/implication-arb-config.json}"
export PREDIGY_INTERNAL_ARB_CONFIG="${PREDIGY_INTERNAL_ARB_CONFIG:-${CONFIG_DIR}/internal-arb-config.json}"
export PREDIGY_WX_STAT_COVERAGE_REPORT="${PREDIGY_WX_STAT_COVERAGE_REPORT:-${DATA_DIR}/wx_stat_coverage_latest.json}"

cd "$PREDIGY_HOME"

echo "[$(date -Iseconds)] opportunity-scanner: arb tick"
"./target/release/opportunity-scanner" \
    --database-url "$DATABASE_URL" \
    arb \
    --implication-config "$PREDIGY_IMPLICATION_ARB_CONFIG" \
    --internal-config "$PREDIGY_INTERNAL_ARB_CONFIG" \
    --quote-delay-ms "${PREDIGY_SCANNER_QUOTE_DELAY_MS:-500}" \
    --write-observations \
    --once

echo "[$(date -Iseconds)] opportunity-scanner: wx-stat coverage tick"
if [[ -f "$PREDIGY_WX_STAT_COVERAGE_REPORT" ]]; then
    "./target/release/opportunity-scanner" \
        --database-url "$DATABASE_URL" \
        wx-stat \
        --coverage-report "$PREDIGY_WX_STAT_COVERAGE_REPORT" \
        --write-observations
else
    echo "coverage report missing: $PREDIGY_WX_STAT_COVERAGE_REPORT"
fi

echo "[$(date -Iseconds)] opportunity-scanner: settlement config tick"
"./target/release/opportunity-scanner" \
    --database-url "$DATABASE_URL" \
    settlement \
    --write-observations
