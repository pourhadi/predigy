#!/bin/zsh
# Wrapper for the predigy-import scheduled job. See
# docs/ARCHITECTURE.md "Phase 1" — keeps Postgres in sync with the
# legacy JSON state files until each strategy ports to the engine.

set -euo pipefail

PREDIGY_HOME="${PREDIGY_HOME:-$HOME/code/predigy}"
LOG_DIR="${HOME}/Library/Logs/predigy"
mkdir -p "$LOG_DIR"

cd "$PREDIGY_HOME"
echo "[$(date -Iseconds)] predigy-import: tick"

exec "./target/release/predigy-import" \
    --database-url "${DATABASE_URL:-postgresql:///predigy}" \
    --config-dir   "${HOME}/.config/predigy"
