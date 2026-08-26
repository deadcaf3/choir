#!/bin/sh
# One-time TLS enablement for a Linux node host: issue a Let's Encrypt
# certificate for the node's public name, project the pair to where the
# node's user can read it, install a renewal hook that keeps doing so,
# and write the tls.enabled marker the installer reads. Run ON the node
# host as root, naming the unprivileged user the node runs as.
#
# Usage: sudo sh setup_tls.sh <domain> <port> <node-user>
#
# Root, not the node's user, because everything privileged here is
# genuinely root's: certbot writes /etc/letsencrypt, and the deploy hook
# lives under it. The node account is deliberately unprivileged
# (`useradd -m choir`, see RUNBOOK.md) and giving it sudo to run this
# would undo that for the life of the machine. Any sudoers rule narrow
# enough to look safe is not: NOPASSWD on tee or chmod is root by
# another spelling. So the privilege stays with the operator's own
# account for the length of one command, and the node user is named as
# an argument rather than inferred from who is running.
#
# Why the copy instead of pointing the unit at /etc/letsencrypt: the
# live cert dir is root-owned by design, the node deliberately runs
# unprivileged, and a unit that reads root-owned paths works exactly
# until the first renewal rotates the files. The deploy hook is the
# renewal-safe seam: certbot runs it on every successful renewal, so
# the node's copy and the node's process are refreshed together, and a
# renewal that stops working is a loud certbot failure rather than a
# quietly expiring cert.
#
# The account is registered without an email on purpose (standing
# privacy rule: no personal identifiers in infrastructure). The cost is
# no expiry-warning mail, which the deploy hook's automation is the
# actual answer to; `certbot renew --dry-run` is the manual check.
#
# Two issuance methods, selected by a file rather than a flag, same
# marker discipline as tls.enabled: if $HOME/.choir/cloudflare.ini
# exists it is DNS-01 through the Cloudflare API, otherwise HTTP-01 via
# --standalone on port 80.
#
# Prefer DNS-01 behind a proxying CDN. HTTP-01 needs the challenge to
# reach this host on port 80, which means the DNS record cannot be
# proxied during issuance or any renewal, and that publishes the origin
# IP. Passive-DNS services archive it permanently, so re-enabling the
# proxy afterwards does not take it back: the origin stays reachable
# directly, around whatever the CDN is absorbing. DNS-01 proves control
# of the record instead, needs no inbound port, and never exposes the
# address.
#
# The credentials file is `dns_cloudflare_api_token = <token>`, 0600,
# with a token scoped to Zone:DNS:Edit on this zone alone. It is
# untracked, like every other secret here.
#
# With --standalone the host's firewall must allow inbound 80
# permanently, because renewals rebind it every ~60 days. Nothing else
# ever serves on 80 here. DNS-01 needs no such hole.
set -eu

DOMAIN=${1:?usage: setup_tls.sh <domain> [port]}
PORT=${2:-8417}
# Required, never defaulted. Falling back to logname or SUDO_USER would
# name the operator's own account, which is exactly the account the node
# does not run as, and the failure is silent: markers and a cert pair
# land in the wrong home and the node keeps serving plaintext.
NODE_USER=${3:?usage: sudo sh setup_tls.sh <domain> <port> <node-user>}
[ "$(id -u)" = 0 ] || { echo "run this as root: sudo sh setup_tls.sh $DOMAIN $PORT $NODE_USER" >&2; exit 2; }
id "$NODE_USER" >/dev/null 2>&1 || { echo "no such user: $NODE_USER" >&2; exit 1; }
NODE_HOME=$(getent passwd "$NODE_USER" | cut -d: -f6)
STATE=$NODE_HOME/.choir
TLS_DIR=$STATE/tls
MARKER=$STATE/tls.enabled
HOOK=/etc/letsencrypt/renewal-hooks/deploy/choir-tls
NODE_UID=$(id -u "$NODE_USER")
HERE="$(cd "$(dirname "$0")" && pwd)"

command -v certbot >/dev/null 2>&1 \
  || { echo "certbot is not installed; run: sudo apt-get install -y certbot" >&2; exit 1; }

CF_INI=$STATE/cloudflare.ini

# 1. Issue. Idempotent: certbot keeps the existing lineage if one exists.
#    The challenge method is whichever the credentials file selects, and
#    the choice is recorded in the renewal config, so `certbot renew`
#    later reuses it without this script being present.
if [ -f "$CF_INI" ]; then
  command -v certbot-dns-cloudflare >/dev/null 2>&1 \
    || python3 -c 'import certbot_dns_cloudflare' 2>/dev/null \
    || { echo "the cloudflare dns plugin is missing; run: sudo apt-get install -y python3-certbot-dns-cloudflare" >&2; exit 1; }
  [ "$(stat -c '%a' "$CF_INI")" = "600" ] \
    || { echo "$CF_INI must be 0600; it holds an API token" >&2; exit 1; }
  certbot certonly --dns-cloudflare \
    --dns-cloudflare-credentials "$CF_INI" -d "$DOMAIN" \
    --non-interactive --agree-tos --register-unsafely-without-email
else
  echo "no $CF_INI; using HTTP-01, which requires port 80 reachable and the" >&2
  echo "DNS record unproxied for this run and every renewal" >&2
  certbot certonly --standalone -d "$DOMAIN" \
    --non-interactive --agree-tos --register-unsafely-without-email
fi

# 2. The deploy hook: refresh whichever process is terminating TLS.
#    Written before the first copy so the manual step below and every
#    future renewal go through the same code.
#
#    Both boundaries are handled, and which one is live is read at
#    renewal time rather than baked in here. The node terminating TLS
#    itself needs the pair projected into a directory it can read and a
#    restart; an nginx boundary in front of a loopback-only node reads
#    /etc/letsencrypt directly as root and needs a reload. Switching
#    between them is an operator decision that must not require
#    remembering to rewrite a hook: a renewal that does not reach the
#    live listener is a certificate that expires while every file on
#    disk says it was renewed, in November, silently.
tee "$HOOK" > /dev/null <<HOOK_EOF
#!/bin/sh
# Installed by choir's setup_tls.sh: refresh whatever is terminating
# TLS for this node, whether that is the node itself or an nginx
# boundary in front of it.
set -eu
if systemctl is-active --quiet nginx 2>/dev/null; then
  systemctl reload nginx
fi
[ -f "$STATE/tls.enabled" ] || exit 0
install -o $NODE_USER -g $NODE_USER -m 600 \
  "/etc/letsencrypt/live/$DOMAIN/fullchain.pem" "$TLS_DIR/fullchain.pem"
install -o $NODE_USER -g $NODE_USER -m 600 \
  "/etc/letsencrypt/live/$DOMAIN/privkey.pem" "$TLS_DIR/privkey.pem"
sudo -u $NODE_USER XDG_RUNTIME_DIR=/run/user/$NODE_UID \
  systemctl --user restart choir-node
HOOK_EOF
chmod 755 "$HOOK"

# 3. The marker the installer reads: cert path, then key path. While it
#    exists the installer renders the public TLS bind; deleting it moves
#    termination to a proxy in front of a loopback-only node.
#
#    Written before the hook runs, not after, because the hook now reads
#    it to decide whether the node needs the pair at all. Run in the
#    other order it would find no marker on a first install, skip the
#    projection, and hand the installer a marker naming two files that
#    are not there.
install -d -o "$NODE_USER" -g "$NODE_USER" -m 700 "$STATE" "$TLS_DIR"
printf '%s\n%s\n' "$TLS_DIR/fullchain.pem" "$TLS_DIR/privkey.pem" > "$MARKER"
chown "$NODE_USER:$NODE_USER" "$MARKER" && chmod 600 "$MARKER"

# 4. First projection, through the hook itself so it is proven now, not
#    at the first renewal two months from today.
"$HOOK"

# 5. The certificate-valid route operator tools use. Without this marker
#    they fall back to the pre-TLS loopback tunnel; on the node itself that
#    either depends on a tunnel that does not exist or reaches TLS by IP and
#    fails hostname verification. Keep the name explicit and untracked.
sudo -u "$NODE_USER" sh "$HERE/configure_public_url.sh" "$DOMAIN" "$PORT"

echo "issued for $DOMAIN; pair projected to $TLS_DIR; marker written to $MARKER"
echo "next: re-run the installer to render the TLS unit, e.g."
echo "  sh ~/choir-build/scripts/flip/install_node_linux.sh $PORT '' ~/bin"
if [ -f "$CF_INI" ]; then
  echo "issued over DNS-01; keep firewall port $PORT (serving) open. No port 80"
  echo "hole is needed and the origin address was never published."
else
  echo "and keep firewall ports 80 (renewals) and $PORT (serving) open."
fi
