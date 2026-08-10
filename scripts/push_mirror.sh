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
# ControlPersist outlives this script deliberately, so a second sync
# within the minute pays no handshake at all. The socket is a control
# channel, not a credential; it lives under ~/.ssh with the key.
SSH_CONTROL=~/.ssh/cm-choir-mirror.sock
ssh_opts=(-i "$KEY" -o IdentitiesOnly=yes -o ControlMaster=auto \
          -o ControlPath="$SSH_CONTROL" -o ControlPersist=60)

# Note: rsync overwrites .git/config, so the mirror remote (whose URL embeds
# the on-box token) is re-created on the VM on every push.
# --filter=':- .gitignore' makes rsync skip everything git ignores
# (CLAUDE.md, target/, ...), so local-only files never reach the VM.
rsync -az -e "ssh ${ssh_opts[*]}" --filter=':- .gitignore' "$REPO_DIR/" "choir@$IP:~/choir-src/"
ssh "${ssh_opts[@]}" "choir@$IP" 'cd ~/choir-src \
  && git remote remove mirror 2>/dev/null || true \
  && git remote add mirror "http://choir:$(cat ~/.forgejo-token)@127.0.0.1:3000/choir/choir.git" \
  && git push -q mirror main \
  && git push -q --tags mirror \
  && git log --oneline -1'
echo "mirror updated"
