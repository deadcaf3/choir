#!/bin/sh
# Fail-closed adapter from one private-beta state directory to a hardened
# system service. It renders only; installation and service restart remain
# explicit operator actions.
set -eu

[ "$#" -eq 6 ] || {
  echo "usage: render_private_beta_service.sh <service-user> <choir-node-bin> <repo-root> <port> <state-dir> <log-file>" >&2
  exit 2
}

SERVICE_USER=$1
BIN=$2
ROOT=$3
PORT=$4
STATE=$5
LOG=$6
HERE=$(cd "$(dirname "$0")" && pwd)

fail() { echo "render-private-beta: $1" >&2; exit 1; }

case "$SERVICE_USER" in ''|*[!a-zA-Z0-9_-]*) fail "invalid service user" ;; esac
[ -x "$BIN" ] || fail "node binary is not executable: $BIN"
[ -d "$STATE" ] || fail "state directory is missing: $STATE"

for file in auth keys reviewers protected-refs newcomer-audit.jsonl newcomer-adjudications.jsonl review-adjudications.jsonl repos.list acl private-beta.manifest; do
  [ -f "$STATE/$file" ] || fail "required beta state is missing: $STATE/$file"
done
# The state copy must be this tree's copy. That the numbers in this
# tree's copy are the numbers render_node_service.sh goes on to emit is
# a separate claim, and one this script cannot make about itself; it is
# asserted in choir-node's `beta_limits` tests (BETA-05).
cmp -s "$HERE/private-beta.manifest" "$STATE/private-beta.manifest" \
  || fail "private-beta.manifest differs from this tree's copy of the limits and feature set"

# Auth and policy are operator material. Group/world-readable files are
# refused before they become command-line inputs to a public service.
for file in auth keys reviewers protected-refs acl; do
  # GNU stat on the node host, BSD stat where this is rendered and
  # tested from. The renderer is pure, so it runs anywhere; refusing to
  # read a mode on the machine the tests run on would make this whole
  # check unexercised until it reached the one host nobody runs tests on.
  mode=$(stat -c '%a' "$STATE/$file" 2>/dev/null || stat -f '%Lp' "$STATE/$file" 2>/dev/null) \
    || fail "cannot read permissions for $STATE/$file"
  case "$mode" in *00) ;; *) fail "$STATE/$file must not be group/world accessible (mode $mode)" ;; esac
done

grep -Eq '^[[:space:]]*[^#[:space:]]' "$STATE/acl" \
  || fail "ACL has no grants; add beta users explicitly"
grep -Eq '^[[:space:]]*[^#[:space:]]' "$STATE/repos.list" \
  || fail "repos.list has no repositories"
sh "$HERE/validate_review_policy.sh" "$STATE/keys" "$STATE/reviewers" "$STATE/protected-refs"
sh "$HERE/validate_beta_acl.sh" "$STATE/acl"

# D36 self-service, governed by the manifest rather than by this script's
# opinion: `accounts=enabled` there turns it on, anything else leaves it
# off. The manifest is already compared byte-for-byte against this tree's
# copy above, so the feature set a beta node runs cannot drift from the
# feature set the beta documents.
BETA_ACCOUNTS=""
if grep -qx 'accounts=enabled' "$STATE/private-beta.manifest"; then
  BETA_ACCOUNTS="$STATE/accounts.jsonl"
fi

# D71, read from the same file for the same reason. WebAuthn needs the
# accounts store, so a manifest asking for it without accounts asks for a
# node that cannot exist; refused here rather than rendered into a unit
# whose behaviour contradicts the file it came from.
BETA_WEBAUTHN=""
if grep -qx 'passkeys=enabled' "$STATE/private-beta.manifest"; then
  [ -n "$BETA_ACCOUNTS" ] \
    || fail "manifest asks for passkeys without accounts; they are enrolled on issued accounts"
  BETA_WEBAUTHN=on
fi

sh "$HERE/render_node_service.sh" choir-node "$BIN" "$ROOT" "$PORT" \
  "$STATE/auth" "$STATE/keys" "$STATE/reviewers" "$LOG" "$STATE/repos.list" \
  "$STATE/newcomer-audit.jsonl" "$STATE/newcomer-adjudications.jsonl" \
  "$STATE/protected-refs" require-scope '' '' "$STATE/acl" \
  "$BETA_ACCOUNTS" behind-tls-proxy "$BETA_WEBAUTHN" "$SERVICE_USER"
