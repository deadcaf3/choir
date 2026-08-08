#!/bin/zsh
# Phase-0 task 7 (plan.md roadmap): reproduce jj first-class conflicts on a
# many-agent workload. Three workspaces edit the same file divergently; the
# merge commits a *conflicted state as a valid commit* (the L1 property the
# platform relies on: agents keep working, resolution is deferred).
set -euo pipefail
export PATH="/opt/homebrew/bin:$PATH"

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
cd "$WORK"

jj git init repo >/dev/null
cd repo
jj config set --repo user.name "spike"
jj config set --repo user.email "spike@localhost"

echo "shared base" > f.txt
jj describe -m base >/dev/null
BASE=$(jj log -r @ --no-graph -T 'change_id.short()')

# Simulate 3 agents: divergent children of base, same file, different edits.
AGENTS=()
for agent in a1 a2 a3; do
  jj new "$BASE" >/dev/null 2>&1
  echo "edit by $agent" > f.txt
  jj describe -m "agent-$agent" >/dev/null
  AGENTS+=($(jj log -r @ --no-graph -T 'change_id.short()'))
done

# Additionally exercise the multi-workspace primitive itself.
jj workspace add ../ws2 >/dev/null 2>&1
jj workspace list

# Octopus-style merge of all three agent changes.
jj new "${AGENTS[@]}"

echo "--- status after merge (expect conflict, recorded in a real commit):"
jj st || true
echo "--- log (conflict flag on the merge commit):"
jj log -r 'conflicts()' --no-graph -T 'change_id.short() ++ " conflict=" ++ if(conflict, "yes", "no") ++ "\n"'

# The critical property: the conflicted commit exists and new work can proceed on top.
jj new -m "work-continues-on-top-of-conflict" >/dev/null 2>&1
echo "unrelated progress" > g.txt
jj st | head -5

CONFLICTED=$(jj log -r 'conflicts()' --no-graph -T 'change_id.short()' | head -1)
if [ -n "$CONFLICTED" ]; then
  echo "RESULT: PASS — conflicted merge committed as valid state ($CONFLICTED); work continued on top"
else
  echo "RESULT: FAIL — no first-class conflict recorded"
  exit 1
fi
