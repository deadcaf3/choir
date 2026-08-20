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
#   - ~/.choir/repos/.choir/refs.snapshot  (the D25 ref attestation)
#   - the nine public-policy/configuration files a beta restore needs
#
# Never pulled, and refused if seen: auth, *.key, *.pem. The signing key
# stays on the node that owns it, and a bearer token in a backup is a
# credential-distribution channel, not an availability measure.
#
# Run as `./choirctl pull-backup`. Exits nonzero on any failure
# so it can be wired to a schedule later.
set -eu

IP=${1:-$(cat "$HOME/.choir-mirror-ip")}
KEY=$HOME/.ssh/choir_bench_ed25519
SSH_CONTROL=$HOME/.ssh/cm-choir-mirror.sock
DEST=${CHOIR_BACKUP_DIR:-$HOME/choir-oplog}
VERIFY_BIN=${CHOIR_NODE_BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/release/choir-node}
REMOTE_STATE='~/.choir/repos/.choir'
REMOTE_HOME='~/.choir'
POLICY_FILES='keys reviewers protected-refs newcomer-audit.jsonl newcomer-adjudications.jsonl review-adjudications.jsonl repos.list acl private-beta.manifest'

# -n: nothing here feeds ssh stdin, and without it an ssh inside the
# objects leg's while-read loop silently swallows the rest of
# repos.list — measured: two listed repos, one bundle, no error.
ssh_run() {
  ssh -n -i "$KEY" -o IdentitiesOnly=yes -o BatchMode=yes -o ConnectTimeout=10 \
      -o ControlMaster=auto -o ControlPath="$SSH_CONTROL" -o ControlPersist=600 \
      "choir@$IP" "$@"
}

fail() { echo "pull-backup: $1" >&2; exit 1; }

# Direction guard: pulling only makes sense when the node is remote. Run
# on the machine that hosts the live log, this would overwrite the real
# backup relation with a self-copy that verifies vacuously.
[ -f "$HOME/.choir/node-remote" ] \
  || fail "no ~/.choir/node-remote marker: this machine is not a follower of a remote node"
[ -x "$VERIFY_BIN" ] \
  || fail "no verifier at $VERIFY_BIN (build choir-node --release or set CHOIR_NODE_BIN)"

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

# 4. Full format and chain verification. This is the same verifier the
#    release daemon exposes for restore tooling: it checks the supported
#    wire version, sequence, every parent link, and every recomputed entry
#    hash. A checksum proves transport equality; it cannot prove that the
#    bytes copied are a valid log.
"$VERIFY_BIN" --verify-log "$DEST/ops.jsonl.part" \
  || fail "the pulled log failed format/sequence/parent/hash verification"
mv "$DEST/ops.jsonl.part" "$DEST/ops.jsonl"

# 5. The pin travels with the log, so a restore onto a fresh host refuses
#    to append under a new identity instead of doing it silently.
ssh_run "cat $REMOTE_STATE/node.fingerprint" > "$DEST/node.fingerprint.part" \
  && mv "$DEST/node.fingerprint.part" "$DEST/node.fingerprint" \
  || fail "could not pull node.fingerprint (the pin must travel with the log)"

# 6. Policy, by explicit name: what a restore needs to boot and nothing
#    else. Pulling a directory would inherit whatever lands there,
#    including a key someone copies in by accident; a name list cannot.
#    repos.list joined the set when the served repos moved into it: a
#    restore without it serves only the default repo, and the log's refs
#    for the others would be retracted by reconcile at first boot.
for f in $POLICY_FILES; do
  ssh_run "cat $REMOTE_HOME/$f" > "$DEST/policy.part.$f" \
    || fail "policy file $f missing on the node host; a restore without it does not boot"
done
mkdir -p "$DEST/policy"
for f in $POLICY_FILES; do
  mv "$DEST/policy.part.$f" "$DEST/policy/$f"
done
chmod 700 "$DEST" && chmod 600 "$DEST/policy/"*

# 6b. The ref attestation (D25), pulled beside the log it rides with.
#    The node rewrites this file after every ref op it admits, as the
#    canonical bytes of the latest RecordRefSnapshot in the log — the
#    fold is its adversarial-grade verifier, replaying anywhere. This
#    leg checks the operational half only: the copy arrived intact, the
#    attestation never goes backwards across pulls, and the bundles
#    below actually hold the state it attests. Absent on a node that
#    has not admitted a ref op since the attesting build: noted loudly,
#    not fatal, so the upgrade does not break the hourly pull.
SNAP=
if ssh_run "cat $REMOTE_STATE/refs.snapshot" > "$DEST/refs.snapshot.part" 2>/dev/null; then
  local_sum=$(shasum -a 256 < "$DEST/refs.snapshot.part" | awk '{print $1}')
  remote_sum=$(ssh_run "sha256sum < $REMOTE_STATE/refs.snapshot | cut -d' ' -f1")
  [ "$local_sum" = "$remote_sum" ] \
    || fail "REF ATTESTATION MISMATCH local=$local_sum remote=$remote_sum"
  new_at=$(sed -n 's/.*"at_seq":\([0-9][0-9]*\).*/\1/p' "$DEST/refs.snapshot.part")
  [ -n "$new_at" ] || fail "the pulled refs.snapshot carries no at_seq: not an attestation"
  if [ -f "$DEST/refs.snapshot" ]; then
    old_at=$(sed -n 's/.*"at_seq":\([0-9][0-9]*\).*/\1/p' "$DEST/refs.snapshot")
    [ "$new_at" -ge "${old_at:-0}" ] \
      || fail "the node's attestation is OLDER than the last pull ($new_at < $old_at): an attestation chain cannot go backwards"
  fi
  mv "$DEST/refs.snapshot.part" "$DEST/refs.snapshot"
  SNAP=$DEST/refs.snapshot
else
  rm -f "$DEST/refs.snapshot.part"
  echo "pull-backup: no refs.snapshot on the node yet (no ref op since the attesting build); this run's bundles are unverified against an attestation" >&2
fi

# 7. Git objects, one full bundle per served repo. The op log carries
#    ref *history*; the objects those refs name lived only on the node
#    host — and the on-box follower is a second copy on the same disk,
#    which is the same failure domain. The refs hash short-circuits the
#    transfer when nothing moved, so the hourly schedule does not
#    re-ship megabytes of unchanged history; the bundle is verified
#    here, against this checkout, before it replaces the previous one.
bundles=0
unverified=0
while IFS= read -r repo; do
  case $repo in ''|\#*) continue ;; esac
  bundle=$DEST/repos/$repo.bundle
  marker=$DEST/repos/$repo.refs
  mkdir -p "$(dirname "$bundle")"
  # A repository with no refs cannot be bundled: `git bundle create`
  # refuses rather than writing a zero-ref bundle, and `fail` took the
  # whole pull down with it — so one repo created and never pushed to
  # made every hourly run exit nonzero, with the log and fingerprint
  # already safely copied. A backup script that always reports failure
  # is one that stops being read, which is the actual damage.
  #
  # Nothing to back up is not a backup failure. Said out loud rather
  # than skipped silently, because "no refs" and "we forgot to pull it"
  # must not look the same in the output.
  count=$(ssh_run "git --git-dir ~/.choir/repos/$repo for-each-ref | wc -l" | tr -d ' ') \
    || fail "could not read the refs of $repo on the node host"
  if [ "$count" = "0" ]; then
    echo "pull-backup: $repo holds no refs yet; nothing to bundle"
    continue
  fi
  refs=$(ssh_run "git --git-dir ~/.choir/repos/$repo for-each-ref | sha256sum | cut -d' ' -f1") \
    || fail "could not read the refs of $repo on the node host"
  if [ -f "$bundle" ] && [ -f "$marker" ] && [ "$(cat "$marker")" = "$refs" ]; then
    : # unchanged since the last pull; still verified against the attestation below
  else
    ssh_run "git --git-dir ~/.choir/repos/$repo bundle create /tmp/choir-pull.\$\$.bundle --all 2>/dev/null \
             && cat /tmp/choir-pull.\$\$.bundle && rm -f /tmp/choir-pull.\$\$.bundle" > "$bundle.part" \
      || fail "could not bundle $repo on the node host"
    git -C "$(dirname "$0")/.." bundle verify "$bundle.part" 2>/dev/null \
      | grep -q "complete history" \
      || fail "the pulled bundle for $repo does not verify as complete; keeping the previous copy"
    mv "$bundle.part" "$bundle"
    printf '%s\n' "$refs" > "$marker"
  fi
  # The D25 cross-check: the bundle's heads must be exactly what the
  # node attested, in the namespaces the pre-receive hook routes (heads
  # and tags — refs/remotes/* in a bare repo are local bookkeeping no op
  # ever attested). ContentHash serializes canonically as
  # {codec, digest bytes}, not display hex, so this projects it with
  # python3 (present on the macOS follower this runs on) rather than
  # pattern-matching the JSON; the first live run proved a sed pattern
  # written against imagined hex silently compared nothing. A push
  # landing mid-pull reads as a mismatch — a re-run comes back clean;
  # anything persistent is real divergence.
  if [ -n "$SNAP" ]; then
    python3 - "$SNAP" "$repo" > "$DEST/.snap_refs" <<'PY' \
      || fail "could not project the attestation for $repo"
import json, sys
snap = json.load(open(sys.argv[1]))
prefix = sys.argv[2] + ":"
rows = []
for name, h in snap["refs"].items():
    ref = name[len(prefix):] if name.startswith(prefix) else None
    if ref and (ref.startswith("refs/heads/") or ref.startswith("refs/tags/")):
        rows.append(bytes(h["digest"]).hex() + " " + ref)
sys.stdout.write("".join(r + "\n" for r in sorted(rows)))
PY
    git -C "$(dirname "$0")/.." bundle list-heads "$bundle" \
      | grep -E ' refs/(heads|tags)/' | LC_ALL=C sort > "$DEST/.bundle_refs"
    # The three outcomes are D47's, and they live in their own script so
    # they can be exercised with fixtures rather than only against a
    # node. A repo with no attested rows is reported and counted, never
    # failed: it cannot have any, so failing it fails every run forever.
    if sh "$(dirname "$0")/attest_check.sh" "$repo" "$DEST/.snap_refs" "$DEST/.bundle_refs"; then
      [ -s "$DEST/.snap_refs" ] || unverified=$((unverified + 1))
    else
      rm -f "$DEST/.snap_refs" "$DEST/.bundle_refs"
      fail "the bundle for $repo does not match the node's attested ref-state (re-run once; a persistent mismatch is divergence)"
    fi
    rm -f "$DEST/.snap_refs" "$DEST/.bundle_refs"
  fi
  bundles=$((bundles + 1))
done <"$DEST/policy/repos.list"
[ "$bundles" -gt 0 ] || fail "repos.list lists no repos; the objects leg backed up nothing"

# 8. An assertion about the backup itself, not about this script: if a
#    secret is sitting in the destination, say so loudly, whoever put it
#    there.
leaked=$(ls "$DEST" "$DEST/policy" | grep -E '^(auth)$|\.key$|\.pem$' || true)
[ -z "$leaked" ] || fail "SECRETS IN THE BACKUP: $leaked (a backup holding a token or key is a credential channel)"

echo "pull-backup: $lines ops, fingerprint, 9 policy/config files, $bundles repo bundles -> $DEST"
# Never let the run's last line read as full verification when it was
# not. The per-repo line above already said which; this says how many,
# because that is the number an operator would otherwise have to count
# out of a scrolled log.
if [ "$unverified" -gt 0 ]; then
  echo "pull-backup: $unverified of $bundles bundles are UNVERIFIED — copied, with no attested ref-state behind them (D47)"
fi
