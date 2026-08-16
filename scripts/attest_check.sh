#!/bin/sh
# One repository's bundle, checked against what the node attested (D25),
# with the three outcomes D47 settled on.
#
#   attest_check.sh <repo> <attested-refs-file> <bundle-refs-file>
#
# Both files are `<oid> <ref>` per line, sorted. This decides; it reads
# nothing, fetches nothing and writes nothing, so the decision can be
# exercised with fixtures instead of a node.
#
#   attested rows | bundle  | outcome
#   --------------|---------|---------------------------------------
#   present       | matches | verified                    (exit 0)
#   present       | differs | divergence                  (exit 1)
#   none          | any     | copied, UNVERIFIED by name  (exit 0)
#
# The third row exists because refs that never went through the
# sequencer cannot appear in an attestation, so comparing them against
# one compares a real bundle with an empty set and fails forever. That
# was a live failure on 2026-08-16: an imported repository made every
# hourly backup exit nonzero, and a backup that always reports failure
# is one that stops being read.
#
# It is reported, never silenced. The gap it names is real — for such a
# repository the op log is not the authority on ref state — and the way
# to close it is to push those refs *through* the node so they are
# sequenced, not to manufacture the attestation rows that would make
# this pass.
set -eu

if [ "$#" -ne 3 ]; then
  echo "usage: attest_check.sh <repo> <attested-refs-file> <bundle-refs-file>" >&2
  exit 2
fi

repo=$1
attested=$2
bundled=$3

for f in "$attested" "$bundled"; do
  [ -f "$f" ] || { echo "attest-check: no such file: $f" >&2; exit 2; }
done

if [ ! -s "$attested" ]; then
  # UNVERIFIED is upper case on purpose: this line sits in a run whose
  # other repositories printed nothing, and it must not read as one more
  # quiet success.
  echo "attest-check: $repo UNVERIFIED — hosted but never sequenced, so no attested ref-state exists to check its bundle against"
  exit 0
fi

if cmp -s "$attested" "$bundled"; then
  echo "attest-check: $repo verified against the node's attested ref-state"
  exit 0
fi

echo "attest-check: $repo does not match the node's attested ref-state (re-run once; a persistent mismatch is divergence)" >&2
exit 1
