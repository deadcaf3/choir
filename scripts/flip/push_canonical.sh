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
# Public TLS, when enabled, moves every HTTP leg to the public name;
# the untracked ~/.choir-public-url holds the base and its absence
# means the pre-flip world: plaintext over the loopback tunnel.
if [ -f "$HOME/.choir-public-url" ]; then
  BASE=$(cat "$HOME/.choir-public-url")
else
  BASE="http://127.0.0.1:$PORT"
fi
URL=$(printf '%s' "$BASE/$REPO" | sed "s|://|://$USER_NAME:$TOKEN@|")

# The bare repo must exist on the node; creating it installs the
# pre-receive hook that turns pushes into signed ops. Read the status
# rather than -f, because -f collapses every failure into one exit code
# and this probe has two very different ones. A refused credential
# reported as a missing repository sends the operator to create a
# repository that is already there, and nothing contradicts them: the
# repo is present, the unit already names it, and the only wrong thing
# is the credential.
CODE=$(curl -s -o /dev/null -w '%{http_code}' -u "$USER_NAME:$TOKEN" \
  "$BASE/$REPO/info/refs?service=git-upload-pack")
case "$CODE" in
  200) ;;
  401|403)
    echo "the node refused this credential for $REPO (HTTP $CODE)" >&2
    echo "the repository may well exist. Check that the credential file's" >&2
    echo "first line matches the node's own, and that the ACL grants" >&2
    echo "$USER_NAME access to $REPO" >&2
    exit 1 ;;
  404)
    echo "no such repo on the node: $REPO" >&2
    echo "create it with: choir-node ... --create $REPO   (or restart the agent with that flag)" >&2
    exit 1 ;;
  000)
    echo "no answer from $BASE: the node is down, or the tunnel is not up" >&2
    exit 1 ;;
  *)
    echo "unexpected $CODE from $BASE/$REPO while checking it exists" >&2
    exit 1 ;;
esac

cd "$REPO_DIR"
BRANCH=$(git rev-parse --abbrev-ref HEAD)
if [ "$BRANCH" = "HEAD" ]; then
  echo "detached HEAD: check out a branch before syncing" >&2
  exit 1
fi
git push "$URL" "$BRANCH"
git push --tags "$URL"
echo "canonical: $(git log --oneline -1)"
echo "verify:    curl -s -u $USER_NAME:<token> $BASE/api/view"
