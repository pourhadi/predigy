#!/bin/zsh
# Cron wrapper for arb-config-curator.
#
# Each invocation = one tick: validate every ticker referenced
# in implication-arb-config.json + internal-arb-config.json
# against Kalshi REST `status=open`, drop settled/closed entries,
# seed new entries from active monotonic ladder series + 2-leg
# event families, atomic-rename writes. The implication-arb /
# internal-arb strategies hot-reload via 30s mtime poll, no
# engine bounce needed.
#
# Driven by launchd's StartInterval (com.predigy.arb-config-curate).

set -euo pipefail

PREDIGY_HOME="${PREDIGY_HOME:-$HOME/code/predigy}"
CONFIG_DIR="${HOME}/.config/predigy"
LOG_DIR="${PREDIGY_LOG_DIR:-${HOME}/Library/Logs/predigy}"
mkdir -p "$CONFIG_DIR" "$LOG_DIR"

KALSHI_KEY_ID="${KALSHI_KEY_ID:?KALSHI_KEY_ID env var required}"
KALSHI_PEM="${KALSHI_PEM:-${CONFIG_DIR}/kalshi.pem}"
export KALSHI_KEY_ID KALSHI_PEM

cd "$PREDIGY_HOME"

echo "[$(date -Iseconds)] arb-config-curate: tick"

exec "./target/release/arb-config-curator" \
    --kalshi-key-id "$KALSHI_KEY_ID" \
    --kalshi-pem    "$KALSHI_PEM" \
    --implication-config "${CONFIG_DIR}/implication-arb-config.json" \
    --internal-config    "${CONFIG_DIR}/internal-arb-config.json" \
    --write
