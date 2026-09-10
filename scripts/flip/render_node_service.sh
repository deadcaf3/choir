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

if [ "$#" -lt 11 ] || [ "$#" -gt 22 ]; then
  echo "usage: render_node_service.sh <label> <bin> <root> <port> <auth> <keys> <reviewers> <log> <repos-file> <newcomer-audit> <newcomer-adjudications> [protected-refs] [require-scope] [tls-cert] [tls-key] [acl] [accounts] [behind-tls-proxy] [webauthn] [site-repo] [private-beta-service-user] [downloads-dir]" >&2
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
# Same contract as the plist renderer: any non-empty 19th argument
# turns on WebAuthn (D71) -- enrolment, the ceremony page, and the
# browser write path. Separate from the accounts file because all
# three read that one store, so a single flag would mean a node
# offering self-service credentials always offered browser signing
# with them.
WEBAUTHN=${19:-}
# Same contract as the plist renderer: a non-empty 20th argument is the
# one repository this node presents as its site (D78), so `/` is that
# repository rather than an index. It sits at 20 on both renderers, ahead
# of the Linux-only service user, for the reason the accounts file gives
# at 17: every argument the two platforms share keeps the same position,
# and a flag added to one and forgotten on the other stays a diff between
# two files fed the same inputs.
#
# Presentation, never a grant. It decides what `/` renders; who may read
# it is the ACL's answer, and a repository this hides is still clonable
# by whoever could clone it before.
SITE_REPO=${20:-}
PRIVATE_BETA=${21:-}
# The release shelf, at 22 on both platforms so the shared arguments
# keep the same position (slot 21 is the Linux-only service user).
DOWNLOADS=${22:-}
if [ -n "$TLS_CERT$TLS_KEY" ] && { [ -z "$TLS_CERT" ] || [ -z "$TLS_KEY" ]; }; then
  echo "render_node_service.sh: tls-cert and tls-key must be given together" >&2
  exit 2
fi
# `serve_single_repository` refuses a name this node could not hold, and
# that refusal is a node that will not start. Checking the shape here
# turns it into a renderer that will not render, on the same reasoning
# as the TLS pair above.
if [ -n "$SITE_REPO" ]; then
  case "$SITE_REPO" in
    */*/*|/*|*/) echo "render_node_service.sh: site-repo must be owner/name" >&2; exit 2 ;;
    */*) : ;;
    *) echo "render_node_service.sh: site-repo must be owner/name" >&2; exit 2 ;;
  esac
fi
# The shelf is a directory the node serves unauthenticated (D79), so the
# renderer refuses a value that is not an absolute path with no
# whitespace in it: this lands unquoted in an ExecStart line, and a value
# that word-splits there is a daemon launched with arguments nobody
# wrote.
if [ -n "$DOWNLOADS" ]; then
  case "$DOWNLOADS" in
    /*) : ;;
    *) echo "render_node_service.sh: downloads-dir must be an absolute path with no spaces" >&2; exit 2 ;;
  esac
  # `[[:space:]]`, not `[\ \t]`: inside a shell bracket expression `\t`
  # is the letter t, so that spelling refused every path with a t in it.
  case "$DOWNLOADS" in
    *[[:space:]]*) echo "render_node_service.sh: downloads-dir must be an absolute path with no spaces" >&2; exit 2 ;;
  esac
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
if [ -n "$SITE_REPO" ]; then
  EXEC="$EXEC --site-repo $SITE_REPO"
fi
if [ -n "$DOWNLOADS" ]; then
  EXEC="$EXEC --downloads-dir $DOWNLOADS"
fi
if [ -n "$BEHIND_TLS_PROXY" ]; then
  EXEC="$EXEC --behind-tls-proxy"
fi
if [ -n "$WEBAUTHN" ]; then
  EXEC="$EXEC --passkeys"
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
