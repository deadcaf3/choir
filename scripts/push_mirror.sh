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

# Note: rsync overwrites .git/config, so the mirror remote (whose URL embeds
# the on-box token) is re-created on the VM on every push.
# --filter=':- .gitignore' makes rsync skip everything git ignores
# (CLAUDE.md, target/, ...), so local-only files never reach the VM.
rsync -az -e "ssh -i $KEY" --filter=':- .gitignore' "$REPO_DIR/" "choir@$IP:~/choir-src/"
ssh -i "$KEY" "choir@$IP" 'cd ~/choir-src \
  && git remote remove mirror 2>/dev/null || true \
  && git remote add mirror "http://choir:$(cat ~/.forgejo-token)@127.0.0.1:3000/choir/choir.git" \
  && git push -q mirror main && git log --oneline -1'
echo "mirror updated"
