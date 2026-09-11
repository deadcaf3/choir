#!/usr/bin/env bash
# demo/run.sh -- a scripted, re-runnable terminal demo against a live choir node.
#
#   demo/run.sh [--pause] [--port N] [--no-build]
#
# --pause     wait for Enter after each beat (for recording); default: no waits.
# --port      loopback port for the node (default 8447).
# --no-build  use the binaries as built; skips cargo, and its target-dir lock.
#
# Idempotent: kills the node a previous take left behind, wipes demo/.run,
# mints fresh keys, starts a fresh node, plays every beat. Hermetic: loopback
# only, no TLS, nothing leaves this machine. Needs: cargo, git, curl, python3.
#
# What it shows (README.md is the shot list):
#   A. the queue never blocks: a conflicting proposal is evicted and the
#      change behind it lands in the same round; the conflict then lands as
#      a committed value the platform reads as one; work builds on top of it
#      without waiting; the resolution is a later commit; and the whole
#      sequence is a signed, hash-chained order anyone can verify offline.
#   B. a CI *infrastructure* failure is Errored, not Failed: the queue
#      requeues the change instead of blaming it; a real red test is Failed
#      and evicted, and the green change in the same round still lands.

set -euo pipefail

PAUSE=0
PORT=8447
BUILD=1
while [ $# -gt 0 ]; do
  case "$1" in
    --pause) PAUSE=1 ;;
    --port) PORT="$2"; shift ;;
    --no-build) BUILD=0 ;;
    *) echo "usage: demo/run.sh [--pause] [--port N] [--no-build]" >&2; exit 2 ;;
  esac
  shift
done

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$HERE/.." && pwd)"
RUN="$HERE/.run"
API="http://127.0.0.1:$PORT"
REPO="acme/app.git"
URL="$API/$REPO"

# ---- presentation -----------------------------------------------------------
if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
  B=$'\033[1m'; D=$'\033[2m'; C=$'\033[36m'; Y=$'\033[33m'; N=$'\033[0m'
else
  B=; D=; C=; Y=; N=
fi

beat() {      # beat <n> <title>
  printf '\n%s== beat %s: %s ==%s\n' "$B$C" "$1" "$2" "$N"
}
say() {       # one narration line before a command: what happens, what to watch
  printf '%s# %s%s\n' "$Y" "$*" "$N"
}
note() { printf '%s%s%s\n' "$D" "$*" "$N"; }
typed() {     # the argv as a person would type it: run dir elided, spaces quoted
  local out="" a
  for a in "$@"; do
    a="${a//$RUN\//}"
    case "$a" in *' '*) a="\"$a\"" ;; esac
    out="$out${out:+ }$a"
  done
  printf '%s' "$out"
}
run() {       # print the command as typed, then run it
  printf '%s$ %s%s\n' "$B" "$(typed "$@")" "$N"
  "$@"
}
in_dir() {    # in_dir <dir> <cmd...>: same, prefixed with the directory
  local dir="$1"; shift
  printf '%s%s $ %s%s\n' "$B" "$(basename "$dir")" "$(typed "$@")" "$N"
  (cd "$dir" && "$@")
}
push() {      # push <dir> <refspec>: a push whose own report is the evidence
  printf '%s%s $ git push origin %s%s\n' "$B" "$(basename "$1")" "$2" "$N"
  (cd "$1" && git push origin "$2" 2>&1 | grep -v '^remote: [^c]' || true)   # keep `remote: choir:` reasons
}
pause() {
  if [ "$PAUSE" = 1 ]; then printf '%s[Enter]%s' "$D" "$N"; read -r _; fi
}

# ---- helpers over the node's API (compact readers for the screen) -----------
# The full documents are `choir view <api>` and `choir log <api>`; show.py
# only trims them to the rows a beat is about.
choir() { "$BIN/choir" "$@"; }
show() { python3 "$HERE/show.py" "$@" | sed "s|$RUN/||g"; }
refs()      { curl -s "$API/api/view" | show refs; }
log_tail()  { curl -s "$API/api/log?from=$1" | show log; }   # every ref op since seq $1
checks()    { curl -s "$API/api/view" | show checks | cut -c -100; }
next_seq()  { curl -s "$API/api/view" | show next-seq; }
queue_run() { # one round of the node's merge queue against main
  curl -s -X POST "$API/api/queue/run" -d "{\"repo\":\"$REPO\",\"branch\":\"main\"}" | show queue
}
conflict_page() { # how the node's own web view reads a file at a revision
  curl -s "$API/r/${REPO%.git}/blob/$1/$2" | show conflict
}

# ---- 0. build, reset, start -------------------------------------------------
cd "$REPO_ROOT"
# A target dir of its own. The workspace's target dir may be shared with
# other checkouts, whose builds overwrite `debug/choir-node` with a binary
# built from *their* sources; a take would then run somebody else's node.
TARGET="$HERE/.target"
BIN="$TARGET/debug"
if [ "$BUILD" = 1 ]; then
  note "building choir-node and choir into demo/.target (first build takes minutes; a rebuild is seconds)"
  cargo build -q --target-dir "$TARGET" -p choir-node -p choir-cli
fi
[ -x "$BIN/choir-node" ] && [ -x "$BIN/choir" ] || { echo "no binaries in $BIN; run without --no-build" >&2; exit 1; }

if [ -f "$RUN/node.pid" ]; then
  kill "$(cat "$RUN/node.pid")" 2>/dev/null || true
  sleep 0.2
fi
if curl -s -o /dev/null "$API/healthz"; then
  echo "something else is already listening on $API; pass --port" >&2
  exit 1
fi
rm -rf "$RUN"
mkdir -p "$RUN/root" "$RUN/queue" "$RUN/ci"

# git: quiet, deterministic, no signing, no rerere, diff3 so the base is visible
export GIT_TERMINAL_PROMPT=0
export GIT_AUTHOR_NAME=agent GIT_COMMITTER_NAME=agent GIT_AUTHOR_EMAIL=agent GIT_COMMITTER_EMAIL=agent
export GIT_CONFIG_COUNT=5
export GIT_CONFIG_KEY_0=commit.gpgsign GIT_CONFIG_VALUE_0=false
export GIT_CONFIG_KEY_1=init.defaultBranch GIT_CONFIG_VALUE_1=main
export GIT_CONFIG_KEY_2=merge.conflictStyle GIT_CONFIG_VALUE_2=diff3
export GIT_CONFIG_KEY_3=rerere.enabled GIT_CONFIG_VALUE_3=false
export GIT_CONFIG_KEY_4=advice.detachedHead GIT_CONFIG_VALUE_4=false

# fresh keys: one operator key the node trusts (turns the platform API on)
"$BIN/choir" key "$RUN/operator.key" acme/operator > "$RUN/trusted-keys"
# the CI runner the queue calls: it runs the repo's own test.sh
printf '#!/bin/sh\nexec ./test.sh\n' > "$RUN/ci/run-tests"
chmod +x "$RUN/ci/run-tests"
cat > "$RUN/ci-command.json" <<EOF
{"format_version":1,"program":"$RUN/ci/run-tests","args":[],"timeout_seconds":30}
EOF

"$BIN/choir-node" "$RUN/root" "$PORT" --create "$REPO" \
  --keys-file "$RUN/trusted-keys" \
  --ci-command "$RUN/ci-command.json" --queue-tree "$RUN/queue" \
  > "$RUN/node.log" 2>&1 &
echo $! > "$RUN/node.pid"
for _ in $(seq 1 100); do curl -s -o /dev/null "$API/healthz" && break; sleep 0.05; done
curl -s -o /dev/null "$API/healthz" || { echo "node did not come up; see $RUN/node.log" >&2; exit 1; }
# the node's public key, so beat 5 can verify the signatures on git-pushed ops
"$BIN/choir" key "$RUN/root/.choir/node.key" node > "$RUN/node.pub"

beat 0 "a choir node on loopback, one empty repo"
say "one daemon, one bare repo, plain git smart-HTTP, a merge queue that runs the repo's test.sh."
note "  repo: $URL"
run curl -s -o /dev/null -w 'healthz %{http_code}\n' "$API/healthz"
pause

# ---- 1. plain git, three agents, agent1 lands -------------------------------
beat 1 "three git clients; agent1 lands a line"
say "seed: one config line and a test that checks it. Three agents clone the same base."
in_dir "$RUN" git clone -q "$URL" agent1 2>/dev/null
printf 'greeting = "hello"\n' > "$RUN/agent1/config.toml"
printf '#!/bin/sh\n# the repo'"'"'s own test: the greeting must be a quoted word\ngrep -q '"'"'^greeting = "[a-z]*"$'"'"' config.toml\n' > "$RUN/agent1/test.sh"
chmod +x "$RUN/agent1/test.sh"
in_dir "$RUN/agent1" git add .
in_dir "$RUN/agent1" git commit -q -m "base: greeting + test"
in_dir "$RUN/agent1" git push -q origin HEAD:main
in_dir "$RUN" git clone -q "$URL" agent2
in_dir "$RUN" git clone -q "$URL" agent3
say "agent1 changes THE line and pushes straight to main. An ordinary push, accepted, sequenced."
printf 'greeting = "hola"\n' > "$RUN/agent1/config.toml"
in_dir "$RUN/agent1" git commit -q -am "agent1: greet in Spanish"
push "$RUN/agent1" main
pause

# ---- 2. the queue never blocks ---------------------------------------------
beat 2 "the queue: a conflicting proposal is evicted, the change behind it lands"
say "agent2 changed the same line from the same base; agent3 added a file. Both propose to main."
printf 'greeting = "bonjour"\n' > "$RUN/agent2/config.toml"
in_dir "$RUN/agent2" git commit -q -am "agent2: greet in French"
in_dir "$RUN/agent2" git push -q origin HEAD:refs/for/main/agent2/french
printf '# Notes\n\nThe greeting is being decided; see config.toml.\n' > "$RUN/agent3/NOTES.md"
in_dir "$RUN/agent3" git add NOTES.md
in_dir "$RUN/agent3" git commit -q -m "agent3: add notes"
in_dir "$RUN/agent3" git push -q origin HEAD:refs/for/main/agent3/notes
SEQ_ROUND=$(next_seq)
say "one round. agent2 is ahead of agent3 in the train. Watch: the conflict does not hold agent3 up."
run queue_run
say "on a forge, a conflicted PR cannot even enter the queue and its author is paged. Here it is a"
say "verdict in the round: evicted first-class, main moved without it, and agent2 still owns the fix."
run refs
pause

# ---- 3. the conflict lands as a value ---------------------------------------
beat 3 "agent2's conflict lands as a value: all three sides on main"
say "agent2 pulls main. git says CONFLICT, as it would anywhere. Nobody stops: it is committed as-is."
printf '%sagent2 $ git pull --no-rebase -q origin main%s\n' "$B" "$N"
(cd "$RUN/agent2" && git pull --no-rebase -q origin main 2>&1 | grep -i '^CONFLICT' || true)
in_dir "$RUN/agent2" git add config.toml
in_dir "$RUN/agent2" git commit -q -m "merge main: conflict kept as a value"
push "$RUN/agent2" main
in_dir "$RUN/agent2" git push -q origin :refs/for/main/agent2/french
say "a forge would show you a file with markers in it. The node reads that commit as what it is:"
say "a conflict, with its base and both sides, on main, for anyone or any agent to pick up."
run conflict_page main config.toml
pause

# ---- 4. work on top, then the resolution -----------------------------------
beat 4 "agent3 builds ON the conflict; the resolution comes after"
say "agent3 pulls main and gets the conflict commit. Its work does not touch that line. It lands."
say "on a forge agent3 waits for agent2, or rebases around a PR that is 'blocked'. Here nobody waits."
in_dir "$RUN/agent3" git pull --no-rebase -q origin main
in_dir "$RUN/agent3" git log --oneline -1
printf '\nEdit config.toml to change the greeting.\n' >> "$RUN/agent3/NOTES.md"
in_dir "$RUN/agent3" git commit -q -am "agent3: notes on config (on top of the unresolved conflict)"
push "$RUN/agent3" main
say "agent2 comes back, picks the answer, pushes. The markers go; the history keeps the conflict."
in_dir "$RUN/agent2" git pull --no-rebase -q origin main
printf 'greeting = "hola"\n' > "$RUN/agent2/config.toml"
in_dir "$RUN/agent2" git commit -q -am "resolve: greeting is Spanish"
push "$RUN/agent2" main
in_dir "$RUN/agent2" git log --graph --oneline main
in_dir "$RUN/agent2" ./test.sh
note "(test.sh passes again: exit 0)"
pause

# ---- 5. the order, verified ------------------------------------------------
beat 5 "the order: every move above, signed, chained, verified offline"
say "beats 2 to 4 as the log has them: the landing, the conflict, the work on top, the resolution."
say "each names the tip it replaced (CAS) and the entry before it (chain). Nobody waited on anybody."
run log_tail "$SEQ_ROUND"
say "a forge gives you an audit log you read on its site. This is a hash chain you verify offline:"
say "re-derive all of it from the wire format with nothing but the node's public key."
run choir log "$API" --verify --keys "$RUN/node.pub" 2>&1 >/dev/null | tail -n 1
pause

# ---- 6. Errored vs Failed in the merge queue --------------------------------
beat 6a "the queue: a CI infrastructure failure is Errored"
say "two new proposals: agent3 breaks the test, agent1 adds a line to NOTES. Then the CI runner breaks."
in_dir "$RUN/agent3" git push -q origin :refs/for/main/agent3/notes
in_dir "$RUN/agent3" git pull --no-rebase -q origin main
printf 'greeting = hi\n' > "$RUN/agent3/config.toml"
in_dir "$RUN/agent3" git commit -q -am "agent3: unquoted greeting (breaks test.sh)"
in_dir "$RUN/agent3" git push -q origin HEAD:refs/for/main/agent3/unquoted
in_dir "$RUN/agent1" git pull --no-rebase -q origin main
printf '\nRun ./test.sh before you push.\n' >> "$RUN/agent1/NOTES.md"
in_dir "$RUN/agent1" git commit -q -am "agent1: notes on testing"
in_dir "$RUN/agent1" git push -q origin HEAD:refs/for/main/agent1/testing
run chmod -x "$RUN/ci/run-tests"
say "round 1. Watch who gets blamed."
run queue_run
say "a forge queue drops the PR either way; the agent re-queues it blind. Here Errored is a statement"
say "about us, not the change: both stay queued, nothing evicted, main unmoved, the reason recorded."
run checks
SUBJECT="$(curl -s "$API/api/view" | show subject)"
printf '%s$ choir checks %s %s%s\n' "$B" "$API" "${SUBJECT:0:7}" "$N"
set +e; choir checks "$API" "$SUBJECT" >/dev/null 2>&1; rc=$?; set -e
note "  exit $rc   (0 passed, 1 failed, 3 running, 4 could not be run)"
pause

beat 6b "the queue: a red test is Failed, and only that change pays"
say "restore the runner. Same proposals, same node; now CI really runs test.sh."
run chmod +x "$RUN/ci/run-tests"
run queue_run
say "Failed is a statement about the change: evicted. The green proposal in the same round landed."
run refs
in_dir "$RUN/agent1" git pull --no-rebase -q origin main
in_dir "$RUN/agent1" git log --oneline -2

printf '\n%sdone. node still running at %s; re-run this script for another take.%s\n' "$D" "$API" "$N"
