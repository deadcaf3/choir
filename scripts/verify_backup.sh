#!/bin/sh
# Pull the offsite backup back and check it, rather than trusting that
# writing it worked. A backup nobody has restored is a hypothesis; this
# is the cheap repeatable half of the rehearsal in
# scripts/flip/RUNBOOK.md, which stops short of booting a node.
#
# Checks, in the order a real restore would hit them:
#   1. the log arrives and every line carries a seq
#   2. seq starts at 0 and has no gaps  (a truncated middle is the
#      failure a checksum on the whole file cannot localise)
#   3. the nine policy/config files are present (a node restored without
#      reviewers refuses to boot -- that is how this was found)
#   4. no secret travelled: no auth, no *.key, no *.pem
#   5. when this machine holds the live log, the backup is a byte-exact
#      *prefix* of it -- the correct relation for an append-only log,
#      and stronger than equality, which would fail on every op
#      appended since the last sync
#
# Run as `sh scripts/choirctl verify-backup`. Exits nonzero on any
# failure so it can be wired to a schedule later.
set -eu

IP=${1:-$(cat "$HOME/.choir-mirror-ip")}
KEY=$HOME/.ssh/choir_bench_ed25519
SSH_CONTROL=$HOME/.ssh/cm-choir-mirror.sock
WORK=$(mktemp -d)
VERIFY_BIN=${CHOIR_NODE_BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/release/choir-node}
trap 'rm -rf "$WORK"' EXIT INT TERM

# After the D20 host move the directions swap: the live log is on the
# node host and the backup is the local pull. The checks are the
# same; what changes is which side is read over ssh. The marker file is
# the same one choirctl and pull_backup.sh key off.
REMOTE_NODE=
[ -f "$HOME/.choir/node-remote" ] && REMOTE_NODE=1
POLICY_FILES='keys reviewers protected-refs newcomer-audit.jsonl newcomer-adjudications.jsonl review-adjudications.jsonl repos.list acl private-beta.manifest'

ssh_run() {
  ssh -i "$KEY" -o IdentitiesOnly=yes -o BatchMode=yes -o ConnectTimeout=10 \
      -o ControlMaster=auto -o ControlPath="$SSH_CONTROL" -o ControlPersist=600 \
      "choir@$IP" "$@"
}

fail() { echo "verify-backup: $1" >&2; exit 1; }
[ -x "$VERIFY_BIN" ] \
  || fail "no verifier at $VERIFY_BIN (build choir-node --release or set CHOIR_NODE_BIN)"

# 1. The log: whichever side holds the backup copy.
if [ -n "$REMOTE_NODE" ]; then
  cp "${CHOIR_BACKUP_DIR:-$HOME/choir-oplog}/ops.jsonl" "$WORK/ops.jsonl" 2>/dev/null \
    || fail "no local backup at ~/choir-oplog/ops.jsonl (never pulled?)"
else
  ssh_run 'cat ~/choir-oplog/ops.jsonl' > "$WORK/ops.jsonl" 2>/dev/null \
    || fail "could not read ~/choir-oplog/ops.jsonl on the mirror (never synced?)"
fi
lines=$(wc -l < "$WORK/ops.jsonl" | tr -d ' ')
[ "$lines" -gt 0 ] || fail "the backed-up log is empty"

# 2. The daemon's read-only verifier checks the supported wire format,
#    sequence, every parent link, and every recomputed entry hash. Refuse
#    even a repairable torn tail here: a completed backup must end on a
#    completed record.
"$VERIFY_BIN" --verify-log "$WORK/ops.jsonl" \
  || fail "the backed-up log failed format/sequence/parent/hash verification"

# 3. Policy. Without these a restore does not boot.
if [ -n "$REMOTE_NODE" ]; then
  ls "${CHOIR_BACKUP_DIR:-$HOME/choir-oplog}/policy" > "$WORK/policy" 2>/dev/null || : > "$WORK/policy"
else
  ssh_run 'ls ~/choir-oplog/policy' > "$WORK/policy" 2>/dev/null || : > "$WORK/policy"
fi
missing=
for f in $POLICY_FILES; do
  grep -qx "$f" "$WORK/policy" || missing="$missing $f"
done
[ -z "$missing" ] || fail "policy files missing from the backup:$missing"

# 4. Secrets must not be there. This is an assertion about the *backup*,
#    not about the script that writes it, so it still holds if someone
#    copies a file up by hand.
leaked=$(grep -E '^(auth)$|\.key$|\.pem$' "$WORK/policy" || true)
[ -z "$leaked" ] || fail "SECRETS IN THE BACKUP: $leaked (a backup holding a token or key is a credential channel)"

# 5. Prefix against the live log — read locally when this machine hosts
#    the node, over ssh when the node host does. Stronger than equality,
#    which would fail on every op appended since the last sync.
LIVE=${CHOIR_NODE_STATE:-$HOME/.choir/repos/.choir}/ops.jsonl
prefix="skipped (no live log here)"
if [ -n "$REMOTE_NODE" ]; then
  ssh_run 'cat ~/.choir/repos/.choir/ops.jsonl' > "$WORK/live.jsonl" 2>/dev/null \
    || fail "could not read the live log on the node host"
  LIVE=$WORK/live.jsonl
fi
if [ -f "$LIVE" ]; then
  backup_bytes=$(wc -c < "$WORK/ops.jsonl" | tr -d ' ')
  live_bytes=$(wc -c < "$LIVE" | tr -d ' ')
  [ "$backup_bytes" -le "$live_bytes" ] \
    || fail "the backup is LONGER than the live log ($backup_bytes > $live_bytes): they have diverged"
  head -c "$backup_bytes" "$LIVE" | cmp -s - "$WORK/ops.jsonl" \
    || fail "the backup is not a prefix of the live log: the two have diverged, which an append-only log cannot do"
  prefix="ok ($backup_bytes of $live_bytes bytes)"
fi

echo "verify-backup: $lines ops, full chain verified, 9 policy/config files, no secrets, prefix $prefix"
