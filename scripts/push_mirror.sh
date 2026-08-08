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

rsync -az -e "ssh -i $KEY" --exclude target "$REPO_DIR/" "choir@$IP:~/choir-src/"
ssh -i "$KEY" "choir@$IP" 'cd ~/choir-src && git push -q mirror main && git log --oneline -1'
echo "mirror updated"
