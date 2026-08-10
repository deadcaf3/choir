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

# awk does the subtraction: $(( )) is integer-only in bash 3.2 and would
# silently truncate every stage to whole seconds, or fail outright on a
# decimal point.
awk -v a="$t_start" -v b="$t_conn" -v c="$t_xfer" -v d="$t_push" 'BEGIN {
  printf "mirror: connect %.1fs | git push %.1fs | box-local push %.1fs | total %.1fs\n", b-a, c-b, d-c, d-a
}'
echo "mirror updated"
