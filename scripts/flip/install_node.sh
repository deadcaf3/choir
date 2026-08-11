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
HERE="$(cd "$(dirname "$0")" && pwd)"
STATE=$HOME/.choir
ROOT="$STATE/repos"
LABEL=com.choir.node
PLIST=$HOME/Library/LaunchAgents/$LABEL.plist
REPO_DIR="$(cd "$HERE/../.." && pwd)"
BIN="$REPO_DIR/target/release/choir-node"
POLICY_MARKER="$STATE/review-gates.enabled"
PROTECTED_REFS="$STATE/protected-refs"
NEWCOMER_AUDIT="$STATE/newcomer-audit.jsonl"
NEWCOMER_ADJUDICATIONS="$STATE/newcomer-adjudications.jsonl"

mkdir -p "$STATE" "$ROOT" "$HOME/Library/LaunchAgents"
chmod 700 "$STATE"

# 1. Binaries. Built release so the daemon is not a debug build.
#
# CHOIR_GIT_HEAD stamps the binary with the commit it came from, which is
# what lets `choirctl status` tell "the rebuild reached the running
# process" from "it did not" — the two are otherwise identical from
# outside. Passed as an env var rather than left to the build script's own
# git call because cargo reruns a build script when a declared env var
# changes, and cannot rerun it on every source edit.
CHOIR_GIT_HEAD="$(git -C "$REPO_DIR" rev-parse HEAD 2>/dev/null || true)" \
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

# 5. Sparse D24 T4 evidence and separate operator adjudications. The node
#    appends only activation, first-attempt, first-acceptance, and appeal
#    rows; the operator owns the adjudication file.
touch "$NEWCOMER_AUDIT" "$NEWCOMER_ADJUDICATIONS"
chmod 600 "$NEWCOMER_AUDIT" "$NEWCOMER_ADJUDICATIONS"

# 6. Optional fail-closed review policy. The marker is explicit state:
#    once present, a reinstall must preserve the gate or refuse to run.
#    Every pool member needs a bound key, and two distinct prefixes keep
#    an accidental one-name or one-operator pool from looking complete.
if [[ -f "$POLICY_MARKER" ]]; then
  sh "$HERE/validate_review_policy.sh" "$STATE/keys" "$STATE/reviewers" "$PROTECTED_REFS"
  sh "$HERE/render_node_plist.sh" "$LABEL" "$BIN" "$ROOT" "$PORT" \
    "$STATE/auth" "$STATE/keys" "$STATE/reviewers" "$STATE/node.log" "$REPO" \
    "$NEWCOMER_AUDIT" "$NEWCOMER_ADJUDICATIONS" "$PROTECTED_REFS" > "$PLIST"
  echo "review gate enabled ($PROTECTED_REFS)"
else
  sh "$HERE/render_node_plist.sh" "$LABEL" "$BIN" "$ROOT" "$PORT" \
    "$STATE/auth" "$STATE/keys" "$STATE/reviewers" "$STATE/node.log" "$REPO" \
    "$NEWCOMER_AUDIT" "$NEWCOMER_ADJUDICATIONS" > "$PLIST"
fi

# 7. launchd agent. The renderer receives absolute paths because launchd
#    has no shell, no PATH expansion, and no $HOME in program arguments.

launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
launchctl bootstrap "gui/$(id -u)" "$PLIST"
echo "loaded $LABEL on 127.0.0.1:$PORT (logs: $STATE/node.log)"
echo "check: curl -s -u choir:\$(cut -d: -f2 $STATE/auth) http://127.0.0.1:$PORT/api/view"
