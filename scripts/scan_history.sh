#!/bin/sh
# Search every blob the object database holds for a string, and say
# whether each hit is still reachable from a ref.
#
#   sh scripts/scan_history.sh <string> [<string>...]
#
# Why this is not `git log -S`: that searches commit *diffs*, so a string
# that entered and left within one commit's tree never appears in it, and
# it says nothing about objects left dangling by a history rewrite. This
# walks the object database itself, which is the only thing that answers
# "is it actually gone".
#
# Written after a rewrite removed a hosted repository's name from this
# repository. A working-tree grep came back clean while the blobs were
# still there; only this distinguished the two.
#
# Exit status is 0 when nothing reachable matched and 1 when something
# did, so it can be a gate step rather than something someone remembers
# to run.
set -u
cd "$(dirname "$0")/.." || exit 1

[ $# -ge 1 ] || { echo "usage: sh scripts/scan_history.sh <string>..." >&2; exit 2; }

WORK=$(mktemp -d) || exit 1
trap 'rm -rf "$WORK"' EXIT

# The reachable set is computed once. Asking `git rev-list` per hit is
# quadratic and was slow enough on a 300-commit history to look hung.
git rev-list --all --objects 2>/dev/null | cut -d' ' -f1 | sort -u > "$WORK/reachable"
git cat-file --batch-all-objects --batch-check='%(objectname) %(objecttype)' \
  | awk '$2=="blob"{print $1}' > "$WORK/blobs"

total=$(wc -l < "$WORK/blobs" | tr -d ' ')
echo "scanning $total blobs for $# string(s)"

# One pass for every needle at once, before the per-needle loop below.
# That loop spawns `git cat-file` once per blob *per needle* -- about 26 s
# for a 1,900-blob history -- so N needles cost N times a clean run, which
# is how a gate step gets turned off. `grep -f` costs the same for ten
# needles as for one, and when nothing matches, which is every run where
# the guard is doing its job, the loop below never runs at all.
printf '%s\n' "$@" > "$WORK/needles"
matched=0
while read -r o; do
  if git cat-file blob "$o" 2>/dev/null | grep -qiFf "$WORK/needles"; then
    matched=1
    break
  fi
done < "$WORK/blobs"
if [ "$matched" -eq 0 ]; then
  for needle in "$@"; do
    echo "  '$needle': clean (0 blobs)"
  done
  exit 0
fi

status=0
for needle in "$@"; do
  : > "$WORK/hits"
  while read -r o; do
    if git cat-file blob "$o" 2>/dev/null | grep -qiF -- "$needle"; then
      echo "$o" >> "$WORK/hits"
    fi
  done < "$WORK/blobs"

  n=$(wc -l < "$WORK/hits" | tr -d ' ')
  if [ "$n" -eq 0 ]; then
    echo "  '$needle': clean (0 blobs)"
    continue
  fi

  reach=0
  while read -r o; do
    if grep -qx "$o" "$WORK/reachable"; then
      reach=$((reach+1))
      echo "  REACHABLE $o"
    fi
  done < "$WORK/hits"

  echo "  '$needle': $n blob(s), $reach reachable"
  # Unreachable hits are pending garbage collection and cannot be served
  # by a clone; reachable ones are still part of the repository.
  [ "$reach" -eq 0 ] || status=1
done

exit $status
