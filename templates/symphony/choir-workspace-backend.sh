#!/usr/bin/env bash
# External workspace backend for a Symphony workspace-manager seam.
# One versioned JSON request enters on stdin. One JSON result leaves on stdout.
#
# Identity derivation, signing, and every call to Choir belong to
# `choir runner`. What is left here is the part that is genuinely about
# Symphony: its request shape, its result envelope, and the two things
# the generic seam has no way to know about.
#
#   1. A checkpoint names an immutable revision, so the object has to be
#      committed and pushed before Choir can be asked to record it. Only
#      a caller holding the workspace can do that.
#   2. Symphony hands back a workspace path on later calls. A path is not
#      proof of a binding, so before acting on one this script checks the
#      metadata it wrote inside that workspace still names the change the
#      current request derives.
#
# There is deliberately no durable adapter state. The node reports the
# base revision a change is actually bound to, so a retry that resolves a
# moved ref still reports the original base, and two schedulers racing the
# same request converge through the idempotency key rather than through a
# lock on this machine.

set -euo pipefail

usage() {
  echo "usage: choir-workspace-backend.sh <config.json>" >&2
  exit 2
}

fail_json() {
  local code=$1
  local retryable=$2
  local message=$3
  jq -n --arg code "$code" --arg message "$message" --argjson retryable "$retryable" \
    '{protocol_version: 1, error: {code: $code, retryable: $retryable, message: $message}}'
  echo "choir-symphony: $code" >&2
  exit 1
}

string_field() {
  local document=$1
  local expression=$2
  jq -er "$expression | select(type == \"string\" and length > 0)" \
    <<<"$document" 2>/dev/null
}

canonical_directory() {
  local path=$1
  [[ "$path" == /* && -d "$path" && ! -L "$path" ]] || return 1
  (cd "$path" && pwd -P)
}

[[ $# -eq 1 ]] || usage
config_file=$1
command -v jq >/dev/null 2>&1 || {
  echo "choir-symphony: jq is required" >&2
  exit 1
}
[[ -r "$config_file" ]] || fail_json invalid_config false "cannot read the config file"

# The same file is handed to `choir runner`, which reads api, repo, owner,
# key_file, namespace, base_ref and the auth pair and validates them
# itself. Listing the keys here only rejects a stale or misspelled config
# early; it is not a second copy of the runner's rules.
config=$(<"$config_file")
jq -e '
  type == "object" and
  ((keys - ["api", "auth_file", "auth_user", "base_ref", "choir_bin", "git_bin",
            "key_file", "namespace", "owner", "repo"]) | length == 0)
' <<<"$config" >/dev/null 2>&1 \
  || fail_json invalid_config false "config contains invalid JSON or unknown fields"

choir_bin=$(jq -er '(.choir_bin // "choir") | select(type == "string" and length > 0)' \
  <<<"$config" 2>/dev/null) || fail_json invalid_config false "config choir_bin is invalid"
git_bin=$(jq -er '(.git_bin // "git") | select(type == "string" and length > 0)' \
  <<<"$config" 2>/dev/null) || fail_json invalid_config false "config git_bin is invalid"

# Only the fields this script uses on its own are checked here. The key
# file is the exception: the runner refuses a relative path but cannot see
# that a 32-byte secret was replaced by a symlink to something else.
key_file=$(string_field "$config" '.key_file') \
  || fail_json invalid_config false "config needs string key_file"
[[ -f "$key_file" && -r "$key_file" && ! -L "$key_file" ]] \
  || fail_json invalid_config false "owner key file is missing, unreadable, or symlinked"
[[ "$(wc -c <"$key_file" | tr -d '[:space:]')" == "32" ]] \
  || fail_json invalid_config false "owner key file must contain exactly 32 bytes"

# Runs one lifecycle step, leaving the runner's result in `runner_result`.
#
# Deliberately not a command substitution: a refusal has to end this
# script, and `exit` inside `$(...)` ends only the subshell, leaving the
# caller to carry on with an error document as if it were a result.
#
# A refusal is already a typed `{protocol_version, error}` document on
# stdout, so it is passed through rather than reclassified: this script
# has no information the runner lacked, and a second opinion about
# `retryable` is a second chance to get it wrong.
runner_result=""
run_runner() {
  local request=$1 rc=0
  runner_result=$("$choir_bin" runner "$config_file" <<<"$request") || rc=$?
  [[ $rc -eq 0 ]] && return 0
  if jq -e 'type == "object" and (.error | type) == "object"' \
    <<<"$runner_result" >/dev/null 2>&1; then
    printf '%s\n' "$runner_result"
    exit 1
  fi
  fail_json choir_unavailable true "choir runner produced no typed result"
}

request=$(cat)
jq -e 'type == "object" and .protocol_version == 1' <<<"$request" >/dev/null 2>&1 \
  || fail_json invalid_request false "request must be a protocol_version 1 object"
operation=$(string_field "$request" '.operation') \
  || fail_json invalid_request false "request needs string operation"
case "$operation" in
  ensure|checkpoint|archive) ;;
  *) fail_json unsupported_operation false "operation must be ensure, checkpoint, or archive" ;;
esac
issue_id=$(string_field "$request" '.issue.id') \
  || fail_json invalid_request false "request needs issue.id"
issue_identifier=$(string_field "$request" '.issue.identifier') \
  || fail_json invalid_request false "request needs issue.identifier"
workspace_key=$(string_field "$request" '.workspace_key') \
  || fail_json invalid_request false "request needs workspace_key"
generation=$(string_field "$request" '.generation') \
  || fail_json invalid_request false "request needs a stable change generation"
run_id=$(jq -er '(.run_id // "") | select(type == "string")' <<<"$request" 2>/dev/null) \
  || fail_json invalid_request false "request run_id must be a string"

# Symphony's tracker id is what "this unit of work" means, and its
# generation is what "this attempt at it" means. Which fields carry that
# meaning is the only thing the seam needs to be told; the rules for them
# are the runner's.
runner_request() {
  local op=$1
  jq -n --arg operation "$op" --arg workspace_key "$workspace_key" \
    --arg external_id "$issue_id" --arg generation "$generation" \
    '{protocol_version: 1, operation: $operation, scheme: "from-external",
      workspace_key: $workspace_key, external_id: $external_id,
      generation: $generation}'
}

# Symphony's binding is exactly these six fields. The runner reports more,
# and passing its object through unchanged would let a future field become
# part of this contract without anyone deciding that.
symphony_binding() {
  jq -c '{repo, workspace_id, change_id, idempotency_key, owner, base}' <<<"$1"
}

# Path to the metadata this script writes inside a live workspace.
sidecar_path() {
  printf '%s/.git/choir/symphony-backend.json' "$1"
}

if [[ "$operation" == "ensure" ]]; then
  run_runner "$(runner_request ensure)"
  result=$runner_result
  binding=$(jq -ce '.binding | select(type == "object")' <<<"$result") \
    || fail_json invalid_response true "choir runner returned no binding"
  path=$(string_field "$result" '.workspace.path') \
    || fail_json invalid_response true "choir runner returned no workspace path"
  canonical_path=$(canonical_directory "$path") \
    || fail_json invalid_response true "Choir returned an inaccessible or unsafe path"
  [[ -d "$canonical_path/.git" && ! -L "$canonical_path/.git" ]] \
    || fail_json invalid_response true "Choir workspace has no standalone Git metadata"

  sidecar_dir="$canonical_path/.git/choir"
  [[ ! -e "$sidecar_dir" || ! -L "$sidecar_dir" ]] \
    || fail_json invalid_response false "workspace metadata directory is a symlink"
  umask 077
  mkdir -p "$sidecar_dir" || fail_json state_unavailable true "cannot create workspace metadata"
  tmp_sidecar=$(mktemp "$sidecar_dir/.symphony-backend.XXXXXX") \
    || fail_json state_unavailable true "cannot create workspace metadata"
  jq -n --argjson binding "$(symphony_binding "$binding")" \
    --arg issue_id "$issue_id" --arg issue_identifier "$issue_identifier" \
    --arg workspace_key "$workspace_key" --arg generation "$generation" \
    --arg path "$canonical_path" \
    '{protocol_version: 1, binding: $binding, issue_id: $issue_id,
      issue_identifier: $issue_identifier, workspace_key: $workspace_key,
      generation: $generation, path: $path}' >"$tmp_sidecar" \
    || fail_json state_unavailable true "cannot encode workspace metadata"
  mv "$tmp_sidecar" "$(sidecar_path "$canonical_path")" \
    || fail_json state_unavailable true "cannot install workspace metadata"

  jq -n --arg path "$canonical_path" --arg workspace_key "$workspace_key" \
    --arg issue_id "$issue_id" --arg issue_identifier "$issue_identifier" \
    --arg generation "$generation" --arg run_id "$run_id" \
    --argjson created "$(jq -c '.workspace.created_now == true' <<<"$result")" \
    --argjson binding "$(symphony_binding "$binding")" \
    --argjson receipt "$(jq -c '.receipt // {}' <<<"$result")" \
    '{protocol_version: 1,
      workspace: {path: $path, workspace_key: $workspace_key, created_now: $created},
      binding: $binding,
      metadata: {issue_id: $issue_id, issue_identifier: $issue_identifier,
                 generation: $generation, run_id: $run_id},
      receipt: $receipt}'
  exit 0
fi

workspace_path=$(string_field "$request" '.workspace_path') \
  || fail_json invalid_request false "checkpoint and archive need workspace_path"
if [[ -d "$workspace_path" ]]; then
  canonical_path=$(canonical_directory "$workspace_path") \
    || fail_json invalid_request false "workspace_path is inaccessible or unsafe"
  sidecar=$(sidecar_path "$canonical_path")
  [[ -f "$sidecar" && -r "$sidecar" && ! -L "$sidecar" ]] \
    || fail_json state_mismatch false "live workspace is missing safe backend metadata"
  # A directory is only evidence of a binding if the metadata inside it
  # names the same unit of work this request does. Checked here rather
  # than after the step, because a checkpoint publishes an immutable
  # object and records it against a change: pointing one attempt's
  # workspace at another attempt's request has to be refused while that
  # is still preventable.
  jq -e --arg issue_id "$issue_id" --arg generation "$generation" \
    --arg workspace_key "$workspace_key" \
    '.issue_id == $issue_id and .generation == $generation
     and .workspace_key == $workspace_key' "$sidecar" >/dev/null 2>&1 \
    || fail_json state_mismatch false "this workspace is bound to a different request"
  recorded=$(jq -ce '{change_id: .binding.change_id,
                      workspace_id: .binding.workspace_id}' "$sidecar" 2>/dev/null) \
    || fail_json state_mismatch false "workspace metadata is unreadable"
else
  [[ ! -e "$workspace_path" ]] \
    || fail_json invalid_request false "workspace_path exists but is not a directory"
  canonical_path=$workspace_path
  recorded=""
fi

if [[ "$operation" == "checkpoint" ]]; then
  [[ -d "$canonical_path" ]] \
    || fail_json workspace_missing false "checkpoint needs a live workspace"
  revision=$("$git_bin" -C "$canonical_path" rev-parse --verify 'HEAD^{commit}' 2>/dev/null) \
    || fail_json local_git false "workspace HEAD does not resolve to a commit"
  [[ "$revision" =~ ^([0-9a-fA-F]{40}|[0-9a-fA-F]{64})$ ]] \
    || fail_json local_git false "git returned an invalid checkpoint object id"
  # Choir records a revision it can serve, so the object has to exist on
  # the node before it is named. Pushing it under its own id keeps the
  # checkpoint immutable and independent of any branch that may move.
  if ! "$git_bin" -C "$canonical_path" push origin \
    "$revision:refs/choir/revisions/$revision" >/dev/null 2>&1; then
    fail_json git_push true "could not publish the immutable checkpoint object"
  fi
  run_runner "$(jq --arg base "$revision" '. + {base: $base}' \
    <<<"$(runner_request checkpoint)")"
else
  run_runner "$(runner_request archive)"
fi
result=$runner_result

# The Choir identifiers can only be compared afterwards, because
# deriving them is the runner's job and it does not do that without
# performing a step. The request fields were already checked above, so a
# difference here means the derivation rules changed under a workspace
# that was already in flight, and reporting that is worth more than
# leaving the two records silently disagreeing.
if [[ -n "$recorded" ]]; then
  actual=$(jq -c '{change_id: (.checkpoint.change_id // .archive.change_id),
                   workspace_id: (.checkpoint.workspace_id // .archive.workspace_id)}' \
    <<<"$result")
  jq -ne --argjson recorded "$recorded" --argjson actual "$actual" \
    '$recorded == $actual' >/dev/null \
    || fail_json state_mismatch false "workspace metadata does not match the binding Choir used"
fi

if [[ "$operation" == "checkpoint" ]]; then
  jq -c '{protocol_version: 1, checkpoint, receipt: (.receipt // {})}' <<<"$result"
  exit 0
fi

jq -c '{protocol_version: 1, archive, receipt: (.receipt // {})}' <<<"$result"
