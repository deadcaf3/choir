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

if [ "$#" -lt 11 ] || [ "$#" -gt 19 ]; then
  echo "usage: render_node_service.sh <label> <bin> <root> <port> <auth> <keys> <reviewers> <log> <repos-file> <newcomer-audit> <newcomer-adjudications> [protected-refs] [require-scope] [tls-cert] [tls-key] [acl] [accounts] [behind-tls-proxy] [private-beta-service-user]" >&2
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
# Same TLS contract as the plist renderer: cert and key together or not
# at all, and their presence flips the bind from loopback to 0.0.0.0.
TLS_CERT=${14:-}
TLS_KEY=${15:-}
# Same contract as the plist renderer: a non-empty 16th argument is the
# D29 ACL path, empty means no per-repository authorization.
ACL=${16:-}
# Same contract as the plist renderer: a non-empty 17th argument is the
# D36 accounts file, empty means invite-only self-service is off. It sits
# at 17 on both renderers rather than after the Linux-only service user,
# so every argument the two platforms share keeps the same position and a
# flag added to one and forgotten on the other is still a diff between
# two files fed the same inputs.
ACCOUNTS=${17:-}
# Same contract as the plist renderer: any non-empty 18th argument says a
# TLS-terminating proxy is in front, so the node writes its absolute URLs
# and its cookies as https even though its own listener is plaintext on
# loopback. Declared here rather than sniffed from X-Forwarded-Proto at
# run time, because an invite link is a bearer credential and the node
# cannot tell a header its proxy set from one a caller sent.
BEHIND_TLS_PROXY=${18:-}
PRIVATE_BETA=${19:-}
if [ -n "$TLS_CERT$TLS_KEY" ] && { [ -z "$TLS_CERT" ] || [ -z "$TLS_KEY" ]; }; then
  echo "render_node_service.sh: tls-cert and tls-key must be given together" >&2
  exit 2
fi
if [ -n "$PRIVATE_BETA" ]; then
  [ -n "$ACL" ] || { echo "render_node_service.sh: private beta needs an ACL" >&2; exit 2; }
  [ -n "$PROTECTED_REFS" ] \
    || { echo "render_node_service.sh: private beta needs protected refs and review gates" >&2; exit 2; }
  [ -n "$REQUIRE_SCOPE" ] \
    || { echo "render_node_service.sh: private beta needs --require-scope" >&2; exit 2; }
  [ -z "$TLS_CERT$TLS_KEY" ] \
    || { echo "render_node_service.sh: private beta terminates TLS at the reverse proxy; direct node TLS is refused" >&2; exit 2; }
  # The other half of that sentence. A beta node that refuses its own TLS
  # because a proxy terminates it must also write its URLs as the scheme
  # that proxy speaks, or every invite link it mints is http.
  [ -n "$BEHIND_TLS_PROXY" ] \
    || { echo "render_node_service.sh: private beta terminates TLS at a proxy, so it must be told it is behind one" >&2; exit 2; }
fi

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
if [ -n "$ACL" ]; then
  EXEC="$EXEC --acl-file $ACL"
fi
if [ -n "$ACCOUNTS" ]; then
  EXEC="$EXEC --accounts-file $ACCOUNTS"
fi
if [ -n "$BEHIND_TLS_PROXY" ]; then
  EXEC="$EXEC --behind-tls-proxy"
fi
if [ -n "$PROTECTED_REFS" ]; then
  EXEC="$EXEC --require-assignment --protected-refs $PROTECTED_REFS --require-review"
fi
if [ -n "$REQUIRE_SCOPE" ]; then
  EXEC="$EXEC --require-scope"
fi
if [ -n "$PRIVATE_BETA" ]; then
  STATE_DIR=$(dirname "$AUTH")
  EXEC="$EXEC --review-adjudications $STATE_DIR/review-adjudications.jsonl"
  EXEC="$EXEC --journal $ROOT/.choir/journal.jsonl"
  EXEC="$EXEC --request-log $LOG.requests.jsonl --request-log-max-bytes 33554432"
  EXEC="$EXEC --rate-limit-api 120 --rate-limit-git 60"
  EXEC="$EXEC --quota-push-bytes 536870912 --quota-workspaces 8"
  EXEC="$EXEC --api-body-limit 1048576 --batch-limit 256"
  EXEC="$EXEC --ready-min-free-bytes 1073741824 --read-only-browser"
fi
if [ -n "$TLS_CERT" ]; then
  EXEC="$EXEC --bind 0.0.0.0"
  EXEC="$EXEC --tls-cert $TLS_CERT"
  EXEC="$EXEC --tls-key $TLS_KEY"
  BIND_DESC="TLS public bind"
else
  EXEC="$EXEC --bind 127.0.0.1"
  BIND_DESC="loopback bind"
fi
repo_count=0
while IFS= read -r repo; do
  case $repo in ''|\#*) continue ;; esac
  EXEC="$EXEC --create $repo"
  repo_count=$((repo_count + 1))
done < "$REPOS_FILE"
[ "$repo_count" -gt 0 ] || { echo "render_node_service.sh: $REPOS_FILE lists no repos" >&2; exit 1; }

cat <<UNIT
[Unit]
Description=$LABEL — choir node ($BIND_DESC, auth mandatory)
Documentation=file://$ROOT
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
$(if [ -n "$PRIVATE_BETA" ]; then printf 'User=%s\nGroup=%s\n' "$PRIVATE_BETA" "$PRIVATE_BETA"; fi)
ExecStart=$EXEC
Restart=always
RestartSec=2
StandardOutput=append:$LOG
StandardError=append:$LOG
$(if [ -n "$PRIVATE_BETA" ]; then cat <<HARDENING
UMask=0077
NoNewPrivileges=true
PrivateTmp=true
PrivateDevices=true
ProtectSystem=strict
ProtectHome=read-only
ReadWritePaths=$ROOT $(dirname "$LOG")
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true
RestrictSUIDSGID=true
RestrictRealtime=true
LockPersonality=true
TasksMax=128
MemoryMax=1G
LimitNOFILE=65536
HARDENING
fi)

[Install]
WantedBy=$(if [ -n "$PRIVATE_BETA" ]; then echo multi-user.target; else echo default.target; fi)
UNIT
