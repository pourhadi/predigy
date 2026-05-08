#!/bin/zsh
# Wrapper for the predigy-import scheduled job. This legacy JSON mirror
# was useful during migration but is disabled after the consolidated
# engine cutover because stale JSON can overwrite/reenable DB state.
# Set PREDIGY_ENABLE_LEGACY_IMPORT=1 explicitly for a one-off migration.

set -euo pipefail

PREDIGY_HOME="${PREDIGY_HOME:-$HOME/code/predigy}"
LOG_DIR="${HOME}/Library/Logs/predigy"
mkdir -p "$LOG_DIR"

cd "$PREDIGY_HOME"

if [[ "${PREDIGY_ENABLE_LEGACY_IMPORT:-0}" != "1" ]]; then
    echo "[$(date -Iseconds)] predigy-import: disabled (set PREDIGY_ENABLE_LEGACY_IMPORT=1 to run)"
    exit 0
fi

echo "[$(date -Iseconds)] predigy-import: tick"

exec "./target/release/predigy-import" \
    --database-url "${DATABASE_URL:-postgresql:///predigy}" \
    --config-dir   "${HOME}/.config/predigy"
