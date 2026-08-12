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
# Repos to serve live in ~/.choir/repos.list, one `owner/name.git` per
# line, rendered into one `--create` each. `--create` is idempotent (an
# existing repo is skipped), so the list stays in the plist permanently:
# creation is also what installs the pre-receive hook that turns pushes
# into signed ops. $2 seeds the list on first install and is appended on
# a later one if missing, so adding a repo is either an appended line or
# a re-run with the new name — both end in a reinstall.
REPO=${2:-choir/choir.git}
REPOS_LIST=$HOME/.choir/repos.list
HERE="$(cd "$(dirname "$0")" && pwd)"
STATE=$HOME/.choir
ROOT="$STATE/repos"
LABEL=com.choir.node
PLIST=$HOME/Library/LaunchAgents/$LABEL.plist
REPO_DIR="$(cd "$HERE/../.." && pwd)"

# Ask cargo where it puts things rather than assuming `$REPO_DIR/target`.
# Cargo resolves `target-dir` from the *working directory*, not from
# --manifest-path, and the worktrees under .claude/worktrees redirect it
# to a shared-target. Run from there, the build below succeeds and lands
# somewhere else entirely while BIN still points here -- and step 7
# boots the running node out before anything notices the binary is
# missing, leaving a plist aimed at a path that does not exist and a
# canonical node that KeepAlive cannot bring back.
TARGET_DIR="$(cargo metadata --format-version 1 --no-deps \
  --manifest-path "$REPO_DIR/Cargo.toml" 2>/dev/null \
  | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
TARGET_DIR="${TARGET_DIR:-$REPO_DIR/target}"
BIN="$TARGET_DIR/release/choir-node"
POLICY_MARKER="$STATE/review-gates.enabled"
PROTECTED_REFS="$STATE/protected-refs"
NEWCOMER_AUDIT="$STATE/newcomer-audit.jsonl"
NEWCOMER_ADJUDICATIONS="$STATE/newcomer-adjudications.jsonl"

mkdir -p "$STATE" "$ROOT" "$HOME/Library/LaunchAgents"
chmod 700 "$STATE"

# 0. Repos list. Seeded once; a later run only appends, and only a repo
#    named explicitly on the command line, so a bare re-run never
#    resurrects a line the operator deleted on purpose.
if [[ ! -f $REPOS_LIST ]]; then
  {
    echo "# repos served by the node, one owner/name.git per line"
    echo "$REPO"
  } > "$REPOS_LIST"
  chmod 600 "$REPOS_LIST"
  echo "seeded $REPOS_LIST with $REPO"
elif [[ $# -ge 2 && -n $2 ]] && ! grep -qxF "$2" "$REPOS_LIST"; then
  echo "$2" >> "$REPOS_LIST"
  echo "appended $2 to $REPOS_LIST"
fi

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

# Refuse before step 7 rather than after. `launchctl bootout` stops the
# node that is currently serving; everything after this line assumes a
# binary exists to replace it with.
if [[ ! -x "$BIN" || ! -x "$TARGET_DIR/release/choir" ]]; then
  echo "install: no built binaries at $TARGET_DIR/release" >&2
  echo "  cargo resolves target-dir from the working directory; run this" >&2
  echo "  from the main checkout, not from a worktree. Nothing was changed." >&2
  exit 1
fi

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
  "$TARGET_DIR/release/choir" key "$STATE/agent.key" >> "$STATE/keys"
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
    "$STATE/auth" "$STATE/keys" "$STATE/reviewers" "$STATE/node.log" "$REPOS_LIST" \
    "$NEWCOMER_AUDIT" "$NEWCOMER_ADJUDICATIONS" "$PROTECTED_REFS" > "$PLIST"
  echo "review gate enabled ($PROTECTED_REFS)"
else
  sh "$HERE/render_node_plist.sh" "$LABEL" "$BIN" "$ROOT" "$PORT" \
    "$STATE/auth" "$STATE/keys" "$STATE/reviewers" "$STATE/node.log" "$REPOS_LIST" \
    "$NEWCOMER_AUDIT" "$NEWCOMER_ADJUDICATIONS" > "$PLIST"
fi

# 7. launchd agent. The renderer receives absolute paths because launchd
#    has no shell, no PATH expansion, and no $HOME in program arguments.

launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
launchctl bootstrap "gui/$(id -u)" "$PLIST"
echo "loaded $LABEL on 127.0.0.1:$PORT (logs: $STATE/node.log)"
echo "check: curl -s -u choir:\$(cut -d: -f2 $STATE/auth) http://127.0.0.1:$PORT/api/view"
