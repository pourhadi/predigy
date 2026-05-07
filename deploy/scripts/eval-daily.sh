#!/bin/zsh
# predigy-eval daily report runner.
#
# Run by com.predigy.eval-daily at 23:55 local. Emits a 24h
# markdown report to ~/Library/Logs/predigy/eval/YYYY-MM-DD.md
# and updates a `latest.md` symlink the operator can quick-glance.
#
# Exit code mirrors `predigy-eval report` — non-zero when any
# strategy carries a critical diagnosis. The operator's existing
# log-watcher (or a manual cron-mail setup) can route the failure
# to push notifications.

set -euo pipefail

PREDIGY_HOME="${PREDIGY_HOME:-$HOME/code/predigy}"
LOG_DIR="${HOME}/Library/Logs/predigy/eval"
mkdir -p "$LOG_DIR"

today=$(date +%Y-%m-%d)
out="$LOG_DIR/$today.md"
latest="$LOG_DIR/latest.md"

cd "$PREDIGY_HOME"

# `report` writes to --out; we capture exit code separately so
# the symlink update + log line still run on critical-diag exit.
set +e
./target/release/predigy-eval report --since 24h --out "$out"
ec=$?
set -e

# Atomic-ish symlink swap.
ln -sfn "$today.md" "$latest"

echo "[$(date -Iseconds)] eval-daily: wrote $out (exit $ec)"
exit "$ec"
