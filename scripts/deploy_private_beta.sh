#!/bin/sh
# Atomic host-local deployment. The caller has already promoted the
# artifact; this script validates it, archives the current target, swaps
# one symlink, and restarts one service.
set -eu

[ "$#" -eq 3 ] || {
  echo "usage: deploy_private_beta.sh <artifact-dir> <install-root> <systemd-service>" >&2
  exit 2
}

ARTIFACT=$1
INSTALL_ROOT=$2
SERVICE=$3
fail() { echo "deploy-private-beta: $1" >&2; exit 1; }

[ -d "$ARTIFACT" ] || fail "artifact directory is missing: $ARTIFACT"
for file in choir-node choir private-beta.manifest release-manifest.json SHA256SUMS; do
  [ -f "$ARTIFACT/$file" ] || fail "artifact is incomplete: $file"
done
(cd "$ARTIFACT" && sha256sum -c SHA256SUMS) \
  || fail "artifact checksum verification failed"

VERSION=$(sed -n 's/.*"version":"\([^"]*\)".*/\1/p' "$ARTIFACT/release-manifest.json")
case "$VERSION" in ''|*/*|*' '*) fail "release manifest has an invalid version" ;; esac
RELEASE="$INSTALL_ROOT/releases/$VERSION"
[ ! -e "$RELEASE" ] || fail "release already exists: $RELEASE"
mkdir -p "$INSTALL_ROOT/releases"
mkdir "$RELEASE"
for file in choir-node choir private-beta.manifest release-manifest.json SHA256SUMS choir-node.cdx.json choir.cdx.json; do
  install -m 0644 "$ARTIFACT/$file" "$RELEASE/$file"
done
chmod 0755 "$RELEASE/choir-node" "$RELEASE/choir"

CURRENT=$(readlink "$INSTALL_ROOT/current" 2>/dev/null || true)
if [ -n "$CURRENT" ]; then
  ln -sfn "$CURRENT" "$INSTALL_ROOT/previous.new"
  mv -Tf "$INSTALL_ROOT/previous.new" "$INSTALL_ROOT/previous"
fi
ln -sfn "$RELEASE" "$INSTALL_ROOT/current.new"
mv -Tf "$INSTALL_ROOT/current.new" "$INSTALL_ROOT/current"

if ! systemctl restart "$SERVICE"; then
  if [ -n "$CURRENT" ]; then
    ln -sfn "$CURRENT" "$INSTALL_ROOT/current.new"
    mv -Tf "$INSTALL_ROOT/current.new" "$INSTALL_ROOT/current"
    systemctl restart "$SERVICE" || true
  fi
  fail "service restart failed; the prior release was restored when available"
fi
systemctl is-active --quiet "$SERVICE" || fail "service is not active after restart"
echo "deploy-private-beta: $VERSION active; previous=${CURRENT:-none}"
