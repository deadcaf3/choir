#!/bin/sh
# Exercise the authenticated GUI/API/Git surface through its real HTTPS
# hostname. An optional staging canary proves pushes cross the proxy too.
set -eu

usage() {
  echo "usage: smoke_private_beta.sh <https-base> <auth-file> <owner/repo.git>" \
       "[--push-canary] [--denied <owner/repo.git>]" >&2
  exit 2
}

[ "$#" -ge 3 ] || usage
BASE=${1%/}
AUTH=$2
REPO=$3
shift 3
CANARY=
DENIED=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --push-canary) CANARY=$1; shift ;;
    --denied) [ "$#" -ge 2 ] || usage; DENIED=$2; shift 2 ;;
    *) usage ;;
  esac
done
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

# Anonymous first. A smoke test that only ever sends credentials cannot
# tell a node that checks them from one that came up with authentication
# off: every check passes either way. `/` is excluded on purpose - it is
# the public landing page and answers 200 to anybody, which is also why
# fetching it with credentials proves nothing about authentication.
for path in /healthz /readyz /metrics /api/schema /api/view "/$REPO"; do
  code=$(curl --silent --output /dev/null --write-out '%{http_code}' "$BASE$path")
  [ "$code" = 401 ] \
    || { echo "smoke-private-beta: anonymous $path returned $code, expected 401" >&2; exit 1; }
done
code=$(curl --silent --output /dev/null --write-out '%{http_code}' \
  "$BASE/$REPO/info/refs?service=git-upload-pack")
[ "$code" = 401 ] \
  || { echo "smoke-private-beta: anonymous git fetch returned $code, expected 401" >&2; exit 1; }

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

# A repository the credential must not reach, when the operator names
# one. Denied access is a receipt item and the only half of the ACL this
# can see from outside: the allowed half is the clone above.
if [ -n "$DENIED" ]; then
  code=$(curl --config "$WORK/curl.conf" --silent --output /dev/null \
    --write-out '%{http_code}' "$BASE/$DENIED")
  case "$code" in
    403|404) ;;
    *) echo "smoke-private-beta: $DENIED returned $code to a credential that must not reach it" >&2
       exit 1 ;;
  esac
fi

echo "smoke-private-beta: anonymous refusal, authenticated health, readiness, GUI, API limit, and clone passed${DENIED:+; denied ACL access passed}${CANARY:+; push canary passed}"
