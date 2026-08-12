#!/bin/sh
# Pure systemd-unit renderer: the Linux sibling of render_node_plist.sh.
#
# The argument contract is identical to the plist renderer's, deliberately.
# One policy-bearing argument list supervised two ways means a flag added
# on one platform and forgotten on the other shows up as a diff between
# two files fed the same inputs, instead of as a node quietly running
# without a gate. Rendering stays separate from installing for the same
# reason it does on macOS: the argument list is testable without building,
# minting secrets, or touching a supervisor.
#
# Emits a *user* unit (~/.config/systemd/user), not a system unit, because
# that is what a LaunchAgent is: per-user, restarted on exit, started at
# load. Surviving logout needs `loginctl enable-linger <user>`, which
# install_node_linux.sh does.
set -eu

if [ "$#" -lt 11 ] || [ "$#" -gt 13 ]; then
  echo "usage: render_node_service.sh <label> <bin> <root> <port> <auth> <keys> <reviewers> <log> <repos-file> <newcomer-audit> <newcomer-adjudications> [protected-refs] [require-scope]" >&2
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
# Same contract as the plist renderer: any non-empty 13th argument emits
# --require-scope (D26 replay containment), empty means absent.
REQUIRE_SCOPE=${13:-}

# Same repos-file contract as the plist renderer: one repo per line,
# `#` comments and blank lines skipped, an empty list refused. The two
# loops must stay in lockstep or the argv comparison in install_policy.rs
# is what catches it.
[ -f "$REPOS_FILE" ] || { echo "render_node_service.sh: no repos file at $REPOS_FILE" >&2; exit 1; }

# ExecStart is assembled incrementally rather than interpolated in one
# line, so that an absent policy contributes no argument at all. Splicing
# a possibly-empty variable into the middle of the line instead leaves a
# double space, which systemd tolerates and an argument-list comparison
# against the plist renderer does not — and that comparison is the only
# thing keeping the two supervisors honest.
EXEC="$BIN $ROOT $PORT"
EXEC="$EXEC --auth-file $AUTH"
EXEC="$EXEC --keys-file $KEYS"
EXEC="$EXEC --reviewers-file $REVIEWERS"
EXEC="$EXEC --newcomer-audit $NEWCOMER_AUDIT"
EXEC="$EXEC --newcomer-adjudications $NEWCOMER_ADJUDICATIONS"
if [ -n "$PROTECTED_REFS" ]; then
  EXEC="$EXEC --require-assignment --protected-refs $PROTECTED_REFS --require-review"
fi
if [ -n "$REQUIRE_SCOPE" ]; then
  EXEC="$EXEC --require-scope"
fi
EXEC="$EXEC --bind 127.0.0.1"
repo_count=0
while IFS= read -r repo; do
  case $repo in ''|\#*) continue ;; esac
  EXEC="$EXEC --create $repo"
  repo_count=$((repo_count + 1))
done < "$REPOS_FILE"
[ "$repo_count" -gt 0 ] || { echo "render_node_service.sh: $REPOS_FILE lists no repos" >&2; exit 1; }

cat <<UNIT
[Unit]
Description=$LABEL — choir node (loopback bind, auth mandatory)
Documentation=file://$ROOT
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=$EXEC
Restart=always
RestartSec=2
StandardOutput=append:$LOG
StandardError=append:$LOG

[Install]
WantedBy=default.target
UNIT
