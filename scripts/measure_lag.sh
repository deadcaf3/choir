#!/bin/sh
# Reproduces the lag-meter figures quoted in the PHASE0 entry "Two
# production checks that could not fail".
#
# Drives a release choir-node with real signed submissions over HTTP and
# prints what the node's own meter says afterwards. Serial client, so one
# op per durability barrier -- the worst shape for `durable` and the one
# the entry reports.
#
#   sh scripts/measure_lag.sh [op-count]
#
# Env: TARGET_DIR (default: a private target dir under /tmp), WORK
# (scratch node root), PORT.
#
# TARGET_DIR must NOT be the shared worktree target. A sibling worktree's
# build can overwrite an artifact there, and measuring another tree's
# binary is worse than not measuring. Build first:
#
#   CHOIR_GIT_HEAD=$(git rev-parse HEAD) CARGO_TARGET_DIR=$TARGET_DIR \
#     cargo build --release -p choir-node -p choir-bridge
#   CHOIR_GIT_HEAD=$(git rev-parse HEAD) CARGO_TARGET_DIR=$TARGET_DIR \
#     cargo build --release -p choir-node --example sign_submit
#
# Expect: decision p50 <= 256 us, durable p50 <= 8.19 ms, zero breaches.
# Do NOT expect the maxima to reproduce -- they moved 32x across four
# runs on an idle machine, which is why the entry calls them noise.
set -e

TARGET_DIR=${TARGET_DIR:-/tmp/choir-lag-target}
WORK=${WORK:-/tmp/choir-lagnode}
PORT=${PORT:-18420}
N=${1:-200}
BIN=$TARGET_DIR/release

for f in "$BIN/choir-node" "$BIN/choir-bridge" "$BIN/examples/sign_submit"; do
  [ -x "$f" ] || { echo "missing $f -- see the build commands in this file's header" >&2; exit 1; }
done

rm -rf "$WORK"
mkdir -p "$WORK"
KEY=$WORK/agent.key
PUB=$("$BIN/choir-bridge" --pubkey "$KEY")
echo "agent $PUB" > "$WORK/keys"

"$BIN/choir-node" "$WORK/repos" "$PORT" --keys-file "$WORK/keys" 2> "$WORK/node.log" &
NODE=$!
trap 'kill $NODE 2>/dev/null || true' EXIT

i=0
while [ $i -lt 40 ]; do
  curl -s -o /dev/null "http://127.0.0.1:$PORT/api/view" && break
  i=$((i + 1))
done

i=0
while [ $i -lt "$N" ]; do
  op=$(python3 -c "import json,sys;i=int(sys.argv[1]);print(json.dumps({'format_version':1,'kind':{'SetRef':{'name':'ref-%d'%i,'commit':{'codec':30,'digest':[i%256]*32},'prev':None}}}))" $i)
  body=$("$BIN/examples/sign_submit" "$KEY" agent "$op")
  curl -s -o /dev/null -X POST -d "$body" "http://127.0.0.1:$PORT/api/submit"
  i=$((i + 1))
done

curl -s "http://127.0.0.1:$PORT/api/view" | python3 -c 'import json,sys; v=json.load(sys.stdin); print(json.dumps(v["sequencer_lag"], indent=2)); print(json.dumps(v["build"]))'
echo "--- breaches written:"
wc -l "$WORK/repos/.choir/lag.jsonl" 2>/dev/null || echo "  (no lag.jsonl: no breach, which is the expected result)"
head -2 "$WORK/node.log"
