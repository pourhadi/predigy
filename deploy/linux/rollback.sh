#!/usr/bin/env bash
# Rollback from the Pi (Linux / systemd) back to the laptop (macOS / launchd).
#
# RUN ON THE LAPTOP. Drives the Pi via ssh.
#
# Use this in the first week post-cutover if the Pi misbehaves.
# Mirror of cutover.sh in the opposite direction:
#   1. arm Pi kill switch
#   2. stop Pi services
#   3. pg_dump Pi -> pg_restore laptop (only if Pi DB has newer state)
#   4. bring laptop launchd back up
#   5. disarm laptop kill switch

set -euo pipefail

PI_HOST="${PI_HOST:-dan@192.168.1.35}"
PI_PORT="${PI_PORT:-22}"
LAPTOP_CONFIG="${HOME}/.config/predigy"
DUMP_PATH="/tmp/predigy-rollback-$(date +%Y%m%d-%H%M%S).dump"

pi () { ssh -p "$PI_PORT" "$PI_HOST" "$@"; }
pause () {
    echo
    read -r -p "[next] $1 [enter to continue, ctrl-c to abort] " _ || exit 130
}

pause "arm Pi kill switch (Pi engine stops submitting)"
pi 'echo "rolling-back-to-laptop" > /home/dan/.config/predigy/kill-switch.flag'
sleep 10
pi 'psql -d predigy -At -c "SELECT COUNT(*) FROM intents WHERE status='\''submitted'\''"'

pause "stop Pi services"
pi 'systemctl --user stop predigy-engine.service predigy-dashboard.service'
pi 'for t in predigy-stat-curate predigy-cross-arb-curate predigy-arb-config-curate predigy-calibration predigy-paper-trader predigy-opportunity-scanner predigy-eval-daily predigy-db-backup; do systemctl --user stop "${t}.timer"; done'

pause "dump Pi DB and restore on laptop (overwrites laptop DB)"
pi "pg_dump -Fc predigy > /tmp/predigy-rollback.dump && ls -l /tmp/predigy-rollback.dump"
scp -P "$PI_PORT" "$PI_HOST:/tmp/predigy-rollback.dump" "$DUMP_PATH"
pg_restore -d predigy --clean --if-exists "$DUMP_PATH"
psql -d predigy -c "SELECT COUNT(*) FROM fills"

pause "bring laptop launchd services back up"
for plist in "$HOME"/Library/LaunchAgents/com.predigy.*.plist; do
    label=$(basename "$plist" .plist)
    launchctl bootstrap "gui/$(id -u)" "$plist" 2>/dev/null && echo "  loaded $label" || echo "  (already loaded: $label)"
done

pause "disarm laptop kill switch (laptop engine resumes submitting)"
: > "$LAPTOP_CONFIG/kill-switch.flag"
sleep 5
launchctl print "gui/$(id -u)/com.predigy.engine" | grep -E "state\s*=" | head -1

echo
echo "=== rollback complete ==="
echo "  laptop dashboard:  http://127.0.0.1:8080"
echo "  laptop engine log: tail -f $HOME/Library/Logs/predigy/engine.stderr.log"
echo "  the Pi DB has been preserved at: $DUMP_PATH (and pi:/tmp/predigy-rollback.dump)"
echo "  Pi services are stopped but installed — re-cutover via cutover.sh anytime."
