#!/usr/bin/env bash
# External workspace backend for a Symphony workspace-manager seam.
# One versioned JSON request enters on stdin. One JSON result leaves on stdout.

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

safe_segment() {
  [[ "$1" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]]
}

validate_key_file() {
  [[ -f "$key_file" && -r "$key_file" && ! -L "$key_file" ]] \
    || fail_json invalid_config false "owner key file is missing, unreadable, or symlinked"
  local key_size
  key_size=$(wc -c <"$key_file" | tr -d '[:space:]')
  [[ "$key_size" == "32" ]] \
    || fail_json invalid_config false "owner key file must contain exactly 32 bytes"
}

choir_failure() {
  local response=$1
  local context=$2
  local code detail retryable
  code=$(jq -r 'if type == "object" and (.code? | type) == "string" then .code else "" end' \
    <<<"$response" 2>/dev/null || true)
  detail=$(jq -r '
    if type != "object" then empty
    elif (.detail? | type) == "string" then .detail
    elif (.error? | type) == "string" then .error
    else empty end
  ' <<<"$response" 2>/dev/null || true)
  [[ -n "$code" ]] || code="choir_unavailable"
  [[ -n "$detail" ]] || detail="choir did not complete $context"
  case "$code" in
    workspace_state|stale_head|malformed_request|malformed_op|unknown_key|channel_not_owned)
      retryable=false
      ;;
    *)
      retryable=true
      ;;
  esac
  fail_json "$code" "$retryable" "$detail"
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

config=$(<"$config_file")
jq -e '
  type == "object" and
  ((keys - ["api", "auth_file", "auth_user", "base_ref", "choir_bin", "git_bin",
            "key_file", "owner", "repo", "state_dir"]) | length == 0)
' <<<"$config" >/dev/null 2>&1 \
  || fail_json invalid_config false "config contains invalid JSON or unknown fields"

api=$(string_field "$config" '.api') || fail_json invalid_config false "config needs string api"
repo=$(string_field "$config" '.repo') || fail_json invalid_config false "config needs string repo"
owner=$(string_field "$config" '.owner') || fail_json invalid_config false "config needs string owner"
key_file=$(string_field "$config" '.key_file') \
  || fail_json invalid_config false "config needs string key_file"
base_ref=$(string_field "$config" '.base_ref') \
  || fail_json invalid_config false "config needs string base_ref"
state_dir=$(string_field "$config" '.state_dir') \
  || fail_json invalid_config false "config needs string state_dir"
choir_bin=$(jq -er '(.choir_bin // "choir") | select(type == "string" and length > 0)' \
  <<<"$config" 2>/dev/null) || fail_json invalid_config false "config choir_bin is invalid"
git_bin=$(jq -er '(.git_bin // "git") | select(type == "string" and length > 0)' \
  <<<"$config" 2>/dev/null) || fail_json invalid_config false "config git_bin is invalid"
auth_file=$(jq -er '(.auth_file // "") | select(type == "string")' \
  <<<"$config" 2>/dev/null) || fail_json invalid_config false "config auth_file is invalid"
auth_user=$(jq -er '(.auth_user // "") | select(type == "string")' \
  <<<"$config" 2>/dev/null) || fail_json invalid_config false "config auth_user is invalid"

[[ "$api" == http://* || "$api" == https://* ]] \
  || fail_json invalid_config false "config api must use http:// or https://"
if [[ ! "$repo" =~ ^([^/]+)/([^/]+)$ ]]; then
  fail_json invalid_config false "config repo must be owner/repo"
fi
repo_owner=${BASH_REMATCH[1]}
repo_name=${BASH_REMATCH[2]}
safe_segment "$repo_owner" && safe_segment "$repo_name" \
  || fail_json invalid_config false "config repo contains an unsafe path segment"
[[ "$key_file" == /* && "$state_dir" == /* ]] \
  || fail_json invalid_config false "config key_file and state_dir must be absolute"
[[ -z "$auth_file" || "$auth_file" == /* ]] \
  || fail_json invalid_config false "config auth_file must be absolute"
[[ -z "$auth_user" || -n "$auth_file" ]] \
  || fail_json invalid_config false "config auth_user needs auth_file"
[[ "$base_ref" == "$repo.git:refs/"* ]] \
  || fail_json invalid_config false "config base_ref must name this repository as owner/repo.git:refs/..."
validate_key_file

run_choir() {
  if [[ -n "$auth_user" ]]; then
    "$choir_bin" --auth-file "$auth_file" --auth-user "$auth_user" "$@"
  elif [[ -n "$auth_file" ]]; then
    "$choir_bin" --auth-file "$auth_file" "$@"
  else
    "$choir_bin" "$@"
  fi
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
safe_segment "$workspace_key" && [[ ${#workspace_key} -le 160 ]] \
  || fail_json invalid_request false "workspace_key is unsafe or too long"
[[ ${#issue_id} -le 512 && ${#issue_identifier} -le 512 && ${#generation} -le 512 ]] \
  || fail_json invalid_request false "issue or generation identity is too long"

fingerprint=$(printf '%s:%s\n%s:%s\n%s:%s\n' \
  "${#repo}" "$repo" "${#issue_id}" "$issue_id" "${#generation}" "$generation" \
  | "$git_bin" hash-object --stdin 2>/dev/null) \
  || fail_json local_git false "git could not derive the stable binding hash"
[[ "$fingerprint" =~ ^[0-9a-fA-F]{40}$ ]] \
  || fail_json local_git false "git returned an invalid binding hash"
workspace_prefix=${workspace_key:0:80}
workspace_name="sy-${workspace_prefix}-${fingerprint:0:16}"
workspace_id="$repo/$workspace_name"
change_id="symphony:$repo:$fingerprint"
idempotency_key="symphony-create:$repo:$fingerprint"
state_file="$state_dir/$fingerprint.json"

mkdir -p "$state_dir" || fail_json state_unavailable true "cannot create adapter state directory"
[[ ! -L "$state_dir" ]] || fail_json invalid_config false "adapter state directory must not be a symlink"

if [[ ! -f "$state_file" ]]; then
  lock_dir="$state_file.lock"
  if ! mkdir "$lock_dir" 2>/dev/null; then
    [[ -f "$state_file" ]] \
      || fail_json state_busy true "another scheduler is reserving this change generation"
  else
    trap 'rmdir "$lock_dir" 2>/dev/null || true' EXIT
    view=""
    if ! view=$(run_choir view "$api" 2>/dev/null); then
      choir_failure "$view" "base revision lookup"
    fi
    revision=$(jq -er --arg ref "$base_ref" '.refs[$ref] | select(type == "string")' \
      <<<"$view" 2>/dev/null) \
      || fail_json base_ref_missing false "configured base_ref is absent from the Choir view"
    case "$revision" in
      11-*) base=${revision#11-} ;;
      12-*) base=${revision#12-} ;;
      *) fail_json base_ref_invalid false "configured base_ref has a non-Git revision" ;;
    esac
    [[ "$base" =~ ^([0-9a-fA-F]{40}|[0-9a-fA-F]{64})$ ]] \
      || fail_json base_ref_invalid false "configured base_ref has an invalid Git object id"
    umask 077
    tmp_state=$(mktemp "$state_dir/.symphony-state.XXXXXX") \
      || fail_json state_unavailable true "cannot create adapter state"
    jq -n \
      --arg repo "$repo" --arg owner "$owner" --arg issue_id "$issue_id" \
      --arg issue_identifier "$issue_identifier" --arg workspace_key "$workspace_key" \
      --arg generation "$generation" --arg workspace_name "$workspace_name" \
      --arg workspace_id "$workspace_id" --arg change_id "$change_id" \
      --arg idempotency_key "$idempotency_key" --arg base "$base" \
      '{protocol_version: 1, repo: $repo, owner: $owner, issue_id: $issue_id,
        issue_identifier: $issue_identifier, workspace_key: $workspace_key,
        generation: $generation, workspace_name: $workspace_name,
        workspace_id: $workspace_id, change_id: $change_id,
        idempotency_key: $idempotency_key, base: $base}' >"$tmp_state" \
      || fail_json state_unavailable true "cannot encode adapter state"
    mv "$tmp_state" "$state_file" || fail_json state_unavailable true "cannot install adapter state"
    rmdir "$lock_dir" || fail_json state_unavailable true "cannot release adapter state lock"
    trap - EXIT
  fi
fi

[[ -f "$state_file" && -r "$state_file" && ! -L "$state_file" ]] \
  || fail_json state_unavailable true "adapter state file is missing, unreadable, or symlinked"
state=$(<"$state_file")
jq -e \
  --arg repo "$repo" --arg owner "$owner" --arg issue_id "$issue_id" \
  --arg issue_identifier "$issue_identifier" --arg workspace_key "$workspace_key" \
  --arg generation "$generation" --arg workspace_name "$workspace_name" \
  --arg workspace_id "$workspace_id" --arg change_id "$change_id" \
  --arg idempotency_key "$idempotency_key" \
  '.protocol_version == 1 and .repo == $repo and .owner == $owner and
   .issue_id == $issue_id and .issue_identifier == $issue_identifier and
   .workspace_key == $workspace_key and .generation == $generation and
   .workspace_name == $workspace_name and .workspace_id == $workspace_id and
   .change_id == $change_id and .idempotency_key == $idempotency_key and
   (.base | type) == "string"' <<<"$state" >/dev/null 2>&1 \
  || fail_json state_mismatch false "durable adapter state does not match this request"
base=$(string_field "$state" '.base') \
  || fail_json state_mismatch false "durable adapter state has no base revision"

if [[ "$operation" == "ensure" ]]; then
  response=""
  if ! response=$(run_choir workspace "$api" "$repo" "$workspace_name" \
    --base "$base" --owner "$owner" --key-file "$key_file" --change "$change_id" \
    --idempotency-key "$idempotency_key" 2>/dev/null); then
    choir_failure "$response" "workspace creation"
  fi
  path=$(string_field "$response" '.path') \
    || fail_json invalid_response true "Choir returned no workspace path"
  returned_workspace=$(string_field "$response" '.workspace') \
    || fail_json invalid_response true "Choir returned no workspace identity"
  returned_change=$(string_field "$response" '.change_id') \
    || fail_json invalid_response true "Choir returned no change identity"
  [[ "$returned_workspace" == "$workspace_id" && "$returned_change" == "$change_id" ]] \
    || fail_json binding_mismatch false "Choir returned a different workspace binding"
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
  jq --arg path "$canonical_path" '. + {path: $path}' <<<"$state" >"$tmp_sidecar" \
    || fail_json state_unavailable true "cannot encode workspace metadata"
  mv "$tmp_sidecar" "$sidecar_dir/symphony-backend.json" \
    || fail_json state_unavailable true "cannot install workspace metadata"
  created=$(jq -e '.created == true' <<<"$response" >/dev/null 2>&1 && echo true || echo false)
  receipt=$(jq -c '.operation // {}' <<<"$response" 2>/dev/null) \
    || fail_json invalid_response true "Choir returned invalid creation JSON"
  jq -n \
    --arg path "$canonical_path" --arg workspace_key "$workspace_key" \
    --arg repo "$repo" --arg workspace_id "$workspace_id" --arg change_id "$change_id" \
    --arg idempotency_key "$idempotency_key" --arg owner "$owner" --arg base "$base" \
    --arg issue_id "$issue_id" --arg issue_identifier "$issue_identifier" \
    --arg generation "$generation" --arg run_id "$run_id" \
    --argjson created "$created" --argjson receipt "$receipt" \
    '{protocol_version: 1,
      workspace: {path: $path, workspace_key: $workspace_key, created_now: $created},
      binding: {repo: $repo, workspace_id: $workspace_id, change_id: $change_id,
                idempotency_key: $idempotency_key, owner: $owner, base: $base},
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
  sidecar="$canonical_path/.git/choir/symphony-backend.json"
  [[ -f "$sidecar" && -r "$sidecar" && ! -L "$sidecar" ]] \
    || fail_json state_mismatch false "live workspace is missing safe backend metadata"
  cmp -s "$state_file" <(jq 'del(.path)' "$sidecar") \
    || fail_json state_mismatch false "workspace metadata does not match durable adapter state"
else
  [[ ! -e "$workspace_path" ]] \
    || fail_json invalid_request false "workspace_path exists but is not a directory"
  canonical_path=$workspace_path
fi

if [[ "$operation" == "checkpoint" ]]; then
  [[ -d "$canonical_path" ]] \
    || fail_json workspace_missing false "checkpoint needs a live workspace"
  revision=$("$git_bin" -C "$canonical_path" rev-parse --verify 'HEAD^{commit}' 2>/dev/null) \
    || fail_json local_git false "workspace HEAD does not resolve to a commit"
  [[ "$revision" =~ ^([0-9a-fA-F]{40}|[0-9a-fA-F]{64})$ ]] \
    || fail_json local_git false "git returned an invalid checkpoint object id"
  if ! "$git_bin" -C "$canonical_path" push origin \
    "$revision:refs/choir/revisions/$revision" >/dev/null 2>&1; then
    fail_json git_push true "could not publish the immutable checkpoint object"
  fi
  response=""
  if ! response=$(run_choir checkpoint "$api" "$key_file" "$owner" \
    "$change_id" "$workspace_id" "$revision" 2>/dev/null); then
    choir_failure "$response" "revision checkpoint"
  fi
  receipt=$(jq -c 'if type == "object" then . else {} end' <<<"$response" 2>/dev/null) \
    || fail_json invalid_response true "Choir returned invalid checkpoint JSON"
  jq -n --arg revision "$revision" --arg change_id "$change_id" \
    --arg workspace_id "$workspace_id" --argjson receipt "$receipt" \
    '{protocol_version: 1, checkpoint: {change_id: $change_id,
      workspace_id: $workspace_id, revision_id: $revision}, receipt: $receipt}'
  exit 0
fi

response=""
if ! response=$(run_choir workspace-archive "$api" "$key_file" \
  "$owner" "$repo" "$workspace_name" "$change_id" "$idempotency_key" 2>/dev/null); then
  choir_failure "$response" "workspace archive"
fi
returned_workspace=$(string_field "$response" '.workspace') \
  || fail_json invalid_response true "Choir returned no archived workspace identity"
returned_change=$(string_field "$response" '.change_id') \
  || fail_json invalid_response true "Choir returned no archived change identity"
archived_path=$(string_field "$response" '.archived_path') \
  || fail_json invalid_response true "Choir returned no archived path"
[[ "$returned_workspace" == "$workspace_id" && "$returned_change" == "$change_id" ]] \
  || fail_json binding_mismatch false "Choir archived a different workspace binding"
already_archived=$(jq -e '.already_archived == true' <<<"$response" >/dev/null 2>&1 \
  && echo true || echo false)
receipt=$(jq -c '.operation // {}' <<<"$response" 2>/dev/null) \
  || fail_json invalid_response true "Choir returned invalid archive JSON"
jq -n --arg archived_path "$archived_path" --arg workspace_id "$workspace_id" \
  --arg change_id "$change_id" --argjson already_archived "$already_archived" \
  --argjson receipt "$receipt" \
  '{protocol_version: 1, archive: {workspace_id: $workspace_id,
    change_id: $change_id, archived_path: $archived_path,
    already_archived: $already_archived}, receipt: $receipt}'
