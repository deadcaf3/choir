#!/bin/sh
# Turn an offsite backup back into a node, and refuse to say it worked
# until the restored node has actually accepted a write.
#
# The other half of scripts/pull_backup.sh. That script proves a copy
# arrived; this one proves the copy is a node. A backup nobody has
# restored is a hypothesis, and the only thing that settles it is a
# running daemon appending to the log it was handed.
#
#   ./choirctl restore-from-backup <backup-dir> <target-root>
#
# Exits: 0 restored and proven, 1 a check failed, 2 usage,
#        3 an operator decision is required (secrets, see the runbook).
#
# Nothing here mints a secret. Backups exclude them by design, so a
# restore has a hole in it that only a human can fill: the daemon's
# signing key, the auth tokens, and any TLS material. This script stops
# and names them rather than inventing replacements, because a minted
# node key is a *new node* wearing the old node's log.
#
# Full procedure, including what to do at each refusal:
# docs/runbook-restore.md
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
SRC=${1:-}
ROOT=${2:-}
NODE_BIN=${CHOIR_NODE_BIN:-$HERE/../target/release/choir-node}
POLICY_FILES="keys reviewers protected-refs newcomer-audit.jsonl newcomer-adjudications.jsonl review-adjudications.jsonl repos.list acl private-beta.manifest"

fail() { echo "restore: $1" >&2; exit 1; }
decide() { echo "restore: $1" >&2; exit 3; }

[ -n "$SRC" ] && [ -n "$ROOT" ] || {
  echo "usage: ./choirctl restore-from-backup <backup-dir> <target-root>" >&2
  echo "  <backup-dir>   a directory written by pull_backup.sh" >&2
  echo "  <target-root>  the repo root to build; must not already hold a log" >&2
  exit 2
}
[ -d "$SRC" ] || fail "no backup directory at $SRC"
[ -x "$NODE_BIN" ] || fail "no daemon at $NODE_BIN — build it: cargo build --release -p choir-node (or set CHOIR_NODE_BIN)"

WORK=$(mktemp -d)
NODE_PID=
cleanup() {
  [ -n "$NODE_PID" ] && kill "$NODE_PID" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT INT TERM

# ---------------------------------------------------------------- 1. read
# Everything checkable about the backup, before a single byte is written
# to the target. A restore that fails halfway has already destroyed the
# thing an operator would fall back to, so the order is: read, refuse,
# then write.

[ -f "$SRC/ops.jsonl" ] || fail "no ops.jsonl in $SRC: that is not a backup"
lines=$(wc -l < "$SRC/ops.jsonl" | tr -d ' ')
[ "$lines" -gt 0 ] || fail "the backed-up log is empty"

# Verify before writing a byte into the target. This rejects unsupported
# formats, sequence gaps, broken parent links, recomputed-hash mismatches,
# and incomplete tails with the same code shipped in the daemon artifact.
"$NODE_BIN" --verify-log "$SRC/ops.jsonl" \
  || fail "the backup failed format/sequence/parent/hash verification"

missing=
for f in $POLICY_FILES; do
  [ -f "$SRC/policy/$f" ] || missing="$missing $f"
done
[ -z "$missing" ] || fail "policy files missing from the backup:$missing (a node restored without reviewers does not boot)"

# The same assertion pull_backup.sh makes about its own output, made
# again here about its input. It holds whoever put the file there.
leaked=$(ls "$SRC" "$SRC/policy" | grep -E '^(auth)$|\.key$|\.pem$' || true)
[ -z "$leaked" ] || fail "SECRETS IN THE BACKUP: $leaked (a backup holding a token or key is a credential channel, and this restore will not spread it)"

repos=$(grep -v '^[[:space:]]*#' "$SRC/policy/repos.list" | grep -v '^[[:space:]]*$' || true)
[ -n "$repos" ] || fail "repos.list names no repositories: there is nothing to serve"
for repo in $repos; do
  [ -f "$SRC/repos/$repo.bundle" ] \
    || fail "no bundle for $repo: the log's refs for it name commits nothing here holds"
done

# ------------------------------------------------------------- 2. refuse
# Never write over a log. If the target already holds one, the operator
# decides which of the two is real -- this script has no basis for that
# and would be destroying the evidence needed to decide.
if [ -e "$ROOT/.choir/ops.jsonl" ]; then
  fail "$ROOT/.choir/ops.jsonl already exists: restore into an empty root, or move the existing log aside first (it is never overwritten here)"
fi
for repo in $repos; do
  if [ -e "$ROOT/$repo" ]; then
    fail "$ROOT/$repo already exists: restore into an empty root"
  fi
done

# -------------------------------------------------------------- 3. place
mkdir -p "$ROOT/.choir/policy"
cp "$SRC/ops.jsonl" "$ROOT/.choir/ops.jsonl"
if [ -f "$SRC/node.fingerprint" ]; then
  cp "$SRC/node.fingerprint" "$ROOT/.choir/node.fingerprint"
fi
if [ -f "$SRC/refs.snapshot" ]; then
  cp "$SRC/refs.snapshot" "$ROOT/.choir/refs.snapshot"
fi
for f in $POLICY_FILES; do cp "$SRC/policy/$f" "$ROOT/.choir/policy/$f"; done
chmod 700 "$ROOT/.choir" && chmod 600 "$ROOT/.choir/policy/"*

# Objects before the first boot, never after. Startup reconciliation
# reads the log against what git holds, and a ref naming a commit the
# repo does not have is classed as unbackable -- so it appends a
# retraction and the log now agrees with the emptiness. Restoring into
# repos the daemon created for itself would therefore erase, in signed
# ops, exactly the ref state being restored.
for repo in $repos; do
  mkdir -p "$(dirname "$ROOT/$repo")"
  git clone --bare --quiet "$SRC/repos/$repo.bundle" "$ROOT/$repo" \
    || fail "could not unbundle $repo"
  # A bundle clone leaves an `origin` pointing at the bundle file, which
  # would make the restored repo fetch from a path that is about to be a
  # temp directory on someone's laptop.
  git --git-dir "$ROOT/$repo" remote remove origin 2>/dev/null || true
done

# --------------------------------------------------------- 4. the secrets
# Two holes the backup deliberately does not fill. Both are refusals
# rather than warnings: the rehearsal below cannot run without them, and
# a restore that has not been rehearsed has not been done.
if [ ! -f "$ROOT/.choir/node.key" ]; then
  if [ -f "$ROOT/.choir/node.fingerprint" ]; then
    decide "the node's signing key is not here, and $ROOT/.choir/node.fingerprint pins the identity that wrote this log.
  Files are in place; the daemon will refuse to start until you choose:
    (a) put the original 32-byte key at $ROOT/.choir/node.key (chmod 600) and re-run this, or
    (b) accept that the log changes author at this point:
          rm $ROOT/.choir/node.fingerprint
        and re-run. Every op after the seam is signed by a different actor.
  Option (b) is not reversible and not invisible: see docs/runbook-restore.md."
  fi
  decide "no node key at $ROOT/.choir/node.key. The daemon will mint one on first start, which is correct only if this log has no earlier author."
fi
AUTH=${CHOIR_RESTORE_AUTH:-$ROOT/.choir/auth}
[ -f "$AUTH" ] || decide "no auth file at $AUTH. Backups carry no credentials, so mint one now:
    printf '<operator>:%s\n' \"\$(openssl rand -hex 32)\" > $AUTH && chmod 600 $AUTH
  Replace <operator> with a username that the restored ACL grants ownership of a restored repository, then re-run. Reuse of the old token is not possible and not wanted: it was last seen on a host you are restoring away from."

# ----------------------------------------------------- 5. rehearsal boot
# Port 0: the daemon picks a free one and prints it, so a rehearsal never
# collides with the node it is rehearsing to replace.
create_args=""
for repo in $repos; do create_args="$create_args --create $repo"; done
# shellcheck disable=SC2086
"$NODE_BIN" "$ROOT" 0 --bind 127.0.0.1 \
  --auth-file "$AUTH" \
  --keys-file "$ROOT/.choir/policy/keys" \
  --reviewers-file "$ROOT/.choir/policy/reviewers" \
  --protected-refs "$ROOT/.choir/policy/protected-refs" \
  --newcomer-audit "$ROOT/.choir/policy/newcomer-audit.jsonl" \
  --newcomer-adjudications "$ROOT/.choir/policy/newcomer-adjudications.jsonl" \
  --review-adjudications "$ROOT/.choir/policy/review-adjudications.jsonl" \
  --acl-file "$ROOT/.choir/policy/acl" \
  --require-assignment \
  --protected-refs "$ROOT/.choir/policy/protected-refs" \
  --require-review \
  --require-scope \
  --read-only-browser \
  --journal "$ROOT/.choir/journal.jsonl" \
  --request-log "$ROOT/.choir/requests.jsonl" \
  --request-log-max-bytes 33554432 \
  --rate-limit-api 120 \
  --rate-limit-git 60 \
  --quota-push-bytes 536870912 \
  --quota-workspaces 8 \
  --api-body-limit 1048576 \
  --batch-limit 256 \
  --ready-min-free-bytes 1073741824 \
  $create_args > "$WORK/node.out" 2> "$WORK/node.err" &
NODE_PID=$!

# Wait for the daemon's own marker, not for a duration: a slow machine
# is not a failed restore, and a dead process is not a slow one.
PORT=
i=0
while [ "$i" -lt 600 ]; do
  PORT=$(sed -n 's/.*choir-node serving .* on http:\/\/[^:]*:\([0-9][0-9]*\).*/\1/p' "$WORK/node.err" | head -1)
  if [ -n "$PORT" ]; then break; fi
  kill -0 "$NODE_PID" 2>/dev/null || break
  sleep 0.1
  i=$((i + 1))
done
[ -n "$PORT" ] || fail "the restored node did not start. Its output:
$(cat "$WORK/node.err")"

# A retraction during a restore is the failure this whole ordering
# exists to prevent, and it is loud rather than fatal-by-accident: the
# log has already been appended to by the time it is printed.
if grep -q '^choir: retracted' "$WORK/node.err"; then
  fail "the restored node RETRACTED refs on start:
$(grep '^choir: retracted' "$WORK/node.err")
  The log named commits the repos do not hold. The restored log now has
  compensating ops in it and is no longer the backup. Start again from
  the backup into a clean root."
fi
grep '^choir: UNRECONCILED' "$WORK/node.err" >&2 || true

USER_TOKEN=$(head -1 "$AUTH")
API=http://127.0.0.1:$PORT
view() { curl -s -u "$USER_TOKEN" "$API/api/view"; }

# --------------------------------------------------- 6. head-hash match
# What the node replayed to, before anything is written. The claim to
# check is that the daemon folded *these* bytes and stopped where they
# stop -- and the check for it is in step 7, where the canary's own
# `parent` has to be this hash. An entry naming it as its parent is the
# log itself agreeing, rather than the node being asked to confirm its
# own arithmetic.
HEAD_BEFORE=$(view | python3 -c 'import json,sys; print(json.load(sys.stdin)["log"]["head"])' 2>/dev/null || true)
[ -n "$HEAD_BEFORE" ] && [ "$HEAD_BEFORE" != None ] \
  || fail "the restored node serves no log head; it did not replay the log it was given"

# The D25 attestation, when the backup carried one: the refs the node now
# serves must be the refs the old node signed for. This is the half of a
# restore that a checksum cannot reach -- the bytes can arrive perfectly
# and still be replayed into a different view.
if [ -f "$ROOT/.choir/refs.snapshot" ]; then
  view > "$WORK/view.json"
  python3 - "$ROOT/.choir/refs.snapshot" "$WORK/view.json" <<'PY' || fail "the restored view does not match the backup's ref attestation"
import json, sys
snap = json.load(open(sys.argv[1]))
view = json.load(open(sys.argv[2]))
# The attestation holds canonical ContentHash values ({codec, digest});
# the served view holds their display form. Project the first into the
# second rather than the reverse -- ContentHash::to_hex is "%02x-" then
# the digest, and a comparison written against imagined hex compares
# nothing at all, which is how the pull_backup.sh cross-check first
# passed vacuously.
want = {k: "%02x-" % v["codec"] + bytes(v["digest"]).hex() for k, v in snap["refs"].items()}
have = view.get("refs", {})
if want != have:
    for name in sorted(set(want) | set(have)):
        if want.get(name) != have.get(name):
            print(f"  {name}: attested {want.get(name)}, serving {have.get(name)}", file=sys.stderr)
    sys.exit(1)
print(f"restore: view matches the attestation at seq {snap['at_seq']} ({len(want)} refs)")
PY
fi

# ------------------------------------------------------------ 7. canary
# A restored node that cannot be written to is a museum. This is a real
# push over the real transport: http-backend, the pre-receive hook, the
# sequencer, and an append to the log that was just restored. Nothing
# short of that distinguishes a node from a directory of files.
CANARY=refs/heads/restore-canary-$(date +%s)
pushed=
for repo in $repos; do
  url="http://$USER_TOKEN@127.0.0.1:$PORT/$repo"
  rm -rf "$WORK/canary"
  git clone --quiet "$url" "$WORK/canary" 2>/dev/null || continue
  # An empty repo clones fine and has nothing to push. That is not a
  # failure of this repo, only a reason to try the next one.
  git -C "$WORK/canary" rev-parse HEAD >/dev/null 2>&1 || continue
  git -C "$WORK/canary" push --quiet "$url" "HEAD:$CANARY" \
    || fail "the canary push to $repo was refused. The restored node serves, but it does not accept writes."
  pushed=$repo
  break
done
[ -n "$pushed" ] \
  || fail "no restored repo has a commit to push, so nothing proved the node accepts writes. This is not a pass."

# The append happened, and it happened on top of what the node replayed.
# `parent` is the hash of the entry before it, so an entry whose parent
# is the head read in step 6 is the log's own statement that the restored
# bytes were folded to exactly that point.
new_lines=$(wc -l < "$ROOT/.choir/ops.jsonl" | tr -d ' ')
[ "$new_lines" -gt "$lines" ] \
  || fail "the canary push returned success but the log did not grow. The repo is being served without its pre-receive hook, so pushes bypass the sequencer."
canary_parent=$(sed -n "$((lines + 1))p" "$ROOT/.choir/ops.jsonl" \
  | python3 -c 'import json,sys; p=json.load(sys.stdin)["parent"]; print("%02x-" % p["codec"] + bytes(p["digest"]).hex())' 2>/dev/null || true)
[ "$canary_parent" = "$HEAD_BEFORE" ] \
  || fail "the first appended entry chains onto $canary_parent, but the node served head $HEAD_BEFORE: the replay and the log do not agree"

# ------------------------------------------------------------ 8. prefix
# Append-only, asserted rather than assumed: everything restored is still
# byte-for-byte where it was, with the canary after it. A restore that
# rewrote history would pass every other check in this file.
backup_bytes=$(wc -c < "$SRC/ops.jsonl" | tr -d ' ')
head -c "$backup_bytes" "$ROOT/.choir/ops.jsonl" | cmp -s - "$SRC/ops.jsonl" \
  || fail "the restored log is not a byte-exact continuation of the backup: something rewrote history"

kill "$NODE_PID" 2>/dev/null || true
wait "$NODE_PID" 2>/dev/null || true
NODE_PID=

cat <<DONE
restore: $lines ops replayed, $(echo "$repos" | wc -w | tr -d ' ') repos unbundled, canary landed at seq $lines
restore: the canary ref is $CANARY in $pushed — it is evidence, delete it when you no longer want it
restore: root is $ROOT — start it under your supervisor with your own flags (docs/runbook-restore.md)
DONE
