#!/bin/sh
# Redraw the social-preview card from `scripts/card.html`.
#
# The card is the only binary this node ships, and a binary with no
# generator is a file nobody can correct: the previous one carried a
# palette the surface had already stopped using, and there was no way to
# redraw it short of opening an image editor. This script is that way.
#
# It renders at twice the size and downsamples, because a headless
# screenshot at 1x sets the wordmark with one pixel of antialiasing and
# the serif loses its thin strokes.
#
# Usage: scripts/render_card.sh [browser]
set -eu

here=$(cd "$(dirname "$0")" && pwd)
out="$here/../crates/choir-node/src/card.png"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

browser=${1:-/Applications/Google Chrome.app/Contents/MacOS/Google Chrome}
if [ ! -x "$browser" ]; then
  echo "no browser at $browser -- pass one as the first argument" >&2
  exit 1
fi

"$browser" --headless=new --disable-gpu --hide-scrollbars \
  --force-device-scale-factor=2 --window-size=1200,630 \
  --virtual-time-budget=2000 \
  --screenshot="$tmp/card2x.png" "file://$here/card.html" >/dev/null 2>&1

# 1200x630 is what every preview renderer expects; the source is drawn
# at that size and rendered at twice it.
sips -Z 1200 "$tmp/card2x.png" --out "$out" >/dev/null
echo "wrote $out"
