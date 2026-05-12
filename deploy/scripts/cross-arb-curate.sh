#!/bin/zsh
# Cron wrapper for the incremental cross-arb-curator.
#
# Each invocation = one tick: load state, scan Kalshi + Polymarket,
# only call Claude on NEW Polymarket candidates (`seen_poly_ids` in
# the state file gates that), drop pairs whose Kalshi side settled,
# write the pair file + state. The consolidated predigy-engine
# hot-reloads this file via its pair-file service; legacy trader
# restart hooks are intentionally not used.
#
# Driven by launchd's StartInterval (com.predigy.cross-arb-curate).
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

echo "[$(date -Iseconds)] cross-arb-curate: tick"

exec "./target/release/cross-arb-curator" \
    --kalshi-key-id "$KALSHI_KEY_ID" \
    --kalshi-pem    "$KALSHI_PEM" \
    --output        "${CONFIG_DIR}/cross-arb-pairs.txt" \
    --state         "${CONFIG_DIR}/cross-arb-state.json" \
    --max-poly          "${PREDIGY_CURATE_MAX_POLY:-200}" \
    --batch-size        "${PREDIGY_CURATE_BATCH:-25}" \
    --max-batches       "${PREDIGY_CURATE_MAX_BATCHES:-8}" \
    --min-poly-liquidity "${PREDIGY_CURATE_MIN_LIQUIDITY:-2000}" \
    --max-days-to-settle "${PREDIGY_CURATE_MAX_DAYS:-90}" \
    --write
