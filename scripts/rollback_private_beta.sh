#!/bin/sh
# Swap back to the release archived by deploy_private_beta.sh.
set -eu

[ "$#" -eq 2 ] || {
  echo "usage: rollback_private_beta.sh <install-root> <systemd-service>" >&2
  exit 2
}

INSTALL_ROOT=$1
SERVICE=$2
PREVIOUS=$(readlink "$INSTALL_ROOT/previous" 2>/dev/null || true)
[ -n "$PREVIOUS" ] || { echo "rollback-private-beta: no previous release" >&2; exit 1; }
[ -x "$PREVIOUS/choir-node" ] || { echo "rollback-private-beta: previous release is incomplete" >&2; exit 1; }

CURRENT=$(readlink "$INSTALL_ROOT/current" 2>/dev/null || true)
ln -sfn "$PREVIOUS" "$INSTALL_ROOT/current.new"
mv -Tf "$INSTALL_ROOT/current.new" "$INSTALL_ROOT/current"
if ! systemctl restart "$SERVICE" || ! systemctl is-active --quiet "$SERVICE"; then
  if [ -n "$CURRENT" ]; then
    ln -sfn "$CURRENT" "$INSTALL_ROOT/current.new"
    mv -Tf "$INSTALL_ROOT/current.new" "$INSTALL_ROOT/current"
    systemctl restart "$SERVICE" || true
  fi
  echo "rollback-private-beta: rollback failed; attempted to restore current" >&2
  exit 1
fi
echo "rollback-private-beta: active release is now $PREVIOUS"
