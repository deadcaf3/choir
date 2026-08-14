#!/bin/zsh
# Phase-0 spike: CoW workspace-provisioning benchmark (DECISIONS.md D8).
# Target: p50 < 50 ms warm. On macOS/APFS this uses clonefile via `cp -c`;
# the production target is Linux btrfs/ZFS reflinks + microVM restore, so
# these numbers are a dev-machine sanity check, not the gate measurement.
set -euo pipefail

FILES=${1:-2000}
RUNS=${2:-20}
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

# Build a synthetic repo: FILES files of ~2 KB across nested dirs.
SRC="$WORK/src"
mkdir -p "$SRC"
for i in $(seq 1 "$FILES"); do
  d="$SRC/dir$((i % 50))"
  mkdir -p "$d"
  head -c 2048 /dev/urandom > "$d/f$i.dat"
done

echo "provisioning benchmark: $FILES files, $RUNS runs (APFS clonefile)"
times_ms=()
for i in $(seq 1 "$RUNS"); do
  dst="$WORK/ws$i"
  t0=$(python3 -c 'import time; print(time.time_ns())')
  cp -Rc "$SRC" "$dst"
  t1=$(python3 -c 'import time; print(time.time_ns())')
  times_ms+=($(( (t1 - t0) / 1000000 )))
done

sorted=($(printf '%s\n' "${times_ms[@]}" | sort -n))
n=${#sorted[@]}
echo "p50: ${sorted[$(( (n + 1) / 2 ))]} ms"
echo "p90: ${sorted[$(( (n * 9 + 9) / 10 ))]} ms"
echo "max: ${sorted[$n]} ms"
