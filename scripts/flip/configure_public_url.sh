#!/bin/sh
# Record the certificate-valid API base used by operator tools.
#
# This is separate from certificate issuance so an existing TLS node can be
# migrated without renewing its certificate. The hostname stays in untracked
# operator state, never in the repository.
set -eu

DOMAIN=${1:?usage: configure_public_url.sh <domain> [port]}
PORT=${2:-8417}
PUBLIC_URL_FILE=$HOME/.choir-public-url

case "$DOMAIN" in
  *.*) ;;
  *) echo "configure_public_url.sh: domain must be a DNS name" >&2; exit 2;;
esac
case "$DOMAIN" in
  .*|*.|*..*|*[!A-Za-z0-9.-]*)
    echo "configure_public_url.sh: domain contains an invalid DNS label" >&2
    exit 2
    ;;
esac
old_ifs=$IFS
IFS=.
set -- $DOMAIN
IFS=$old_ifs
for label in "$@"; do
  case "$label" in
    ''|-*|*-)
      echo "configure_public_url.sh: domain contains an invalid DNS label" >&2
      exit 2
      ;;
  esac
done
case "$PORT" in
  ''|*[!0-9]*|??????*)
    echo "configure_public_url.sh: port must be an integer from 1 to 65535" >&2
    exit 2
    ;;
esac
if [ "$PORT" -lt 1 ] || [ "$PORT" -gt 65535 ]; then
  echo "configure_public_url.sh: port must be an integer from 1 to 65535" >&2
  exit 2
fi

# 443 is written without the port. The whole reason to move termination
# to a proxy on the default port is that the base becomes something a
# person can type and an agent can be handed, and `https://host:443`
# would keep the port visible in every clone URL, every .choir/config
# and every doc example for no gain. Both spellings reach the same
# socket; only one of them reads like a product.
if [ "$PORT" = 443 ]; then
  wanted=https://$DOMAIN
else
  wanted=https://$DOMAIN:$PORT
fi
if [ -f "$PUBLIC_URL_FILE" ] \
  && [ "$(sed -n '1p' "$PUBLIC_URL_FILE")" = "$wanted" ] \
  && [ "$(wc -l < "$PUBLIC_URL_FILE" | tr -d ' ')" = 1 ]; then
  chmod 600 "$PUBLIC_URL_FILE"
  echo "public API route already configured"
  exit 0
fi

umask 077
new=$(mktemp "$PUBLIC_URL_FILE.new.XXXXXX")
cleanup() {
  trap - EXIT HUP INT TERM
  if [ -f "$new" ]; then
    find "$new" -delete
  fi
}
trap cleanup EXIT HUP INT TERM
printf '%s\n' "$wanted" > "$new"
chmod 600 "$new"
mv "$new" "$PUBLIC_URL_FILE"
trap - EXIT HUP INT TERM
echo "public API route configured"
