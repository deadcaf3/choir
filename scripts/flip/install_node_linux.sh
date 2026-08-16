#!/bin/sh
# D20: install the dogfood choir-node as a supervised systemd user service.
# The Linux sibling of install_node.sh, for a node that lives on a server
# rather than on the operator's laptop.
#
# Deliberately does NOT build. The e2-micro class of host this targets has
# under a gigabyte of RAM; binaries are built elsewhere and copied in, and
# `--bin-dir` is where they landed. That is the one structural difference
# from the macOS installer, which builds in place.
#
# Everything else matches install_node.sh: loopback bind (invariant 9 — no
# TLS, so no non-loopback bind), auth mandatory, keys file for the platform
# API, reviewer pool for D24 layer-5 assignment, restart supervision, logs
# under ~/.choir.
#
# Idempotent: safe to re-run after copying new binaries (it reloads the
# unit). Creates no secrets in the repo; everything lives under ~/.choir
# at 0600. Existing state is never overwritten, so running this after
# migrating a ~/.choir from another host preserves that host's identity.
set -eu

PORT=${1:-8417}
REPO=${2:-choir/choir.git}
BIN_DIR=${3:-$HOME/bin}
ARTIFACTS=${4:-}
REPOS_LIST=$HOME/.choir/repos.list

HERE="$(cd "$(dirname "$0")" && pwd)"
STATE=$HOME/.choir
ROOT="$STATE/repos"
LABEL=choir-node
UNIT_DIR=$HOME/.config/systemd/user
UNIT="$UNIT_DIR/$LABEL.service"
BIN="$BIN_DIR/choir-node"
CHOIR="$BIN_DIR/choir"
POLICY_MARKER="$STATE/review-gates.enabled"
SCOPE_MARKER="$STATE/scope-required.enabled"
TLS_MARKER="$STATE/tls.enabled"
PUBLIC_URL_FILE=$HOME/.choir-public-url
PROTECTED_REFS="$STATE/protected-refs"
NEWCOMER_AUDIT="$STATE/newcomer-audit.jsonl"
NEWCOMER_ADJUDICATIONS="$STATE/newcomer-adjudications.jsonl"

mkdir -p "$STATE" "$ROOT" "$UNIT_DIR"
chmod 700 "$STATE"

# 0. Repos list, same contract as the macOS installer: seeded once, and
#    a later run appends only a repo named explicitly on the command
#    line, so a bare re-run never resurrects a deleted line.
if [ ! -f "$REPOS_LIST" ]; then
  {
    echo "# repos served by the node, one owner/name.git per line"
    echo "$REPO"
  } > "$REPOS_LIST"
  chmod 600 "$REPOS_LIST"
  echo "seeded $REPOS_LIST with $REPO"
elif [ "$#" -ge 2 ] && [ -n "$2" ] && ! grep -qxF "$2" "$REPOS_LIST"; then
  echo "$2" >> "$REPOS_LIST"
  echo "appended $2 to $REPOS_LIST"
fi

# 1. Binaries must already be here — or be named as a fourth argument,
#    e.g. `install_node_linux.sh 8417 '' '' ~/choir-build/target/release`.
#    The copy goes through `.new` + `mv` because `cp` straight over the
#    running daemon's binary fails with ETXTBSY on Linux; the rename
#    swaps the path atomically and the running process keeps its old
#    inode until the restart below.
if [ -n "$ARTIFACTS" ]; then
  mkdir -p "$BIN_DIR"
  for f in choir-node choir; do
    [ -f "$ARTIFACTS/$f" ] || { echo "no $f in $ARTIFACTS" >&2; exit 1; }
    cp "$ARTIFACTS/$f" "$BIN_DIR/$f.new"
    mv "$BIN_DIR/$f.new" "$BIN_DIR/$f"
  done
  echo "copied choir-node + choir in from $ARTIFACTS"
fi

#    Failing loudly beats starting a unit that will crash-loop under
#    Restart=always.
for f in "$BIN" "$CHOIR"; do
  [ -x "$f" ] || { echo "missing executable: $f (build elsewhere and copy in)" >&2; exit 1; }
done

# 2. Auth token. Mandatory on the real node.
if [ ! -f "$STATE/auth" ]; then
  printf 'choir:%s\n' "$(openssl rand -hex 32)" > "$STATE/auth"
  chmod 600 "$STATE/auth"
  echo "minted $STATE/auth"
fi

# 3. Trusted keys for the platform API. Hot-reloaded, so later additions
#    need no restart.
if [ ! -f "$STATE/keys" ]; then
  : > "$STATE/keys"
  chmod 600 "$STATE/keys"
  "$CHOIR" key "$STATE/agent.key" >> "$STATE/keys"
  echo "minted $STATE/agent.key and registered it in $STATE/keys"
fi

# 4. Reviewer pool (D24 layer 5). Re-read per draw.
if [ ! -f "$STATE/reviewers" ]; then
  echo "# eligible reviewer names, one per line (re-read per draw)" > "$STATE/reviewers"
  chmod 600 "$STATE/reviewers"
fi

# 5. Sparse D24 T4 evidence and separate operator adjudications.
touch "$NEWCOMER_AUDIT" "$NEWCOMER_ADJUDICATIONS"
chmod 600 "$NEWCOMER_AUDIT" "$NEWCOMER_ADJUDICATIONS"

# 6. Optional fail-closed review policy, same marker and same validation
#    as the macOS path: once present, a reinstall preserves the gate or
#    refuses to run. The scope marker is the same idea for D26 replay
#    containment: once present, every reinstall keeps --require-scope.
REQUIRE_SCOPE=""
if [ -f "$SCOPE_MARKER" ]; then
  REQUIRE_SCOPE="require-scope"
fi
# D29 per-repository authorization, same contract as the macOS installer:
# the file's existence is the marker, because an empty ACL and no ACL mean
# opposite things and a separate enable-flag could disagree with the file
# it guards.
ACL=""
if [ -f "$STATE/acl" ]; then
  ACL="$STATE/acl"
fi
# TLS marker: two lines, cert path then key path, both readable by this
# user. Once present, every reinstall keeps the public TLS bind — the
# same one-way marker discipline as the review and scope gates, and the
# same fail-closed shape: a marker naming unreadable files refuses to
# render rather than installing a unit that crash-loops on startup.
TLS_CERT=""
TLS_KEY=""
PUBLIC_URL=""
if [ -f "$TLS_MARKER" ]; then
  TLS_CERT=$(sed -n 1p "$TLS_MARKER")
  TLS_KEY=$(sed -n 2p "$TLS_MARKER")
  [ -n "$TLS_CERT" ] && [ -n "$TLS_KEY" ] \
    || { echo "$TLS_MARKER must hold two lines: cert path, key path" >&2; exit 1; }
  [ -r "$TLS_CERT" ] && [ -r "$TLS_KEY" ] \
    || { echo "TLS enabled but the cert/key files named by $TLS_MARKER are not readable" >&2; exit 1; }
  [ -r "$PUBLIC_URL_FILE" ] \
    || { echo "TLS enabled but no verified API route is configured; run configure_public_url.sh <domain> $PORT" >&2; exit 1; }
  PUBLIC_URL=$(sed -n 1p "$PUBLIC_URL_FILE")
  [ "$(wc -l < "$PUBLIC_URL_FILE" | tr -d ' ')" = 1 ] \
    || { echo "$PUBLIC_URL_FILE must hold exactly one URL" >&2; exit 1; }
  case "$PUBLIC_URL" in
    https://*:"$PORT") ;;
    *) echo "$PUBLIC_URL_FILE must hold https://<domain>:$PORT" >&2; exit 1;;
  esac
fi
if [ -f "$POLICY_MARKER" ]; then
  sh "$HERE/validate_review_policy.sh" "$STATE/keys" "$STATE/reviewers" "$PROTECTED_REFS"
  sh "$HERE/render_node_service.sh" "$LABEL" "$BIN" "$ROOT" "$PORT" \
    "$STATE/auth" "$STATE/keys" "$STATE/reviewers" "$STATE/node.log" "$REPOS_LIST" \
    "$NEWCOMER_AUDIT" "$NEWCOMER_ADJUDICATIONS" "$PROTECTED_REFS" "$REQUIRE_SCOPE" \
    "$TLS_CERT" "$TLS_KEY" "$ACL" > "$UNIT"
  echo "review gate enabled ($PROTECTED_REFS)"
else
  sh "$HERE/render_node_service.sh" "$LABEL" "$BIN" "$ROOT" "$PORT" \
    "$STATE/auth" "$STATE/keys" "$STATE/reviewers" "$STATE/node.log" "$REPOS_LIST" \
    "$NEWCOMER_AUDIT" "$NEWCOMER_ADJUDICATIONS" "" "$REQUIRE_SCOPE" \
    "$TLS_CERT" "$TLS_KEY" "$ACL" > "$UNIT"
fi
if [ -n "$TLS_CERT" ]; then
  echo "TLS public bind enabled ($TLS_MARKER): serving 0.0.0.0:$PORT with $TLS_CERT"
fi
if [ -n "$ACL" ]; then
  echo "per-repository authorization enabled ($ACL): ungranted access is refused"
else
  echo "no $STATE/acl: every authenticated credential reaches every repository"
fi
if [ -n "$REQUIRE_SCOPE" ]; then
  echo "scope gate enabled ($SCOPE_MARKER): ops must name this node's log and a recent head"
fi

# 7. Lingering is what makes a user unit a daemon: without it systemd stops
#    the user manager at logout and takes the node with it. This is the one
#    step that needs privilege, and it is why the check is explicit rather
#    than assumed.
if ! loginctl show-user "$(id -un)" 2>/dev/null | grep -q '^Linger=yes'; then
  if sudo -n loginctl enable-linger "$(id -un)" 2>/dev/null; then
    echo "enabled linger for $(id -un)"
  else
    echo "WARNING: linger is off and could not be enabled without a password." >&2
    echo "         The node will stop at logout. Run: sudo loginctl enable-linger $(id -un)" >&2
  fi
fi

systemctl --user daemon-reload
systemctl --user enable "$LABEL.service" >/dev/null 2>&1 || true
systemctl --user restart "$LABEL.service"
if [ -n "$PUBLIC_URL" ]; then
  echo "loaded $LABEL on public TLS port $PORT (logs: $STATE/node.log)"
  echo "check: $CHOIR --auth-file $STATE/auth --auth-user <user> view \"\$(cat ~/.choir-public-url)\""
else
  echo "loaded $LABEL on 127.0.0.1:$PORT (logs: $STATE/node.log)"
  echo "check: $CHOIR --auth-file $STATE/auth --auth-user <user> view http://127.0.0.1:$PORT"
fi
