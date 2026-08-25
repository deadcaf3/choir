#!/bin/sh
# Schedule the offsite pull of the node's op log as a LaunchAgent, so the
# backup stops depending on an operator remembering to sync. Runs on the
# operator laptop (the machine holding ~/.choir/node-remote), never on
# the node host: the whole point of the pull direction is that the copy
# lives away from the disk that can lose it.
#
# StartInterval rather than a cron line because this is a laptop that
# sleeps; launchd runs a missed interval at wake instead of skipping it.
# pull_backup.sh verifies its own transfer (checksum, byte-prefix against
# the previous copy, seq contiguity), so a quiet log here means verified
# pulls, and a loud one is worth reading.
set -eu

INTERVAL=${1:-3600}
HERE="$(cd "$(dirname "$0")" && pwd)"
PULL="$HERE/pull_backup.sh"
LABEL=com.choir.pull-backup
PLIST=$HOME/Library/LaunchAgents/$LABEL.plist
LOG=$HOME/.choir/backup-pull.log

[ -f "$HOME/.choir/node-remote" ] || {
  echo "install_pull_timer: no ~/.choir/node-remote — this machine hosts the node," >&2
  echo "  so there is nothing to pull; the backup direction here is push_mirror.sh" >&2
  exit 1
}

mkdir -p "$HOME/Library/LaunchAgents"
cat > "$PLIST" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$LABEL</string>
  <key>ProgramArguments</key>
  <array>
    <string>/bin/sh</string>
    <string>$PULL</string>
  </array>
  <key>StartInterval</key><integer>$INTERVAL</integer>
  <key>StandardOutPath</key><string>$LOG</string>
  <key>StandardErrorPath</key><string>$LOG</string>
</dict>
</plist>
PLIST

launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
launchctl bootstrap "gui/$(id -u)" "$PLIST"
echo "loaded $LABEL: pulls the op log every ${INTERVAL}s (log: $LOG)"
