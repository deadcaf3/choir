#!/bin/sh
# Run a directory of mutations one at a time and report which ones the
# test suite fails to catch.
#
#   sh scripts/mutation_run.sh <mutation-dir> <cargo-test-args...>
#
# Each mutation is a `*.py` file in the directory that edits exactly one
# tracked file in place. The runner does not need to be told which file:
# it asks git afterwards, which is also how it enforces that an edit
# landed at all.
#
# The four rules below are each here because breaking one produced a
# false result that read as a pass:
#
#   1. The tree must be committed before the first mutation. Restore is
#      `git checkout -- <file>`, which restores from HEAD, so uncommitted
#      work would be destroyed by the restore rather than by the edit.
#   2. A mutation that changes nothing is a failure, not a survivor. A
#      `sed` range that silently matches nothing reads exactly like a
#      mutation the tests missed.
#   3. Restore the FILE by name, never its directory, and prove the
#      restore with `git status` rather than with the hand that made the
#      edit.
#   4. A compile failure proves nothing. Detect it by "could not
#      compile" -- cargo also exits nonzero when tests merely fail, so
#      the exit code alone cannot tell the two apart.
#
# A mutation that is NOT CAUGHT is a finding about the tests, never
# evidence that some other check covers it.
set -u
cd "$(dirname "$0")/.." || exit 1

DIR=${1:-}
[ -n "$DIR" ] || { echo "usage: sh scripts/mutation_run.sh <dir> <cargo-test-args...>" >&2; exit 2; }
shift

[ -d "$DIR" ] || { echo "no such mutation directory: $DIR" >&2; exit 2; }

# Rule 1.
if [ -n "$(git status --porcelain)" ]; then
  echo "refusing to run: working tree is dirty, and restore is from HEAD" >&2
  git status --short >&2
  exit 2
fi

LOG=${TMPDIR:-/tmp}/choir-mutation
mkdir -p "$LOG"

# Build the unmutated suite once, up front. Every mutation's `cargo test`
# then recompiles only the mutated crate and its dependents instead of
# the first one paying for the whole build -- and a baseline that does
# not even compile is reported before any mutation runs, rather than
# reading as every mutation "not compiling". Some argument shapes reject
# `--no-run` (`--doc` does); that only forfeits the head start, so it is
# a fatal error only when the tree itself failed to compile.
if ! cargo test --no-run "$@" >"$LOG/prebuild.log" 2>&1; then
  if grep -q "could not compile" "$LOG/prebuild.log"; then
    echo "the UNMUTATED tree does not build with these arguments; nothing was mutated" >&2
    grep -E '^error(\[|:)' "$LOG/prebuild.log" | head -10 >&2
    echo "full log: $LOG/prebuild.log" >&2
    exit 2
  fi
  echo "note: prebuild skipped (cargo rejected --no-run with these arguments)" >&2
fi

# Rule 3, also on the way out: an interrupt mid-mutation must not leave
# the tree mutated -- the next run would refuse to start, and a reader
# of the tree would be reading the mutation.
CHANGED=
restore() {
  for f in $CHANGED; do git checkout -- "$f"; done
  CHANGED=
}
trap 'restore; exit 130' INT TERM

# `findings` are mutations the tests missed; `void` are mutations that
# produced no evidence at all (script errored, matched nothing, or did
# not compile). Both fail the run: rule 2 calls a no-op mutation a
# failure, and until now that failure was a printed line with a green
# exit code, which is exactly the kind of check that quietly stops
# checking.
findings=0
void=0
ran=0

for m in "$DIR"/*.py; do
  [ -e "$m" ] || { echo "no *.py mutations in $DIR" >&2; exit 2; }
  name=$(basename "$m" .py)
  ran=$((ran+1))

  if ! python3 "$m"; then
    echo "$name: MUTATION SCRIPT ERRORED"
    void=$((void+1))
    continue
  fi

  # Rule 2, and it also tells us what to restore.
  CHANGED=$(git status --porcelain | awk '{print $2}')
  if [ -z "$CHANGED" ]; then
    echo "$name: NO EDIT LANDED (the mutation matched nothing)"
    void=$((void+1))
    continue
  fi

  t0=$(date +%s)
  cargo test "$@" > "$LOG/$name.log" 2>&1
  code=$?
  el=$(($(date +%s) - t0))

  # Rule 4.
  if grep -q "could not compile" "$LOG/$name.log"; then
    echo "$name: did not compile (proves nothing)  [${el}s]"
    void=$((void+1))
  elif [ "$code" -eq 0 ]; then
    echo "$name: NOT CAUGHT  <-- finding  [${el}s]"
    findings=$((findings+1))
  else
    echo "$name: caught  [${el}s]"
    grep -E "^test .* FAILED" "$LOG/$name.log" | grep -v "test result" | head -4
  fi

  # Rule 3.
  restore
  leftover=$(git status --porcelain)
  [ -z "$leftover" ] || { echo "$name: NOT RESTORED: $leftover" >&2; exit 1; }
done

echo "--- $ran mutation(s), $findings not caught, $void proved nothing ---"
git status --porcelain
[ "$findings" -eq 0 ] && [ "$void" -eq 0 ]
