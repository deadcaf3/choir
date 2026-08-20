#!/bin/sh
# Search everything the object database holds for a string, and say
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
# Four places a string can hide, and this looks in all four:
#
#   blob contents   the obvious one
#   commit messages a rewrite that scrubs trees can leave the name in the
#                   message of every commit that touched it
#   tag messages    same, and annotated tags are objects of their own
#   path names      a file *named* after the thing leaks it without the
#                   name appearing inside any blob
#
# An earlier version read blob contents only, which is three of those
# four missed.
#
# Written after a rewrite removed a hosted repository's name from this
# repository. A working-tree grep came back clean while the blobs were
# still there; only this distinguished the two.
#
# Exit status is 0 when nothing reachable matched, 1 when something did,
# and 2 on a usage or environment error, so it can be a gate step rather
# than something someone remembers to run.
set -u
cd "$(dirname "$0")/.." || exit 2

[ $# -ge 1 ] || { echo "usage: sh scripts/scan_history.sh <string>..." >&2; exit 2; }

WORK=$(mktemp -d) || exit 2
trap 'rm -rf "$WORK"' EXIT

# An empty needle would match every object, turn the gate red, and read
# like the repository is full of the thing being hunted. Refused rather
# than reported.
for needle in "$@"; do
  [ -n "$needle" ] || { echo "empty needle: an empty string matches everything" >&2; exit 2; }
done
printf '%s\n' "$@" > "$WORK/needles"

# The reachable set is computed once. Asking `git rev-list` per hit is
# quadratic and was slow enough on a 300-commit history to look hung.
# `--objects` also gives the path each blob and tree is stored under,
# which is both the fourth thing to search and what makes a hit
# actionable: an object id alone does not say what leaked.
git rev-list --all --objects > "$WORK/objects" 2>"$WORK/err" || {
  echo "git rev-list failed:" >&2; cat "$WORK/err" >&2; exit 2; }
cut -d' ' -f1 "$WORK/objects" | sort -u > "$WORK/reachable"

git cat-file --batch-all-objects --batch-check='%(objectname) %(objecttype)' \
  > "$WORK/all" 2>"$WORK/err" || {
  echo "git cat-file failed:" >&2; cat "$WORK/err" >&2; exit 2; }
total=$(wc -l < "$WORK/all" | tr -d ' ')
# An empty object database would otherwise scan nothing and report clean,
# which is the one wrong answer this script must never give.
[ "$total" -gt 0 ] || { echo "no objects found; is this a git repository?" >&2; exit 2; }

echo "scanning $total objects and $(wc -l < "$WORK/objects" | tr -d ' ') paths for $# string(s)"

# --- Phase 1: one pass, every needle at once, every object at once.
#
# `git cat-file --batch` streams the whole database through one process
# instead of spawning one per object. On this repository that is 0.1 s
# against 26 s *per needle* for the per-object loop below, and the loop
# is quadratic in needles besides. A gate step that costs a minute is a
# gate step someone turns off.
#
# `grep -a` because blobs are binary in general: without it grep stops at
# "binary file matches" and the count below would be wrong. Phase 2 only
# runs when this pass finds something, which is never, when the guard is
# doing its job.
#
# `LC_ALL=C` is not decoration. BSD grep's `-i` in a UTF-8 locale does
# multibyte case folding over all 46 MB; in the C locale it folds ASCII
# and takes about half as long. ASCII folding is exactly what these
# needles want -- they are repository names and PEM headers.
matched=0
if git cat-file --batch-all-objects --batch --buffer 2>/dev/null \
   | LC_ALL=C grep -qaiFf "$WORK/needles"; then
  matched=1
fi
# Paths are text and come from the reachable walk, so a hit here is
# reachable by construction. `$1=""` drops the object id column, which
# would otherwise let a hex needle match an object id.
if awk 'NF>1{$1=""; print}' "$WORK/objects" | grep -qiFf "$WORK/needles"; then
  matched=1
fi

if [ "$matched" -eq 0 ]; then
  for needle in "$@"; do
    echo "  '$needle': clean"
  done
  exit 0
fi

# --- Phase 2: something matched. Now spend the time to say exactly what.
status=0
awk '{print $1}' "$WORK/all" > "$WORK/oids"

for needle in "$@"; do
  reach=0
  unreach=0
  paths=0

  # Path names first: cheap, and the most legible kind of leak.
  while IFS= read -r line; do
    case "$line" in *' '*) ;; *) continue ;; esac
    p=${line#* }
    case "$p" in
      *"$needle"*) paths=$((paths+1)); echo "  REACHABLE path $p" ;;
    esac
  done < "$WORK/objects"

  while read -r o; do
    git cat-file -p "$o" 2>/dev/null | LC_ALL=C grep -qaiF -- "$needle" || continue
    kind=$(awk -v o="$o" '$1==o{print $2}' "$WORK/all")
    if LC_ALL=C grep -qxF "$o" "$WORK/reachable"; then
      reach=$((reach+1))
      where=$(awk -v o="$o" '$1==o && NF>1{$1=""; print; exit}' "$WORK/objects")
      echo "  REACHABLE $kind $o${where:+ $where}"
    else
      unreach=$((unreach+1))
    fi
  done < "$WORK/oids"

  echo "  '$needle': $reach reachable, $unreach unreachable, $paths path(s)"
  # Unreachable hits are pending garbage collection and cannot be served
  # by a clone; reachable ones, and any path, are still part of the
  # repository.
  [ "$reach" -eq 0 ] && [ "$paths" -eq 0 ] || status=1
done

exit $status
