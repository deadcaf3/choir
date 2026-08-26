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
[ "$CURRENT" != "$PREVIOUS" ] || {
  echo "rollback-private-beta: current and previous are the same release; nothing to roll back to" >&2
  exit 1
}
ln -sfn "$PREVIOUS" "$INSTALL_ROOT/current.new"
mv -Tf "$INSTALL_ROOT/current.new" "$INSTALL_ROOT/current"
# The release being left becomes the one to go back to. Without this,
# `previous` still names the release now running: current and previous
# are equal, the next rollback moves nothing and says it succeeded, and
# there is no way to undo a rollback that turned out to be the wrong
# call. Rolling back twice now returns to where it started.
if [ -n "$CURRENT" ]; then
  ln -sfn "$CURRENT" "$INSTALL_ROOT/previous.new"
  mv -Tf "$INSTALL_ROOT/previous.new" "$INSTALL_ROOT/previous"
fi
if ! systemctl restart "$SERVICE" || ! systemctl is-active --quiet "$SERVICE"; then
  if [ -n "$CURRENT" ]; then
    ln -sfn "$CURRENT" "$INSTALL_ROOT/current.new"
    mv -Tf "$INSTALL_ROOT/current.new" "$INSTALL_ROOT/current"
    ln -sfn "$PREVIOUS" "$INSTALL_ROOT/previous.new"
    mv -Tf "$INSTALL_ROOT/previous.new" "$INSTALL_ROOT/previous"
    systemctl restart "$SERVICE" || true
  fi
  echo "rollback-private-beta: rollback failed; attempted to restore current" >&2
  exit 1
fi
echo "rollback-private-beta: active release is now $PREVIOUS"
