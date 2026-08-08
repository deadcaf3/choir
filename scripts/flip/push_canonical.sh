#!/bin/zsh
# D20 flip prep, checklist items 3 and 5: push this repo to the choir
# node as the canonical remote.
#
# After the flip the choir node is canonical and Forgejo is a follower
# (D21 single-canonical invariant, one direction only — never dual-write).
# Every ref lands as a signed, sequenced op through the smart-HTTP path.
#
# Item 3: a branch push does NOT carry tags, so tags go in a second push.
# Both are needed or the mirror silently loses release history.
#
# Pushes the *current branch* by name, not `--all`. `--all` swept up every
# local branch including agent worktree branches, and a worktree that
# rebases makes its branch non-fast-forward — which failed the whole push
# and, because sync is `set -e`, stopped the mirror behind the node. The
# canonical node carries canonical history; a transient worktree branch is
# not that. To put another branch on the node, push it by name.
set -euo pipefail

PORT=${1:-8417}
REPO=${2:-choir/choir.git}
AUTH=${3:-$HOME/.choir/auth}
REPO_DIR="$(cd "$(dirname "$0")/../.." && pwd)"

# Credentials come from the daemon's own auth file (0600, never in-repo).
# The URL carries them, so it is built here and never written to .git/config.
USER_NAME=$(head -1 "$AUTH" | cut -d: -f1)
TOKEN=$(head -1 "$AUTH" | cut -d: -f2-)
URL="http://$USER_NAME:$TOKEN@127.0.0.1:$PORT/$REPO"

# The bare repo must exist on the node; creating it installs the
# pre-receive hook that turns pushes into signed ops.
if ! curl -sf -u "$USER_NAME:$TOKEN" "http://127.0.0.1:$PORT/$REPO/info/refs?service=git-upload-pack" > /dev/null; then
  echo "no such repo on the node: $REPO" >&2
  echo "create it with: choir-node ... --create $REPO   (or restart the agent with that flag)" >&2
  exit 1
fi

cd "$REPO_DIR"
BRANCH=$(git rev-parse --abbrev-ref HEAD)
if [ "$BRANCH" = "HEAD" ]; then
  echo "detached HEAD: check out a branch before syncing" >&2
  exit 1
fi
git push "$URL" "$BRANCH"
git push --tags "$URL"
echo "canonical: $(git log --oneline -1)"
echo "verify:    curl -s -u $USER_NAME:<token> http://127.0.0.1:$PORT/api/view"
