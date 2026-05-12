#!/usr/bin/env bash
# Daily pg_dump backup with 30-day rotation.
#
# Writes `predigy-YYYY-MM-DD.sql.gz` to PREDIGY_BACKUP_DIR (default
# ~/.config/predigy/backups). On Linux, the production env points
# this at the attached USB drive so the bulk capacity is used:
#   PREDIGY_BACKUP_DIR=/media/devmon/NAS/predigy/backups
#
# Sequential gzipped output is fine on HDD — fsync latency doesn't
# matter for this workload.

set -euo pipefail

CONFIG_DIR="${HOME}/.config/predigy"
BACKUP_DIR="${PREDIGY_BACKUP_DIR:-${CONFIG_DIR}/backups}"
DB_NAME="${PREDIGY_DB_NAME:-predigy}"
RETAIN_DAYS="${PREDIGY_BACKUP_RETAIN_DAYS:-30}"

mkdir -p "$BACKUP_DIR"

today=$(date +%Y-%m-%d)
out="$BACKUP_DIR/predigy-${today}.sql.gz"
tmp="${out}.partial"

echo "[$(date -Iseconds)] db-backup: pg_dump $DB_NAME -> $out"
pg_dump "$DB_NAME" | gzip -c > "$tmp"
mv "$tmp" "$out"

echo "[$(date -Iseconds)] db-backup: pruning files older than ${RETAIN_DAYS} days"
find "$BACKUP_DIR" -name 'predigy-*.sql.gz' -type f -mtime "+${RETAIN_DAYS}" -delete

echo "[$(date -Iseconds)] db-backup: done ($(du -h "$out" | cut -f1))"
