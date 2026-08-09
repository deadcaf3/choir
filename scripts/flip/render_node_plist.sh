#!/bin/sh
# Pure LaunchAgent renderer for install_node.sh. Keeping rendering separate
# makes the policy-bearing argument list testable without building, minting
# secrets, or touching launchd.
set -eu

if [ "$#" -ne 9 ] && [ "$#" -ne 10 ]; then
  echo "usage: render_node_plist.sh <label> <bin> <root> <port> <auth> <keys> <reviewers> <log> <repo> [protected-refs]" >&2
  exit 2
fi

LABEL=$1
BIN=$2
ROOT=$3
PORT=$4
AUTH=$5
KEYS=$6
REVIEWERS=$7
LOG=$8
REPO=$9
PROTECTED_REFS=${10:-}

cat <<PLIST_HEAD
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$LABEL</string>
  <key>ProgramArguments</key>
  <array>
    <string>$BIN</string>
    <string>$ROOT</string>
    <string>$PORT</string>
    <string>--auth-file</string><string>$AUTH</string>
    <string>--keys-file</string><string>$KEYS</string>
    <string>--reviewers-file</string><string>$REVIEWERS</string>
PLIST_HEAD

if [ -n "$PROTECTED_REFS" ]; then
  cat <<PLIST_POLICY
    <string>--require-assignment</string>
    <string>--protected-refs</string><string>$PROTECTED_REFS</string>
    <string>--require-review</string>
PLIST_POLICY
fi

cat <<PLIST_TAIL
    <string>--bind</string><string>127.0.0.1</string>
    <string>--create</string><string>$REPO</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>$LOG</string>
  <key>StandardErrorPath</key><string>$LOG</string>
</dict>
</plist>
PLIST_TAIL
