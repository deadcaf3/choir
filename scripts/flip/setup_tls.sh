#!/bin/sh
# One-time TLS enablement for a Linux node host: issue a Let's Encrypt
# certificate for the node's public name, project the pair to where the
# node's user can read it, install a renewal hook that keeps doing so,
# and write the tls.enabled marker the installer reads. Run ON the node
# host as the node's user; the certbot and hook steps need sudo.
#
# Usage: sh setup_tls.sh <domain> [port]
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
# Issuance and renewal use --standalone on port 80, so the host's
# firewall must allow inbound 80 permanently — renewals rebind it every
# ~60 days. Nothing else ever serves on 80 here.
set -eu

DOMAIN=${1:?usage: setup_tls.sh <domain> [port]}
PORT=${2:-8417}
STATE=$HOME/.choir
TLS_DIR=$STATE/tls
MARKER=$STATE/tls.enabled
HOOK=/etc/letsencrypt/renewal-hooks/deploy/choir-tls
NODE_USER=$(id -un)
NODE_UID=$(id -u)

command -v certbot >/dev/null 2>&1 \
  || { echo "certbot is not installed; run: sudo apt-get install -y certbot" >&2; exit 1; }

# 1. Issue. Idempotent: certbot keeps the existing lineage if one exists.
sudo certbot certonly --standalone -d "$DOMAIN" \
  --non-interactive --agree-tos --register-unsafely-without-email

# 2. The deploy hook: copy the pair somewhere the node user owns, then
#    restart the node so it serves the fresh cert. Written before the
#    first copy so the manual step below and every future renewal go
#    through the same code.
sudo tee "$HOOK" > /dev/null <<HOOK_EOF
#!/bin/sh
# Installed by choir's setup_tls.sh: project the renewed cert pair to
# the node user's state dir and restart the node unit.
set -eu
install -o $NODE_USER -g $NODE_USER -m 600 \
  "/etc/letsencrypt/live/$DOMAIN/fullchain.pem" "$TLS_DIR/fullchain.pem"
install -o $NODE_USER -g $NODE_USER -m 600 \
  "/etc/letsencrypt/live/$DOMAIN/privkey.pem" "$TLS_DIR/privkey.pem"
sudo -u $NODE_USER XDG_RUNTIME_DIR=/run/user/$NODE_UID \
  systemctl --user restart choir-node
HOOK_EOF
sudo chmod 755 "$HOOK"

# 3. First projection, through the hook itself so it is proven now, not
#    at the first renewal two months from today.
mkdir -p "$TLS_DIR" && chmod 700 "$TLS_DIR"
sudo "$HOOK"

# 4. The marker the installer reads: cert path, then key path. Once this
#    exists every reinstall keeps the public TLS bind, same one-way
#    marker discipline as the review and scope gates.
printf '%s\n%s\n' "$TLS_DIR/fullchain.pem" "$TLS_DIR/privkey.pem" > "$MARKER"
chmod 600 "$MARKER"

echo "issued for $DOMAIN; pair projected to $TLS_DIR; marker written to $MARKER"
echo "next: re-run the installer to render the TLS unit, e.g."
echo "  sh ~/choir-build/scripts/flip/install_node_linux.sh $PORT '' ~/bin"
echo "and keep firewall ports 80 (renewals) and $PORT (serving) open."
