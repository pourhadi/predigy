#!/bin/zsh
# Cron wrapper for the incremental cross-arb-curator.
#
# Each invocation = one tick: load state, scan Kalshi + Polymarket,
# only call Claude on NEW Polymarket candidates (`seen_poly_ids` in
# the state file gates that), drop pairs whose Kalshi side settled,
# write the pair file + state, kickstart the trader if the pair set
# changed.
#
# Driven by launchd's StartInterval (com.predigy.cross-arb-curate).
# Required env from ~/.zprofile:
#   KALSHI_KEY_ID, ANTHROPIC_API_KEY
#   PREDIGY_LIVE=1 if you want changes to actually restart the
#                 trader; otherwise the kickstart is harmless.

set -euo pipefail

PREDIGY_HOME="${PREDIGY_HOME:-$HOME/code/predigy}"
CONFIG_DIR="${HOME}/.config/predigy"
LOG_DIR="${HOME}/Library/Logs/predigy"
mkdir -p "$CONFIG_DIR" "$LOG_DIR"

KALSHI_KEY_ID="${KALSHI_KEY_ID:?KALSHI_KEY_ID env var required}"
ANTHROPIC_API_KEY="${ANTHROPIC_API_KEY:?ANTHROPIC_API_KEY env var required}"
KALSHI_PEM="${KALSHI_PEM:-${CONFIG_DIR}/kalshi.pem}"
export KALSHI_KEY_ID KALSHI_PEM ANTHROPIC_API_KEY

cd "$PREDIGY_HOME"

echo "[$(date -Iseconds)] cross-arb-curate: tick"

exec "./target/release/cross-arb-curator" \
    --kalshi-key-id "$KALSHI_KEY_ID" \
    --kalshi-pem    "$KALSHI_PEM" \
    --output        "${CONFIG_DIR}/cross-arb-pairs.txt" \
    --state         "${CONFIG_DIR}/cross-arb-state.json" \
    --max-poly          "${PREDIGY_CURATE_MAX_POLY:-100}" \
    --batch-size        "${PREDIGY_CURATE_BATCH:-25}" \
    --max-batches       "${PREDIGY_CURATE_MAX_BATCHES:-4}" \
    --min-poly-liquidity "${PREDIGY_CURATE_MIN_LIQUIDITY:-5000}" \
    --max-days-to-settle "${PREDIGY_CURATE_MAX_DAYS:-60}" \
    --restart-job       "com.predigy.cross-arb" \
    --write
