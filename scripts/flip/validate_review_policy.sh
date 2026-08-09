#!/bin/sh
# Validates the operator files that make an enabled review gate usable.
# Pure and side-effect free so install_node.sh can fail before touching
# the running daemon, and tests can exercise the macOS system awk path.
set -eu

if [ "$#" -ne 3 ]; then
  echo "usage: validate_review_policy.sh <keys> <reviewers> <protected-refs>" >&2
  exit 2
fi

KEYS=$1
REVIEWERS=$2
PROTECTED_REFS=$3

for file in "$KEYS" "$REVIEWERS" "$PROTECTED_REFS"; do
  if [ ! -f "$file" ]; then
    echo "review gate policy file is missing" >&2
    exit 1
  fi
done

reviewer_count=$(awk '!/^[[:space:]]*($|#)/ { count++ } END { print count+0 }' "$REVIEWERS")
operator_count=$(awk '!/^[[:space:]]*($|#)/ { split($0, parts, "/"); seen[parts[1]]=1 } END { for (prefix in seen) count++; print count+0 }' "$REVIEWERS")
protected_count=$(awk '!/^[[:space:]]*($|#)/ { count++ } END { print count+0 }' "$PROTECTED_REFS")

if [ "$reviewer_count" -lt 2 ] || [ "$operator_count" -lt 2 ] || [ "$protected_count" -lt 1 ]; then
  echo "review gate needs two reviewer prefixes and one protected ref" >&2
  exit 1
fi

while IFS= read -r reviewer; do
  case "$reviewer" in
    ""|\#*) continue ;;
  esac
  if ! awk -v name="$reviewer" '$1 == name && NF == 2 { found=1 } END { exit !found }' "$KEYS"; then
    echo "reviewer pool entry has no bound key" >&2
    exit 1
  fi
done < "$REVIEWERS"
