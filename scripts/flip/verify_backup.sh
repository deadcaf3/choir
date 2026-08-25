#!/bin/sh
# Is the backup restorable? The pull leg reports that it wrote one; this
# is the only thing that reports it could be used.
#
# Deliberately makes no ssh connection and reads nothing from the node.
# A verifier that phones the original home cannot tell you the copy is
# self-sufficient, which is the single question worth asking of a backup.
#
# Usage: sh verify_backup.sh [backup-dir]
set -eu

DEST=${1:-$HOME/choir-backup}
FAIL=0

fail() { echo "  FAIL: $*" >&2; FAIL=1; }
ok()   { echo "  ok: $*"; }

[ -d "$DEST" ] || { echo "verify_backup: no backup directory at $DEST" >&2; exit 1; }
echo "verifying $DEST"

# 1. Everything a restore needs is present. Named individually because
#    "the directory exists" is how a backup of nothing passes a check.
for f in ops.jsonl node.fingerprint policy.tar manifest; do
  if [ -s "$DEST/$f" ]; then ok "$f present"; else fail "$f missing or empty"; fi
done

# 2. The log still matches what the pull recorded. Catches bit rot and an
#    edit made to the copy after the fact, neither of which changes a
#    file's presence.
if [ -f "$DEST/manifest" ] && [ -f "$DEST/ops.jsonl" ]; then
  RECORDED=$(sed -n 's/^ops_sha256 //p' "$DEST/manifest")
  if command -v sha256sum >/dev/null 2>&1; then
    ACTUAL=$(sha256sum "$DEST/ops.jsonl" | cut -d' ' -f1)
  else
    ACTUAL=$(shasum -a 256 "$DEST/ops.jsonl" | cut -d' ' -f1)
  fi
  if [ "$RECORDED" = "$ACTUAL" ]; then
    ok "ops.jsonl matches the manifest checksum"
  else
    fail "ops.jsonl has changed since it was pulled"
    echo "    manifest: $RECORDED" >&2
    echo "    actual:   $ACTUAL" >&2
  fi
fi

# 3. The chain itself, when a binary is around to check it. This is the
#    one that refuses a bad format_version, a broken hash chain or a torn
#    tail; the checksum above only proves the bytes are the pulled bytes,
#    not that those bytes were ever a valid log.
VERIFIER="$(cd "$(dirname "$0")/../.." && pwd)/target/release/choir-node"
if [ -x "$VERIFIER" ]; then
  if "$VERIFIER" --verify-log "$DEST/ops.jsonl" >/dev/null 2>&1; then
    ok "hash chain verifies (choir-node --verify-log)"
  else
    fail "choir-node --verify-log rejected this log"
    "$VERIFIER" --verify-log "$DEST/ops.jsonl" >&2 || true
  fi
else
  echo "  UNVERIFIED: no $VERIFIER — build it: cargo build --release -p choir-node"
fi

# 3b. The D25 attestation. Not required — a node that never moved a ref
#     never signed one — but its absence removes a check from the restore
#     rather than failing one, and that is worth saying where an operator
#     reads whether the backup is good.
if [ -s "$DEST/refs.snapshot" ]; then
  ok "ref attestation present (the restore will check the view it replays into)"
else
  echo "  no ref attestation — a restore from this backup cannot check the view it replays into"
fi

# 4. The policy files, without which a restored node refuses to boot
#    rather than starting degraded. Their absence is the rehearsal's
#    finding, so it is checked by name.
if [ -s "$DEST/policy.tar" ]; then
  MEMBERS=$(tar tf "$DEST/policy.tar" 2>/dev/null || true)
  for p in keys reviewers repos.list; do
    if printf '%s\n' "$MEMBERS" | grep -qx "$p"; then
      ok "policy: $p"
    else
      fail "policy: $p absent — a node restored from this refuses to boot"
    fi
  done
  # The other six. Absent, they do not stop a restore: they make the
  # restored node enforce less than the one it replaces — no protected
  # ref, no review gate, no ownership, no newcomer audit — and that is a
  # thing to learn here rather than from a node that is quietly open.
  for p in protected-refs newcomer-audit.jsonl newcomer-adjudications.jsonl \
           review-adjudications.jsonl acl private-beta.manifest; do
    if printf '%s\n' "$MEMBERS" | grep -qx "$p"; then
      ok "policy: $p"
    else
      echo "  no $p — a node restored from this starts without it and enforces less"
    fi
  done
  # The other direction: a backup that gained a credential is worse than
  # one missing a policy file, so it fails rather than warns.
  if printf '%s\n' "$MEMBERS" | grep -Eq '(^|/)(auth|node\.key)$|\.(key|pem)$'; then
    fail "policy tar CONTAINS A CREDENTIAL — this backup must not be kept"
    printf '%s\n' "$MEMBERS" | grep -E '(^|/)(auth|node\.key)$|\.(key|pem)$' >&2
  else
    ok "no credentials in the backup"
  fi
fi

# 5. The git objects. The log names commits; only a bundle holds them.
BUNDLES=0
for b in "$DEST"/repos/*.bundle; do
  [ -e "$b" ] || continue
  BUNDLES=$((BUNDLES + 1))
  if git bundle verify "$b" >/dev/null 2>&1; then
    ok "bundle $(basename "$b") verifies"
  else
    fail "bundle $(basename "$b") is corrupt"
  fi
done
[ "$BUNDLES" -gt 0 ] || fail "no repo bundles — the refs in the log name objects nothing here holds"

# 6. Age. A backup that stopped running looks identical to one that ran a
#    minute ago until somebody reads the date.
if [ -f "$DEST/manifest" ]; then
  echo "  pulled: $(sed -n 's/^pulled_at //p' "$DEST/manifest") ($(sed -n 's/^next_seq //p' "$DEST/manifest") ops)"
fi

if [ "$FAIL" -eq 0 ]; then
  echo "backup at $DEST is restorable"
  echo "restore with: ./choirctl restore-from-backup $DEST <target-root>"
  exit 0
fi
echo "backup at $DEST is NOT restorable — see the failures above" >&2
exit 1
