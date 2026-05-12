#!/bin/zsh
# Cron wrapper for the stat-curator.
#
# Each invocation = one tick: scan Kalshi for markets in the
# Sports/Politics/Elections/World/Economics categories settling
# within the configured horizon, call Claude on each batch to
# generate calibrated model probabilities, validate (probability
# range, confidence, edge gap, side direction), and write the
# resulting StatRule array to disk. It also shadow-writes disabled DB
# rules plus model_p snapshots for calibration evidence. The consolidated
# engine reads DB rules; legacy stat-trader restart hooks are intentionally
# not used.
#
# Driven by launchd's StartInterval (com.predigy.stat-curate).
# Required env from ~/.zprofile:
#   KALSHI_KEY_ID, ANTHROPIC_API_KEY

set -euo pipefail

PREDIGY_HOME="${PREDIGY_HOME:-$HOME/code/predigy}"
CONFIG_DIR="${HOME}/.config/predigy"
LOG_DIR="${PREDIGY_LOG_DIR:-${HOME}/Library/Logs/predigy}"
mkdir -p "$CONFIG_DIR" "$LOG_DIR"

KALSHI_KEY_ID="${KALSHI_KEY_ID:?KALSHI_KEY_ID env var required}"
ANTHROPIC_API_KEY="${ANTHROPIC_API_KEY:?ANTHROPIC_API_KEY env var required}"
KALSHI_PEM="${KALSHI_PEM:-${CONFIG_DIR}/kalshi.pem}"
export KALSHI_KEY_ID KALSHI_PEM ANTHROPIC_API_KEY

cd "$PREDIGY_HOME"

echo "[$(date -Iseconds)] stat-curate: tick"

exec "./target/release/stat-curator" \
    --kalshi-key-id "$KALSHI_KEY_ID" \
    --kalshi-pem    "$KALSHI_PEM" \
    --output        "${CONFIG_DIR}/stat-rules.json" \
    --batch-size         "${PREDIGY_STAT_CURATE_BATCH:-25}" \
    --max-batches        "${PREDIGY_STAT_CURATE_MAX_BATCHES:-4}" \
    --max-days-to-settle "${PREDIGY_STAT_CURATE_MAX_DAYS:-3}" \
    --shadow-db \
    --write
