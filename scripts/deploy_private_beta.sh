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

# One list, checked and then installed. It used to be two -- five files
# validated, seven installed -- so a missing SBOM passed validation and
# failed halfway through the install loop, after the release directory
# existed. Combined with the "release already exists" guard below, that
# left the version permanently undeployable.
FILES='choir-node choir private-beta.manifest release-manifest.json SHA256SUMS choir-node.cdx.json choir.cdx.json'

[ -d "$ARTIFACT" ] || fail "artifact directory is missing: $ARTIFACT"
for file in $FILES; do
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
# Anything that fails from here until the swap succeeds takes the
# half-written release with it. Without this, a deploy that failed for a
# reason the operator then fixed could never be retried: the guard above
# would refuse the version forever, and the fix was an undocumented
# `rm -rf` on a production install root.
trap 'rm -rf "$RELEASE"' EXIT
for file in $FILES; do
  install -m 0644 "$ARTIFACT/$file" "$RELEASE/$file"
done
chmod 0755 "$RELEASE/choir-node" "$RELEASE/choir"

CURRENT=$(readlink "$INSTALL_ROOT/current" 2>/dev/null || true)
PRIOR_PREVIOUS=$(readlink "$INSTALL_ROOT/previous" 2>/dev/null || true)
if [ -n "$CURRENT" ]; then
  ln -sfn "$CURRENT" "$INSTALL_ROOT/previous.new"
  mv -Tf "$INSTALL_ROOT/previous.new" "$INSTALL_ROOT/previous"
fi
ln -sfn "$RELEASE" "$INSTALL_ROOT/current.new"
mv -Tf "$INSTALL_ROOT/current.new" "$INSTALL_ROOT/current"

# Undo everything the swap above did, so the EXIT trap is always safe to
# let delete $RELEASE: after this runs, nothing points at it.
#
# `previous` is put back to what it was, not left at $CURRENT. It was
# repointed before the swap, so leaving it there once $CURRENT is live
# makes current and previous the same release -- and a rollback run
# against that state changes nothing and reports success, which is the
# worst thing a rollback can do.
restore_previous_release() {
  if [ -n "$CURRENT" ]; then
    ln -sfn "$CURRENT" "$INSTALL_ROOT/current.new"
    mv -Tf "$INSTALL_ROOT/current.new" "$INSTALL_ROOT/current"
  else
    # There was nothing here before this deploy, so leaving the symlink
    # would leave it dangling at a directory the trap is about to remove.
    rm -f "$INSTALL_ROOT/current"
  fi
  if [ -n "$PRIOR_PREVIOUS" ]; then
    ln -sfn "$PRIOR_PREVIOUS" "$INSTALL_ROOT/previous.new"
    mv -Tf "$INSTALL_ROOT/previous.new" "$INSTALL_ROOT/previous"
  else
    rm -f "$INSTALL_ROOT/previous"
  fi
  [ -n "$CURRENT" ] && { systemctl restart "$SERVICE" || true; }
  return 0
}

if ! systemctl restart "$SERVICE"; then
  restore_previous_release
  fail "service restart failed; the prior release was restored when available"
fi
# A restart that returns zero and a unit that is running are two claims,
# and this one used to be checked after the release was already live with
# no way back. It restores like the branch above now.
if ! systemctl is-active --quiet "$SERVICE"; then
  restore_previous_release
  fail "service is not active after restart; the prior release was restored when available"
fi
# Past the point of no return: the new release is live and must survive.
trap - EXIT
echo "deploy-private-beta: $VERSION active; previous=${CURRENT:-none}"
