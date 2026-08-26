#!/bin/sh
# Validates that a private-beta ACL grants no beta user more than the
# repositories it names. Pure and side-effect free, like its sibling
# validate_review_policy.sh, so the renderer can fail before touching a
# running daemon and a test can drive it directly.
#
# The rule is narrower than "no wildcards", because the beta needs one.
# The scope column takes three forms and two of them are wide:
#
#   owner/repo   one repository
#   *            every repository -- and only repositories
#   @node        the node itself: the op log, the attestation, and ops
#                that name no repository. Never matched by `*`.
#
# `@node` is how the operator credential holds the node-wide audit grant
# the runbook requires for recovery, so it stays. `*` is the one that has
# no legitimate use here: a beta user reaches every repository on the
# node, including repositories belonging to other beta users, and the
# grant does not name any of them so nobody reviewing the file sees who
# was exposed.
set -eu

if [ "$#" -ne 1 ]; then
  echo "usage: validate_beta_acl.sh <acl>" >&2
  exit 2
fi

ACL=$1

if [ ! -f "$ACL" ]; then
  echo "ACL is missing: $ACL" >&2
  exit 1
fi

# Comments run to end of line, so strip them before reading the columns:
# a row's scope is its second field once `#` and everything after it is
# gone. An empty second field means a malformed row, which is the ACL
# parser's error to report and not this one's.
if awk '{ sub(/#.*/, "") } $2 == "*" { print NR": "$1; found=1 } END { exit !found }' "$ACL" >&2; then
  echo 'private beta ACL grants `*`: every repository on the node, named above by line' >&2
  echo 'give each beta user their repositories by name; `@node` is the operator grant' >&2
  exit 1
fi
