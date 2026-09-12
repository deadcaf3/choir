#!/bin/sh
# Node host -> this machine: the offsite copy of the live op log.
#
# After the D20 host move the node lives on a server and this laptop is a
# client, so the backup direction inverts: the copy has to be pulled to a
# disk that cannot be lost with the original. A live log and its only copy
# on one disk is not a backup.
#
# Runs on the operator laptop (the machine holding ~/.choir/node-remote),
# never on the node host. It refuses the other direction rather than
# writing a "backup" next to the thing it is backing up.
#
# What travels: ops.jsonl, node.fingerprint, refs.snapshot, whichever of
# the nine policy files the node has, as one tar, and one `--all` bundle
# per served repo. The log says
# which commits the refs named; only the bundles make the git objects
# restorable, and ops.jsonl is not a git object so no bundle has ever
# contained it.
#
# What must not, and is never named below: node.key, auth, and any
# *.key/*.pem. A backup carrying the signing key lets whoever holds the
# backup keep signing as this node; one carrying auth ships a bearer
# token. auth is re-issued on restore, not recovered.
#
# Self-verifying, because a silently failing backup is the failure mode
# this repository keeps re-learning. Three checks, and any one of them
# failing leaves the previous good copy in place:
#   1. checksum   — sha256 computed on the node must equal the local one
#   2. prefix     — the previous copy must be a byte prefix of the new one
#   3. contiguity — seq starts at 0 and increases by exactly 1 per line
# Check 2 is the one that catches a log that was rewritten rather than
# appended to, which no checksum of a single file can see.
set -eu

DEST=${1:-$HOME/choir-backup}
STATE=$HOME/.choir
IP_FILE=$HOME/.choir-mirror-ip
REMOTE_STATE='$HOME/.choir'

[ -f "$STATE/node-remote" ] || {
  echo "pull_backup: no $STATE/node-remote — the node is on this machine," >&2
  echo "  so pulling here would put the copy on the disk it is meant to survive" >&2
  exit 1
}
[ -f "$IP_FILE" ] || { echo "pull_backup: no $IP_FILE naming the node host" >&2; exit 1; }
IP=$(cat "$IP_FILE")
KEY=$HOME/.ssh/choir_bench_ed25519
[ -f "$KEY" ] || { echo "pull_backup: no ssh identity at $KEY" >&2; exit 1; }

node_ssh() {
  ssh -i "$KEY" -o IdentitiesOnly=yes -o BatchMode=yes -o ConnectTimeout=10 \
      -o ControlMaster=auto -o ControlPath="$HOME/.ssh/cm-choir-backup.sock" \
      -o ControlPersist=60 "choir@$IP" "$@"
}

# macOS ships shasum, Linux ships sha256sum, and this script runs on the
# laptop while the far side is the server -- so both spellings are needed.
sha256_local() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

STAMP=$(date -u +%Y%m%dT%H%M%SZ)
INCOMING="$DEST/.incoming-$STAMP"
mkdir -p "$DEST" "$INCOMING/repos"
# Everything lands in .incoming and is promoted only after every check
# passes, so a failed pull can never half-overwrite a good backup.
trap 'rm -rf "$INCOMING"' EXIT

# ---------------------------------------------------------------- the log

echo "pulling ops.jsonl"
REMOTE_SUM=$(node_ssh "sha256sum $REMOTE_STATE/repos/.choir/ops.jsonl | cut -d' ' -f1")
node_ssh "cat $REMOTE_STATE/repos/.choir/ops.jsonl" > "$INCOMING/ops.jsonl"
LOCAL_SUM=$(sha256_local "$INCOMING/ops.jsonl")

[ "$REMOTE_SUM" = "$LOCAL_SUM" ] || {
  echo "pull_backup: checksum mismatch — the transfer is torn" >&2
  echo "  node:  $REMOTE_SUM" >&2
  echo "  local: $LOCAL_SUM" >&2
  exit 1
}
echo "  checksum ok ($LOCAL_SUM)"

# The log is append-only, so the previous copy must survive byte for byte
# inside the new one. A node restored from an older log and restarted
# would produce a divergent history that still checksums fine on its own.
if [ -f "$DEST/ops.jsonl" ]; then
  PREV_BYTES=$(wc -c < "$DEST/ops.jsonl" | tr -d ' ')
  NEW_BYTES=$(wc -c < "$INCOMING/ops.jsonl" | tr -d ' ')
  if [ "$NEW_BYTES" -lt "$PREV_BYTES" ]; then
    echo "pull_backup: the node's log is SHORTER than the copy held here" >&2
    echo "  held $PREV_BYTES bytes, node has $NEW_BYTES — the log was rewritten, not appended to" >&2
    exit 1
  fi
  PREV_SUM=$(sha256_local "$DEST/ops.jsonl")
  HEAD_SUM=$(head -c "$PREV_BYTES" "$INCOMING/ops.jsonl" | { if command -v sha256sum >/dev/null 2>&1; then sha256sum; else shasum -a 256; fi } | cut -d' ' -f1)
  [ "$PREV_SUM" = "$HEAD_SUM" ] || {
    echo "pull_backup: the copy held here is NOT a prefix of the node's log" >&2
    echo "  the history diverged; keeping the existing backup and refusing this one" >&2
    exit 1
  }
  echo "  prefix ok (grew $PREV_BYTES -> $NEW_BYTES bytes)"
else
  echo "  prefix skipped (first pull, nothing to compare against)"
fi

# seq contiguity. Cheap, and it runs whether or not a choir-node binary is
# around; the chain check below is stronger but needs one.
LINES=$(wc -l < "$INCOMING/ops.jsonl" | tr -d ' ')
SEQS=$(sed -n 's/.*"seq":\([0-9]*\).*/\1/p' "$INCOMING/ops.jsonl" | wc -l | tr -d ' ')
[ "$LINES" = "$SEQS" ] || {
  echo "pull_backup: $LINES lines but $SEQS carry a seq — the log is malformed" >&2
  exit 1
}
NEXT_SEQ=$(sed -n 's/.*"seq":\([0-9]*\).*/\1/p' "$INCOMING/ops.jsonl" | awk '
  NR == 1 { if ($1 != 0) { printf "first seq is %s, not 0\n", $1 > "/dev/stderr"; exit 1 }
            prev = $1; next }
  { if ($1 != prev + 1) { printf "gap: seq %s followed by %s\n", prev, $1 > "/dev/stderr"; exit 1 }
    prev = $1 }
  END { print prev + 1 }')
echo "  contiguity ok ($LINES ops, next_seq $NEXT_SEQ)"

# The real verifier, when this checkout has one built. It refuses a bad
# format_version, a broken hash chain and a torn tail -- none of which the
# three checks above can see.
VERIFIER="$(cd "$(dirname "$0")/../.." && pwd)/target/release/choir-node"
if [ -x "$VERIFIER" ]; then
  if "$VERIFIER" --verify-log "$INCOMING/ops.jsonl" >/dev/null 2>&1; then
    echo "  chain ok (choir-node --verify-log)"
  else
    echo "pull_backup: choir-node --verify-log rejected the pulled log" >&2
    "$VERIFIER" --verify-log "$INCOMING/ops.jsonl" >&2 || true
    exit 1
  fi
else
  echo "  chain UNVERIFIED — no $VERIFIER; build it: cargo build --release -p choir-node"
fi

# ------------------------------------------------------- identity + policy

echo "pulling node.fingerprint"
node_ssh "cat $REMOTE_STATE/repos/.choir/node.fingerprint" > "$INCOMING/node.fingerprint"

# The D25 ref attestation: what the node signed for its own ref-state, as
# against what a restore happens to replay into. It is the only check a
# restore can make that a checksum cannot reach -- bytes can arrive
# perfectly and still fold into a different view -- and
# restore_from_backup.sh skips that check entirely when the backup has no
# snapshot in it, which is every backup this leg wrote before now.
#
# Absent is not a failure: a node that has never moved a ref has never
# attested one. It is said out loud instead, because a silently skipped
# proof reads exactly like a passed one.
echo "pulling refs.snapshot"
if node_ssh "test -f $REMOTE_STATE/repos/.choir/refs.snapshot"; then
  node_ssh "cat $REMOTE_STATE/repos/.choir/refs.snapshot" > "$INCOMING/refs.snapshot"
  [ -s "$INCOMING/refs.snapshot" ] || {
    echo "pull_backup: refs.snapshot is on the node but arrived empty" >&2
    exit 1
  }
  echo "  attestation ok ($(wc -c < "$INCOMING/refs.snapshot" | tr -d ' ') bytes)"
else
  echo "  no attestation on the node — a restore from this backup cannot check the view it replays into"
fi

# The node's policy is configuration, not sequenced fact, so it lives
# outside the log -- and a node rebuilt from ops.jsonl alone refuses to
# boot for want of it. Named one by one rather than globbed: a glob over
# ~/.choir would sweep in auth and node.key.
#
# All nine, not the six this leg carried for its whole life. The three it
# left behind -- review-adjudications.jsonl, acl, private-beta.manifest --
# are the ones that decide whether landings are adjudicated, who owns
# which repository, and which limits the beta runs under. A node restored
# without them boots and serves and enforces less than the node it
# replaces, which is the failure worth catching here rather than later.
#
# tar is given every name and its complaints are dropped: a node that
# protects no ref has no protected-refs file, and that is a fact about
# the node rather than an error. Which names actually arrived is read
# back off the finished archive below, so the count can never be a claim
# about what was asked for.
POLICY_NAMES='keys reviewers protected-refs newcomer-audit.jsonl
newcomer-adjudications.jsonl review-adjudications.jsonl acl
private-beta.manifest repos.list'
echo "pulling policy"
node_ssh "cd $REMOTE_STATE && tar cf - $(echo $POLICY_NAMES) 2>/dev/null || true" \
  > "$INCOMING/policy.tar"

[ -s "$INCOMING/policy.tar" ] || {
  echo "pull_backup: policy tar is empty — the node has no policy files to ship" >&2
  exit 1
}

MEMBERS=$(tar tf "$INCOMING/policy.tar")
for f in $POLICY_NAMES; do
  printf '%s\n' "$MEMBERS" | grep -qx "$f" \
    || echo "  no $f on the node — a restore from this backup starts without it"
done

# Belt and braces. The tar above names nine files and none of them are
# secrets, but a backup that quietly grew a key is worth failing loudly
# over rather than trusting the line that built it.
if tar tf "$INCOMING/policy.tar" | grep -Eq '(^|/)(auth|node\.key)$|\.(key|pem)$'; then
  echo "pull_backup: REFUSING — the policy tar contains a credential" >&2
  tar tf "$INCOMING/policy.tar" | grep -E '(^|/)(auth|node\.key)$|\.(key|pem)$' >&2
  exit 1
fi
echo "  policy ok ($(tar tf "$INCOMING/policy.tar" | wc -l | tr -d ' ') files, no credentials)"

# ------------------------------------------------------------- the objects

# One --all bundle per served repo. Re-pulled only when the refs hash
# moves, because a bundle of an unchanged repo is the same bytes and this
# runs on an interval.
REPOS=$(node_ssh "grep -v '^#' $REMOTE_STATE/repos.list | grep -v '^\$' || true")
for repo in $REPOS; do
  name=$(basename "$repo" .git)
  refs_hash=$(node_ssh "git -C $REMOTE_STATE/repos/$repo show-ref 2>/dev/null | sha256sum | cut -d' ' -f1")
  held=""
  [ -f "$DEST/repos/$name.refs" ] && held=$(cat "$DEST/repos/$name.refs")
  if [ "$refs_hash" = "$held" ] && [ -f "$DEST/repos/$name.bundle" ]; then
    echo "bundle $name: unchanged, kept"
    cp "$DEST/repos/$name.bundle" "$INCOMING/repos/$name.bundle"
    printf '%s\n' "$refs_hash" > "$INCOMING/repos/$name.refs"
    continue
  fi
  echo "pulling bundle $name"
  node_ssh "git -C $REMOTE_STATE/repos/$repo bundle create - --all 2>/dev/null" \
    > "$INCOMING/repos/$name.bundle"
  git bundle verify "$INCOMING/repos/$name.bundle" >/dev/null 2>&1 || {
    echo "pull_backup: bundle for $name did not verify" >&2
    exit 1
  }
  printf '%s\n' "$refs_hash" > "$INCOMING/repos/$name.refs"
  echo "  bundle ok ($(wc -c < "$INCOMING/repos/$name.bundle" | tr -d ' ') bytes)"
done

# ---------------------------------------------------------------- promote

cat > "$INCOMING/manifest" <<MANIFEST
format_version 1
pulled_at $STAMP
ops_sha256 $LOCAL_SUM
ops_bytes $(wc -c < "$INCOMING/ops.jsonl" | tr -d ' ')
next_seq $NEXT_SEQ
repos $(echo "$REPOS" | wc -w | tr -d ' ')
MANIFEST

# Promote by moving each verified file over the live copy. The previous
# copy stays readable until this point, which is what makes a failed pull
# harmless.
mv "$INCOMING/ops.jsonl"        "$DEST/ops.jsonl"
mv "$INCOMING/node.fingerprint" "$DEST/node.fingerprint"
# A snapshot from an earlier run is removed rather than kept beside a
# newer log. The restore compares the attestation against the view the
# whole log replays into, so a stale one describes a ref-state the log
# has since moved past and would fail a restore that is perfectly good.
if [ -f "$INCOMING/refs.snapshot" ]; then
  mv "$INCOMING/refs.snapshot" "$DEST/refs.snapshot"
else
  rm -f "$DEST/refs.snapshot"
fi
mv "$INCOMING/policy.tar"       "$DEST/policy.tar"
mv "$INCOMING/manifest"         "$DEST/manifest"
mkdir -p "$DEST/repos"
for f in "$INCOMING"/repos/*; do
  [ -e "$f" ] || continue
  mv "$f" "$DEST/repos/$(basename "$f")"
done
chmod 700 "$DEST"

echo "backup: $DEST — $LINES ops, next_seq $NEXT_SEQ, verified $STAMP"
echo "restorable? run: choir backup verify $DEST"
