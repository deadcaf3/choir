#!/bin/zsh
# D20 dogfood: push the local repo to the canonical Forgejo mirror.
# Forgejo binds to localhost on the mirror VM (nothing internet-exposed), so
# the flow is: get the commits onto the VM, then push box-locally with the
# on-box token (the token never leaves the VM). Run after meaningful commits.
# The VM's IP is passed as $1 or read from ~/.choir-mirror-ip (not in-repo).
#
# Note the shebang is not what runs this: choirctl invokes it as
# `sh push_mirror.sh`, and /bin/sh here is bash 3.2. Nothing below may
# use a zsh-only builtin — see the clock and the arithmetic.
set -euo pipefail

IP=${1:-$(cat ~/.choir-mirror-ip)}
KEY=~/.ssh/choir_bench_ed25519
REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"

# Direction guard, added with the D20 host move. This script's whole
# topology is "the node lives here, the VM holds the backup". Run after
# the move, its oplog leg would overwrite the historical backup with
# this machine's frozen pre-flip log — a stale copy that still checksums
# clean. The follower is fed on-box now (choirctl mirror), and the
# backup direction is pull_backup.sh.
if [ -f "$HOME/.choir/node-remote" ]; then
  echo "mirror: ~/.choir/node-remote exists — this machine no longer hosts the node." >&2
  echo "        use: ./choirctl sync   (follower + backup, new topology)" >&2
  exit 1
fi

# This script talks to the VM twice, and a fresh handshake to us-east1
# measured 3.78s of which only 0.28s is the 275ms RTT: the rest is KEX
# plus auth latency. Multiplexing the second trip onto the first makes
# it 0.55s. IdentitiesOnly stops ssh offering the agent's key first and
# having it refused, which is one more round trip.
#
# ControlPersist outlives this script deliberately. 60s was too short to
# ever hit: syncs are minutes apart, so the master had always expired.
# Ten minutes spans a working session, and a repeat sync then reports
# connect 0.0s. The socket is a control channel, not a credential.
#
# BatchMode and ConnectTimeout matter more than they look now that this
# runs detached: without them any prompt or black-holed route makes a
# backgrounded run wait forever and no receipt is ever written, which
# reads exactly like a sync that has not finished yet.
SSH_CONTROL=~/.ssh/cm-choir-mirror.sock
ssh_opts=(-i "$KEY" -o IdentitiesOnly=yes -o BatchMode=yes -o ConnectTimeout=10 \
          -o ControlMaster=auto -o ControlPath="$SSH_CONTROL" -o ControlPersist=600)

now() { perl -MTime::HiRes=time -e 'printf "%.3f\n", time'; }
t_start=$(now)

# Two syncs in quick succession would otherwise push into ~/choir-src
# concurrently. mkdir is the atomic primitive available everywhere;
# flock is not stock on macOS. Wait rather than skip, so the newest
# commit is the one that ends up mirrored.
LOCK=$HOME/.choir/mirror.lock
waited=0
until mkdir "$LOCK" 2>/dev/null; do
  waited=$((waited + 1))
  if [ "$waited" -gt 180 ]; then
    echo "mirror: gave up after 180s waiting for $LOCK (stale? rmdir it)" >&2
    exit 1
  fi
  sleep 1
done
trap 'rmdir "$LOCK" 2>/dev/null || true' EXIT INT TERM

# Open the shared connection as its own step, so the handshake shows up
# as a handshake instead of hiding inside the transfer's number.
if ! ssh "${ssh_opts[@]}" -O check "choir@$IP" 2>/dev/null; then
  ssh "${ssh_opts[@]}" -N -f "choir@$IP"
fi
t_conn=$(now)

# Git, not rsync. rsync walked 707 files and 37.5 MB to move a delta git
# already knows how to compute: 1.74s even when nothing had changed,
# because almost every one of those files is an immutable content-
# addressed object that cannot have changed. git push asks for the far
# ref, sends only the missing objects, and is atomic at the ref.
#
# It also deletes two workarounds. rsync overwrote .git/config, so the
# mirror remote had to be recreated on every single run (commit
# 4740864 exists only to patch that); and it needed a .gitignore filter
# so local-only files would not reach the VM, which is now structural —
# git pushes committed objects and nothing else.
#
# The receiving repo has a checked-out branch and 53 dirty files, so
# denyCurrentBranch must be `ignore` rather than `updateInstead`, which
# would refuse. Only refs matter there: the daily bundle cron does
# `git fetch mirror && git bundle create --all`, which reads refs and
# objects, never the working tree. That tree is stale by design now.
export GIT_SSH_COMMAND="ssh ${ssh_opts[*]}"
if ! git push -q "choir@$IP:choir-src" main --tags 2>/dev/null; then
  ssh "${ssh_opts[@]}" "choir@$IP" 'git -C ~/choir-src config receive.denyCurrentBranch ignore'
  git push -q "choir@$IP:choir-src" main --tags
fi
t_xfer=$(now)

# The mirror remote now survives between runs, so this only adds it when
# it is genuinely missing instead of recreating it every time.
ssh "${ssh_opts[@]}" "choir@$IP" 'cd ~/choir-src \
  && { git remote get-url mirror >/dev/null 2>&1 \
       || git remote add mirror "http://choir:$(cat ~/.forgejo-token)@127.0.0.1:3000/choir/choir.git"; } \
  && git push -q mirror main --tags \
  && git log --oneline -1'
t_push=$(now)

# The op log is the one piece of state that exists in exactly one place.
# Bundles carry commits; ops.jsonl is not a git object and no bundle has
# ever contained it. It is append-only and hash-chained, so a byte-
# identical copy is a verifiable backup rather than a hopeful one.
#
# What gets backed up is (ops.jsonl, node.fingerprint) and deliberately
# not the node's signing key. That key stays on the node that owns it: a
# backup carrying it would let whoever holds the backup keep signing as
# this node. Shipping the fingerprint alongside the log is what makes a
# restore onto a fresh host refuse to start rather than silently append
# under a new identity — see the pin check in choir-node/src/main.rs.
#
# Written to .part and renamed, so an interrupted transfer leaves the
# previous good backup in place instead of a truncated log that still
# looks like a log.
NODE_STATE=${CHOIR_NODE_STATE:-$HOME/.choir/repos/.choir}
oplog_lines=0
if [ -f "$NODE_STATE/ops.jsonl" ]; then
  local_sum=$(shasum -a 256 < "$NODE_STATE/ops.jsonl" | awk '{print $1}')
  remote_sum=$(ssh "${ssh_opts[@]}" "choir@$IP" \
    'mkdir -p ~/choir-oplog && cat > ~/choir-oplog/ops.jsonl.part \
     && mv ~/choir-oplog/ops.jsonl.part ~/choir-oplog/ops.jsonl \
     && sha256sum < ~/choir-oplog/ops.jsonl | cut -d" " -f1' \
    < "$NODE_STATE/ops.jsonl")
  if [ "$local_sum" != "$remote_sum" ]; then
    echo "mirror: OP LOG BACKUP MISMATCH local=$local_sum remote=$remote_sum" >&2
    exit 1
  fi
  if [ -f "$NODE_STATE/node.fingerprint" ]; then
    ssh "${ssh_opts[@]}" "choir@$IP" 'cat > ~/choir-oplog/node.fingerprint' \
      < "$NODE_STATE/node.fingerprint"
  fi
  oplog_lines=$(wc -l < "$NODE_STATE/ops.jsonl" | tr -d ' ')
fi

# Rehearsing the restore proved the log is not the whole node. A node
# rebuilt from ops.jsonl alone refused to boot — "review assignment
# policy needs --reviewers-file" — and once booted its D23/D24 metrics
# read `configured: false` until the audit files were present too.
# These are policy rather than sequenced facts, so they live outside the
# log by design, and they were single-copy in exactly the way the log
# had been. A backup that restores a ledger onto a node that will not
# start is not a backup.
#
# The exclusions carry the same weight as in the leg above: `auth` holds
# a bearer token, and the *.key and *.pem files hold private keys.
# Neither travels. What ships is public keys and policy — what a restore
# needs, and what an attacker gains nothing from holding.
#
# One tar over the existing connection rather than a round trip per
# file, extracted into .part and swapped in only once it is complete.
CHOIR_HOME=${CHOIR_HOME:-$HOME/.choir}
policy_files=""
for f in keys reviewers protected-refs newcomer-audit.jsonl newcomer-adjudications.jsonl review-adjudications.jsonl repos.list acl private-beta.manifest; do
  if [ -f "$CHOIR_HOME/$f" ]; then policy_files="$policy_files $f"; fi
done
policy_count=0
if [ -n "$policy_files" ]; then
  # shellcheck disable=SC2086 — the list is built above from fixed names.
  # --no-xattrs: bsdtar writes LIBARCHIVE.xattr.com.apple.provenance
  # headers that GNU tar on the VM warns about once per file. The
  # warnings are harmless and that is the problem — five lines of noise
  # per sync in the receipt is where a real tar error would hide.
  tar --no-xattrs -cf - -C "$CHOIR_HOME" $policy_files | ssh "${ssh_opts[@]}" "choir@$IP" \
    'rm -rf ~/choir-oplog/policy.part \
     && mkdir -p ~/choir-oplog/policy.part \
     && tar -xf - -C ~/choir-oplog/policy.part \
     && rm -rf ~/choir-oplog/policy \
     && mv ~/choir-oplog/policy.part ~/choir-oplog/policy'
  policy_count=$(echo $policy_files | wc -w | tr -d ' ')
fi
t_oplog=$(now)

# awk does the subtraction: $(( )) is integer-only in bash 3.2 and would
# silently truncate every stage to whole seconds, or fail outright on a
# decimal point.
awk -v a="$t_start" -v b="$t_conn" -v c="$t_xfer" -v d="$t_push" -v e="$t_oplog" -v n="$oplog_lines" -v p="$policy_count" 'BEGIN {
  printf "mirror: connect %.1fs | git push %.1fs | box-local push %.1fs | oplog %.1fs (%d ops, %d policy) | total %.1fs\n", b-a, c-b, d-c, e-d, n, p, e-a
}'
echo "mirror updated"
