#!/bin/sh
# Exercise the authenticated GUI/API/Git surface through its real HTTPS
# hostname. An optional staging canary proves pushes cross the proxy too.
set -eu

[ "$#" -ge 3 ] && [ "$#" -le 4 ] || {
  echo "usage: smoke_private_beta.sh <https-base> <auth-file> <owner/repo.git> [--push-canary]" >&2
  exit 2
}

BASE=${1%/}
AUTH=$2
REPO=$3
CANARY=${4:-}
case "$BASE" in https://*) ;; *) echo "smoke-private-beta: HTTPS is required" >&2; exit 2 ;; esac
[ -f "$AUTH" ] || { echo "smoke-private-beta: auth file is missing" >&2; exit 1; }
CREDS=$(sed -n 1p "$AUTH")
case "$CREDS" in *:*) ;; *) echo "smoke-private-beta: auth file must start with user:token" >&2; exit 1 ;; esac
USER=${CREDS%%:*}
TOKEN=${CREDS#*:}
HOST=$(printf '%s' "$BASE" | sed 's#^https://##; s#/.*##')
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT INT TERM
umask 077
printf 'user = "%s"\n' "$CREDS" > "$WORK/curl.conf"
printf 'protocol=https\nhost=%s\nusername=%s\npassword=%s\n\n' "$HOST" "$USER" "$TOKEN" \
  | git -c credential.helper="store --file=$WORK/git-credentials" credential approve

for path in /healthz /readyz / /api/schema; do
  curl --config "$WORK/curl.conf" --fail --silent --show-error "$BASE$path" >/dev/null
done

# One byte over the shipped API ceiling must be refused before parsing.
code=$(head -c 1048577 /dev/zero \
  | curl --config "$WORK/curl.conf" --silent --output /dev/null --write-out '%{http_code}' \
      --request POST --data-binary @- "$BASE/api/submit")
[ "$code" = 413 ] \
  || { echo "smoke-private-beta: oversized API body returned $code, expected 413" >&2; exit 1; }

git -c credential.helper="store --file=$WORK/git-credentials" clone --quiet "$BASE/$REPO" "$WORK/clone"

if [ "$CANARY" = "--push-canary" ]; then
  git -C "$WORK/clone" rev-parse HEAD >/dev/null 2>&1 \
    || { echo "smoke-private-beta: cannot push a canary from an empty repository" >&2; exit 1; }
  REF="refs/heads/staging-smoke-$(date +%s)"
  git -C "$WORK/clone" -c credential.helper="store --file=$WORK/git-credentials" push --quiet "$BASE/$REPO" "HEAD:$REF"
  git -C "$WORK/clone" -c credential.helper="store --file=$WORK/git-credentials" push --quiet "$BASE/$REPO" ":$REF"
fi

echo "smoke-private-beta: authenticated health, readiness, GUI, API limit, and clone passed${CANARY:+; push canary passed}"
