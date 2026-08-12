#!/bin/sh
# The mirror image of push_mirror.sh's backup legs, for after the D20
# host move: the canonical node lives on the VM, so the offsite copy of
# its op log has to live here. A node whose live log and only backup sit
# on the same disk has no backup, which is the exact situation this
# repository refused to enter in the other direction.
#
# Pulls, by explicit name and nothing else:
#   - ~/.choir/repos/.choir/ops.jsonl   (the live log)
#   - ~/.choir/repos/.choir/node.fingerprint
#   - the five policy files a restore needs to boot
#
# Never pulled, and refused if seen: auth, *.key, *.pem. The signing key
# stays on the node that owns it, and a bearer token in a backup is a
# credential-distribution channel, not an availability measure.
#
# Run as `sh scripts/choirctl pull-backup`. Exits nonzero on any failure
# so it can be wired to a schedule later.
set -eu

IP=${1:-$(cat "$HOME/.choir-mirror-ip")}
KEY=$HOME/.ssh/choir_bench_ed25519
SSH_CONTROL=$HOME/.ssh/cm-choir-mirror.sock
DEST=${CHOIR_BACKUP_DIR:-$HOME/choir-oplog}
REMOTE_STATE='~/.choir/repos/.choir'
REMOTE_HOME='~/.choir'

ssh_run() {
  ssh -i "$KEY" -o IdentitiesOnly=yes -o BatchMode=yes -o ConnectTimeout=10 \
      -o ControlMaster=auto -o ControlPath="$SSH_CONTROL" -o ControlPersist=600 \
      "choir@$IP" "$@"
}

fail() { echo "pull-backup: $1" >&2; exit 1; }

# Direction guard: pulling only makes sense when the node is remote. Run
# on the machine that hosts the live log, this would overwrite the real
# backup relation with a self-copy that verifies vacuously.
[ -f "$HOME/.choir/node-remote" ] \
  || fail "no ~/.choir/node-remote marker: this machine is not a follower of a remote node"

mkdir -p "$DEST"

# 1. The log, written to .part and renamed so an interrupted transfer
#    leaves the previous good backup in place instead of a truncated log
#    that still looks like a log.
ssh_run "cat $REMOTE_STATE/ops.jsonl" > "$DEST/ops.jsonl.part" 2>/dev/null \
  || fail "could not read the live log on the node host"
lines=$(wc -l < "$DEST/ops.jsonl.part" | tr -d ' ')
[ "$lines" -gt 0 ] || fail "the pulled log is empty"

# 2. Verified, not hoped: checksum both ends. A copy that dropped bytes
#    in flight reads exactly like a successful backup until restore day.
local_sum=$(shasum -a 256 < "$DEST/ops.jsonl.part" | awk '{print $1}')
remote_sum=$(ssh_run "sha256sum < $REMOTE_STATE/ops.jsonl | cut -d' ' -f1")
[ "$local_sum" = "$remote_sum" ] \
  || fail "OP LOG BACKUP MISMATCH local=$local_sum remote=$remote_sum"

# 3. Append-only means the previous pull must be a byte-exact prefix of
#    this one. A pull that fails this has watched the live log diverge
#    from its own history, which an append-only log cannot do -- keep the
#    old copy and start asking questions rather than paper over it.
if [ -f "$DEST/ops.jsonl" ]; then
  old_bytes=$(wc -c < "$DEST/ops.jsonl" | tr -d ' ')
  new_bytes=$(wc -c < "$DEST/ops.jsonl.part" | tr -d ' ')
  [ "$old_bytes" -le "$new_bytes" ] \
    || fail "the live log is SHORTER than the last pull ($new_bytes < $old_bytes): refusing to shrink the backup"
  head -c "$old_bytes" "$DEST/ops.jsonl.part" | cmp -s - "$DEST/ops.jsonl" \
    || fail "the last pull is not a prefix of the live log: the two have diverged"
fi

# 4. Contiguity, same as verify_backup.sh: a truncated middle is the
#    failure a whole-file checksum cannot localise.
sed -n 's/.*"seq":\([0-9][0-9]*\).*/\1/p' "$DEST/ops.jsonl.part" > "$DEST/.seqs"
seqcount=$(wc -l < "$DEST/.seqs" | tr -d ' ')
[ "$seqcount" = "$lines" ] \
  || fail "$lines lines but $seqcount carry a seq: the log is not all op entries"
awk 'NR-1 != $1 { printf "seq gap: line %d carries seq %d\n", NR, $1; exit 1 }' "$DEST/.seqs" \
  || fail "the pulled log has a gap; it cannot be replayed to the head"
rm -f "$DEST/.seqs"
mv "$DEST/ops.jsonl.part" "$DEST/ops.jsonl"

# 5. The pin travels with the log, so a restore onto a fresh host refuses
#    to append under a new identity instead of doing it silently.
ssh_run "cat $REMOTE_STATE/node.fingerprint" > "$DEST/node.fingerprint.part" \
  && mv "$DEST/node.fingerprint.part" "$DEST/node.fingerprint" \
  || fail "could not pull node.fingerprint (the pin must travel with the log)"

# 6. Policy, by explicit name: what a restore needs to boot and nothing
#    else. Pulling a directory would inherit whatever lands there,
#    including a key someone copies in by accident; a name list cannot.
for f in keys reviewers protected-refs newcomer-audit.jsonl newcomer-adjudications.jsonl; do
  ssh_run "cat $REMOTE_HOME/$f" > "$DEST/policy.part.$f" \
    || fail "policy file $f missing on the node host; a restore without it does not boot"
done
mkdir -p "$DEST/policy"
for f in keys reviewers protected-refs newcomer-audit.jsonl newcomer-adjudications.jsonl; do
  mv "$DEST/policy.part.$f" "$DEST/policy/$f"
done
chmod 700 "$DEST" && chmod 600 "$DEST/policy/"*

# 7. An assertion about the backup itself, not about this script: if a
#    secret is sitting in the destination, say so loudly, whoever put it
#    there.
leaked=$(ls "$DEST" "$DEST/policy" | grep -E '^(auth)$|\.key$|\.pem$' || true)
[ -z "$leaked" ] || fail "SECRETS IN THE BACKUP: $leaked (a backup holding a token or key is a credential channel)"

echo "pull-backup: $lines ops, fingerprint, 5 policy files -> $DEST"
