#!/usr/bin/env bash
# demo/run.sh -- build the binaries, then play the two-pane demo (demo/tui.py).
#
#   demo/run.sh [--no-build] [tui options...]
#
# --no-build   use the binaries as built; skips cargo.
# Everything else goes to tui.py: --auto [SECS], --dump, --port N, --agents N.
#
# Builds into demo/.target, a target dir of the demo's own: a target dir
# shared across checkouts hands whoever builds last the debug/choir-node
# path, and a take would then run a node built from somebody else's
# sources. Needs: cargo, git, curl, python3.

set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
TARGET="$HERE/.target"
BUILD=1
ARGS=()
for a in "$@"; do
  case "$a" in
    --no-build) BUILD=0 ;;
    *) ARGS+=("$a") ;;
  esac
done
cd "$HERE/.."
if [ "$BUILD" = 1 ]; then
  echo "building choir-node and choir into demo/.target (a minute or two the first time)"
  cargo build -q --target-dir "$TARGET" -p choir-node -p choir-cli
fi
exec python3 "$HERE/tui.py" --bin "$TARGET/debug" "${ARGS[@]}"
