#!/bin/sh
# Pure LaunchAgent renderer for install_node.sh. Keeping rendering separate
# makes the policy-bearing argument list testable without building, minting
# secrets, or touching launchd.
set -eu

if [ "$#" -lt 11 ] || [ "$#" -gt 15 ]; then
  echo "usage: render_node_plist.sh <label> <bin> <root> <port> <auth> <keys> <reviewers> <log> <repos-file> <newcomer-audit> <newcomer-adjudications> [protected-refs] [require-scope] [tls-cert] [tls-key]" >&2
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
REPOS_FILE=$9
NEWCOMER_AUDIT=${10}
NEWCOMER_ADJUDICATIONS=${11}
PROTECTED_REFS=${12:-}
# Any non-empty 13th argument turns on D26 replay containment: the node
# then admits only ops whose signed payload names this node's log and a
# head still in the window. Positional like [protected-refs], and empty
# means absent for the same reason: an installer branch can always pass
# both slots and let the marker files decide.
REQUIRE_SCOPE=${13:-}
# TLS is one decision, not two: cert and key arrive together or not at
# all, and their presence is what flips the bind from loopback to
# 0.0.0.0. Half a pair is refused rather than defaulted, because the
# node itself will refuse a non-loopback bind without TLS (invariant 9)
# and the renderer failing here beats a unit that crash-loops there.
TLS_CERT=${14:-}
TLS_KEY=${15:-}
if [ -n "$TLS_CERT$TLS_KEY" ] && { [ -z "$TLS_CERT" ] || [ -z "$TLS_KEY" ]; }; then
  echo "render_node_plist.sh: tls-cert and tls-key must be given together" >&2
  exit 2
fi

# The served repos come from a file (one `owner/name.git` per line,
# `#` comments and blank lines skipped) rather than a single positional
# argument, so adding a repo is an appended line and a reinstall instead
# of a renderer signature change. An empty list is refused: a dogfood
# node with no --create serves nothing and installs no pre-receive hook,
# which reads like success until the first push is never sequenced.
[ -f "$REPOS_FILE" ] || { echo "render_node_plist.sh: no repos file at $REPOS_FILE" >&2; exit 1; }
repo_count=0
while IFS= read -r repo; do
  case $repo in ''|\#*) continue ;; esac
  repo_count=$((repo_count + 1))
done < "$REPOS_FILE"
[ "$repo_count" -gt 0 ] || { echo "render_node_plist.sh: $REPOS_FILE lists no repos" >&2; exit 1; }

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
    <string>--newcomer-audit</string><string>$NEWCOMER_AUDIT</string>
    <string>--newcomer-adjudications</string><string>$NEWCOMER_ADJUDICATIONS</string>
PLIST_HEAD

if [ -n "$PROTECTED_REFS" ]; then
  cat <<PLIST_POLICY
    <string>--require-assignment</string>
    <string>--protected-refs</string><string>$PROTECTED_REFS</string>
    <string>--require-review</string>
PLIST_POLICY
fi

if [ -n "$REQUIRE_SCOPE" ]; then
  echo '    <string>--require-scope</string>'
fi

if [ -n "$TLS_CERT" ]; then
  cat <<PLIST_TLS
    <string>--bind</string><string>0.0.0.0</string>
    <string>--tls-cert</string><string>$TLS_CERT</string>
    <string>--tls-key</string><string>$TLS_KEY</string>
PLIST_TLS
else
  echo '    <string>--bind</string><string>127.0.0.1</string>'
fi
while IFS= read -r repo; do
  case $repo in ''|\#*) continue ;; esac
  printf '    <string>--create</string><string>%s</string>\n' "$repo"
done < "$REPOS_FILE"

cat <<PLIST_TAIL
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>$LOG</string>
  <key>StandardErrorPath</key><string>$LOG</string>
</dict>
</plist>
PLIST_TAIL
