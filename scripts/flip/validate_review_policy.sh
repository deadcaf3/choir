#!/bin/sh
# Validates the operator files that make an enabled review gate usable.
# Pure and side-effect free so install_node.sh can fail before touching
# the running daemon, and tests can exercise the macOS system awk path.
#
# There are two ways a protected ref can be landed on, and this script
# has to accept both, because the daemon does:
#
#   Owner basis (D42). `authorize` reaches `owner_assented` before it
#   ever weighs approvals, so a repository with an `own` grant in the
#   ACL is landed by its owner with no reviewers drawn at all. A person
#   working alone on their own repository is a supported configuration
#   in the engine, and used not to be one in this script -- it demanded
#   a second operator that the code path in question never consults.
#
#   Quorum basis. No owner, so REQUIRED_APPROVAL_WEIGHT applies: two
#   approvals from two distinct operators. `assign_reviewers` never
#   draws a reviewer sharing the author's own operator prefix, and takes
#   one seat per operator, so a pool of two operators leaves exactly one
#   drawable seat whenever the author is one of them -- a review that
#   can be opened, assigned, and then never approved. Three distinct
#   operators is the smallest pool that keeps a quorum reachable for any
#   author. A dedicated pool that no author belongs to would be usable
#   at two, but nothing here can verify that disjointness, so this fails
#   closed at three rather than rendering a unit that deadlocks.
set -eu

if [ "$#" -ne 3 ] && [ "$#" -ne 4 ]; then
  echo "usage: validate_review_policy.sh <keys> <reviewers> <protected-refs> [acl]" >&2
  exit 2
fi

KEYS=$1
REVIEWERS=$2
PROTECTED_REFS=$3
ACL=${4:-}

for file in "$KEYS" "$REVIEWERS" "$PROTECTED_REFS"; do
  if [ ! -f "$file" ]; then
    echo "review gate policy file is missing" >&2
    exit 1
  fi
done

protected_count=$(awk '!/^[[:space:]]*($|#)/ { count++ } END { print count+0 }' "$PROTECTED_REFS")
if [ "$protected_count" -lt 1 ]; then
  echo "review gate needs one protected ref" >&2
  exit 1
fi

# Every reviewer named must have a bound key, on either basis: an owner
# repository may still list reviewers, and a name in the pool that the
# node cannot resolve to a key is a draw that fails at request time.
while IFS= read -r reviewer; do
  case "$reviewer" in
    ""|\#*) continue ;;
  esac
  if ! awk -v name="$reviewer" '$1 == name && NF == 2 { found=1 } END { exit !found }' "$KEYS"; then
    echo "reviewer pool entry has no bound key" >&2
    exit 1
  fi
done < "$REVIEWERS"

# Owner basis. A protected ref is `<repo>:<refname>`; the repository is
# owned when the ACL carries an `own` grant naming it, or naming `*`,
# which `Effective::has_owner` treats as covering every repository. Only
# a file that covers *every* protected repository clears the pool
# requirement -- one owned repository does not license an unowned one.
owner_covers_all=0
if [ -n "$ACL" ] && [ -f "$ACL" ]; then
  owner_covers_all=$(awk -v aclfile="$ACL" '
    BEGIN {
      while ((getline line < aclfile) > 0) {
        sub(/#.*/, "", line)
        if (split(line, field) >= 3 && field[3] == "own") { owned[field[2]] = 1 }
      }
      close(aclfile)
    }
    !/^[[:space:]]*($|#)/ {
      line = $0
      sub(/#.*/, "", line)
      split(line, column)
      repo = column[1]
      sub(/:.*/, "", repo)
      if (repo == "") { next }
      total++
      if (owned[repo] || owned["*"]) { covered++ }
    }
    END { print (total > 0 && total == covered) ? 1 : 0 }
  ' "$PROTECTED_REFS")
fi

if [ "$owner_covers_all" -eq 1 ]; then
  exit 0
fi

operator_count=$(awk '!/^[[:space:]]*($|#)/ { split($0, parts, "/"); seen[parts[1]]=1 } END { for (prefix in seen) count++; print count+0 }' "$REVIEWERS")
if [ "$operator_count" -lt 3 ]; then
  echo "review gate needs an owner for every protected repository, or three reviewer prefixes" >&2
  echo "grant \`own\` in the ACL to land as the owner (D42), or add reviewers: an approval" >&2
  echo "needs two distinct operators and the author's own operator is never drawn" >&2
  exit 1
fi
