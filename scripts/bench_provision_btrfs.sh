#!/bin/bash
# Phase-0 gate measurement (D8), Linux target: workspace provisioning via
# btrfs subvolume snapshots. Run on a btrfs mount, e.g.:
#   ./bench_provision_btrfs.sh /mnt/choir 2000 20
# Companion to the macOS clonefile results in the build log; this is the number
# the gate formally wants (target p50 < 50 ms warm).
set -euo pipefail

ROOT=${1:?usage: bench_provision_btrfs.sh <btrfs-mount> [files] [runs]}
FILES=${2:-2000}
RUNS=${3:-20}
WORK="$ROOT/bench-$$"
mkdir -p "$WORK"
cleanup() {
  for s in "$WORK"/ws* "$WORK/src"; do
    btrfs subvolume delete "$s" >/dev/null 2>&1 || true
  done
  rm -rf "$WORK"
}
trap cleanup EXIT

# Base tree as a subvolume: FILES files of ~2 KB across nested dirs.
btrfs subvolume create "$WORK/src" >/dev/null
for i in $(seq 1 "$FILES"); do
  d="$WORK/src/dir$((i % 50))"
  mkdir -p "$d"
  head -c 2048 /dev/urandom > "$d/f$i.dat"
done
sync

echo "btrfs snapshot provisioning: $FILES files, $RUNS runs"
times_ms=()
for i in $(seq 1 "$RUNS"); do
  t0=$(date +%s%N)
  btrfs subvolume snapshot "$WORK/src" "$WORK/ws$i" >/dev/null
  t1=$(date +%s%N)
  times_ms+=($(( (t1 - t0) / 1000000 )))
done

mapfile -t sorted < <(printf '%s\n' "${times_ms[@]}" | sort -n)
n=${#sorted[@]}
echo "p50: ${sorted[$(( n / 2 ))]} ms"
echo "p90: ${sorted[$(( n * 9 / 10 ))]} ms"
echo "max: ${sorted[$(( n - 1 ))]} ms"
