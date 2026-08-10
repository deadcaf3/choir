#!/bin/zsh
# D20 dogfood: push the local repo to the canonical Forgejo mirror.
# Forgejo binds to localhost on the mirror VM (nothing internet-exposed), so
# the flow is: rsync the repo to the VM, then push box-locally with the
# on-box token (secrets never leave the VM). Run after meaningful commits.
# The VM's IP is passed as $1 or read from ~/.choir-mirror-ip (not in-repo).
set -euo pipefail

IP=${1:-$(cat ~/.choir-mirror-ip)}
KEY=~/.ssh/choir_bench_ed25519
REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"

# This script talks to the VM twice — rsync, then the box-local push —
# and a fresh handshake to us-east1 measured 3.78s each, of which only
# 0.28s is the 275ms RTT: the rest is KEX plus auth latency on an
# e2-micro absorbing a few thousand failed logins a day. Multiplexing
# the second trip onto the first connection makes it 0.57s. Measured on
# this laptop, so treat as directional per PHASE0's standing disclaimer.
#
# IdentitiesOnly is the cheaper half: without it ssh offers the agent's
# key first, the server refuses it, and that is one wasted round trip
# before the explicit key is even tried.
#
# ControlPersist outlives this script deliberately. 60s was too short to
# help: syncs are minutes apart, so the master had always expired and
# every sync still paid a full handshake. Ten minutes spans a working
# session, which is the interval that actually repeats. The socket is a
# control channel, not a credential; it lives under ~/.ssh with the key.
SSH_CONTROL=~/.ssh/cm-choir-mirror.sock
ssh_opts=(-i "$KEY" -o IdentitiesOnly=yes -o ControlMaster=auto \
          -o ControlPath="$SSH_CONTROL" -o ControlPersist=600)

# Every stage is timed and reported. This script was once "fixed" for
# speed against a model of where its time went rather than a
# measurement, and the model was wrong; the fix helped and the sync was
# still slow, because the estimate had never been checked end to end.
# A run that reports its own three numbers cannot be argued with.
zmodload zsh/datetime
t_start=$EPOCHREALTIME

# Open the shared connection as its own step, so the handshake shows up
# as a handshake instead of hiding inside rsync's number. `-O check`
# reuses a master still alive from an earlier sync; otherwise -N -f
# opens one and ControlPersist keeps it for the next.
if ! ssh "${ssh_opts[@]}" -O check "choir@$IP" 2>/dev/null; then
  ssh "${ssh_opts[@]}" -N -f "choir@$IP"
fi
t_conn=$EPOCHREALTIME

# Note: rsync overwrites .git/config, so the mirror remote (whose URL embeds
# the on-box token) is re-created on the VM on every push.
# --filter=':- .gitignore' makes rsync skip everything git ignores
# (CLAUDE.md, target/, ...), so local-only files never reach the VM.
rsync -az -e "ssh ${ssh_opts[*]}" --filter=':- .gitignore' "$REPO_DIR/" "choir@$IP:~/choir-src/"
t_rsync=$EPOCHREALTIME

ssh "${ssh_opts[@]}" "choir@$IP" 'cd ~/choir-src \
  && git remote remove mirror 2>/dev/null || true \
  && git remote add mirror "http://choir:$(cat ~/.forgejo-token)@127.0.0.1:3000/choir/choir.git" \
  && git push -q mirror main \
  && git push -q --tags mirror \
  && git log --oneline -1'
t_push=$EPOCHREALTIME

printf 'mirror: connect %.1fs | rsync %.1fs | box-local push %.1fs | total %.1fs\n' \
  $((t_conn - t_start)) $((t_rsync - t_conn)) $((t_push - t_rsync)) $((t_push - t_start))
echo "mirror updated"
