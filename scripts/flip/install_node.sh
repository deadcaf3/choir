#!/bin/zsh
# D20 flip prep, checklist item 1: install the dogfood choir-node as a
# supervised launchd agent, with auth mandatory.
#
# The rehearsal node ran bare in a scratch dir and died with the shell.
# This installs the real one: loopback bind (invariant 9 — no TLS, so no
# non-loopback bind), basic auth on, keys file for the platform API,
# reviewer pool for D24 layer-5 assignment, KeepAlive supervision, logs
# under ~/.choir/.
#
# Idempotent: safe to re-run after a rebuild (it reloads the agent).
# Creates no secrets in the repo; everything lives under ~/.choir at 0600.
set -euo pipefail

PORT=${1:-8417}
# Repo to serve. `--create` is idempotent (an existing repo is skipped),
# so it lives in the plist permanently: creation is also what installs
# the pre-receive hook that turns pushes into signed ops.
REPO=${2:-choir/choir.git}
STATE=$HOME/.choir
ROOT="$STATE/repos"
LABEL=com.choir.node
PLIST=$HOME/Library/LaunchAgents/$LABEL.plist
REPO_DIR="$(cd "$(dirname "$0")/../.." && pwd)"
BIN="$REPO_DIR/target/release/choir-node"

mkdir -p "$STATE" "$ROOT" "$HOME/Library/LaunchAgents"
chmod 700 "$STATE"

# 1. Binaries. Built release so the daemon is not a debug build.
cargo build --release --manifest-path "$REPO_DIR/Cargo.toml" -p choir-node -p choir-cli

# 2. Auth token (item 1: --auth-file is mandatory on the real node).
if [[ ! -f $STATE/auth ]]; then
  printf 'choir:%s\n' "$(openssl rand -hex 32)" > "$STATE/auth"
  chmod 600 "$STATE/auth"
  echo "minted $STATE/auth"
fi

# 3. Trusted keys for the platform API. Starts with the operator's own
#    agent key; more are appended later (hot-reloaded, no restart).
if [[ ! -f $STATE/keys ]]; then
  : > "$STATE/keys"
  chmod 600 "$STATE/keys"
  "$REPO_DIR/target/release/choir" key "$STATE/agent.key" >> "$STATE/keys"
  echo "minted $STATE/agent.key and registered it in $STATE/keys"
fi

# 4. Reviewer pool (D24 layer 5). One name per line; a one-name pool
#    still beats self-selection, and the file is re-read per draw.
if [[ ! -f $STATE/reviewers ]]; then
  echo "# eligible reviewer names, one per line (re-read per draw)" > "$STATE/reviewers"
  chmod 600 "$STATE/reviewers"
fi

# 5. launchd agent. Absolute paths only (launchd has no shell, no PATH
#    expansion, no $HOME in program arguments).
cat > "$PLIST" <<PLIST_EOF
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
    <string>--auth-file</string><string>$STATE/auth</string>
    <string>--keys-file</string><string>$STATE/keys</string>
    <string>--reviewers-file</string><string>$STATE/reviewers</string>
    <string>--bind</string><string>127.0.0.1</string>
    <string>--create</string><string>$REPO</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>$STATE/node.log</string>
  <key>StandardErrorPath</key><string>$STATE/node.log</string>
</dict>
</plist>
PLIST_EOF

launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
launchctl bootstrap "gui/$(id -u)" "$PLIST"
echo "loaded $LABEL on 127.0.0.1:$PORT (logs: $STATE/node.log)"
echo "check: curl -s -u choir:\$(cut -d: -f2 $STATE/auth) http://127.0.0.1:$PORT/api/view"
