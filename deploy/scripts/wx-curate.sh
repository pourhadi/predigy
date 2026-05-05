#!/bin/zsh
# Daily curate-and-restart wrapper for the weather strategy.
#
# Run by launchd (com.predigy.wx-curate.plist) once per day.
# Re-curates the rule set against current Kalshi weather markets,
# then restarts latency-trader so it picks up the fresh rules.
# A no-op if the curator fails — preserves whatever rules were
# previously written rather than leaving an empty file.
#
# Layout assumed:
#   $PREDIGY_HOME/target/release/wx-curator
#   $PREDIGY_HOME/target/release/latency-trader
#   $PREDIGY_HOME/.config/kalshi.pem
#   $HOME/.config/predigy/wx-rules.json   (output)
#   $HOME/.config/predigy/oms-cids        (latency-trader cid store)
#   $HOME/.config/predigy/oms-state.json  (OMS state snapshot)
#   $HOME/.config/predigy/wx-seen.json    (NWS seen-id set)
#
# Required env (loaded from ~/.zprofile via bash -lc):
#   KALSHI_KEY_ID, ANTHROPIC_API_KEY

set -euo pipefail

PREDIGY_HOME="${PREDIGY_HOME:-$HOME/code/predigy}"
CONFIG_DIR="${HOME}/.config/predigy"
LOG_DIR="${HOME}/Library/Logs/predigy"
mkdir -p "$CONFIG_DIR" "$LOG_DIR"

RULES="${CONFIG_DIR}/wx-rules.json"
RULES_TMP="${RULES}.next"

KALSHI_KEY_ID="${KALSHI_KEY_ID:?KALSHI_KEY_ID env var required}"
KALSHI_PEM="${KALSHI_PEM:-${CONFIG_DIR}/kalshi.pem}"
ANTHROPIC_API_KEY="${ANTHROPIC_API_KEY:?ANTHROPIC_API_KEY env var required}"
export KALSHI_KEY_ID KALSHI_PEM ANTHROPIC_API_KEY

cd "$PREDIGY_HOME"

echo "[$(date -Iseconds)] wx-curate: starting"
"./target/release/wx-curator" \
    --kalshi-key-id "$KALSHI_KEY_ID" \
    --kalshi-pem    "$KALSHI_PEM" \
    --batch-size    30 \
    --max-batches   15 \
    --output        "$RULES_TMP" \
    --write
mv "$RULES_TMP" "$RULES"
RULE_COUNT=$(grep -c '"kalshi_market"' "$RULES" 2>/dev/null || echo 0)
echo "[$(date -Iseconds)] wx-curate: wrote $RULE_COUNT rules to $RULES"

# Signal latency-trader (loaded by the other plist) to restart and
# pick up fresh rules. launchd will auto-relaunch it.
launchctl kickstart -k gui/$(id -u)/com.predigy.latency-trader || true
echo "[$(date -Iseconds)] wx-curate: triggered latency-trader reload"
