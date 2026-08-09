#!/usr/bin/env bash
# Claude Code WorktreeCreate / WorktreeRemove adapter for choir.
# stdin and stdout follow Claude's hook contract. Diagnostics use stderr.

set -euo pipefail

usage() {
  echo "usage: choir-worktree.sh create|remove <config.json>" >&2
  exit 2
}

fail() {
  echo "choir-worktree: $1" >&2
  exit 1
}

safe_segment() {
  [[ "$1" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]]
}

json_field() {
  local document=$1
  local expression=$2
  jq -er "$expression | select(type == \"string\" and length > 0)" <<<"$document" 2>/dev/null
}

choir_error() {
  local response=$1
  local detail
  detail=$(jq -r '
    if type != "object" then empty
    elif (.code? | type) == "string" and (.detail? | type) == "string"
      then "\(.code): \(.detail)"
    elif (.error? | type) == "string" then .error
    else empty end
  ' <<<"$response" 2>/dev/null || true)
  if [[ -n "$detail" ]]; then
    fail "choir rejected the lifecycle request: $detail"
  fi
  fail "choir could not complete the lifecycle request"
}

[[ $# -eq 2 ]] || usage
mode=$1
config_file=$2
[[ "$mode" == "create" || "$mode" == "remove" ]] || usage
command -v jq >/dev/null 2>&1 || fail "jq is required"
[[ -r "$config_file" ]] || fail "cannot read the config file"

config=$(<"$config_file")
jq -e '
  type == "object" and
  ((keys - ["api", "auth_file", "auth_user", "choir_bin", "key_file", "owner", "repo"]) | length == 0)
' <<<"$config" >/dev/null 2>&1 || fail "config contains invalid JSON or unknown fields"

api=$(json_field "$config" '.api') || fail "config needs string api"
repo=$(json_field "$config" '.repo') || fail "config needs string repo"
owner=$(json_field "$config" '.owner') || fail "config needs string owner"
key_file=$(json_field "$config" '.key_file') || fail "config needs string key_file"
choir_bin=$(jq -er '(.choir_bin // "choir") | select(type == "string" and length > 0)' <<<"$config" 2>/dev/null) \
  || fail "config choir_bin must be a non-empty string"
auth_file=$(jq -er '(.auth_file // "") | select(type == "string")' <<<"$config" 2>/dev/null) \
  || fail "config auth_file must be a string"
auth_user=$(jq -er '(.auth_user // "") | select(type == "string")' <<<"$config" 2>/dev/null) \
  || fail "config auth_user must be a string"

[[ "$api" == http://* || "$api" == https://* ]] || fail "config api must use http:// or https://"
if [[ ! "$repo" =~ ^([^/]+)/([^/]+)$ ]]; then
  fail "config repo must be two safe path segments: owner/repo"
fi
repo_owner=${BASH_REMATCH[1]}
repo_name=${BASH_REMATCH[2]}
safe_segment "$repo_owner" && safe_segment "$repo_name" \
  || fail "config repo must be two safe path segments: owner/repo"
[[ -z "$auth_user" || -n "$auth_file" ]] || fail "config auth_user needs auth_file"
[[ "$key_file" == /* ]] || fail "config key_file must be absolute"
[[ -z "$auth_file" || "$auth_file" == /* ]] || fail "config auth_file must be absolute"

auth_args=()
if [[ -n "$auth_file" ]]; then
  auth_args+=(--auth-file "$auth_file")
fi
if [[ -n "$auth_user" ]]; then
  auth_args+=(--auth-user "$auth_user")
fi

input=$(cat)
event=$(json_field "$input" '.hook_event_name') || fail "hook input needs string hook_event_name"

if [[ "$mode" == "create" ]]; then
  [[ "$event" == "WorktreeCreate" ]] || fail "create expects a WorktreeCreate event"
  suggested=$(json_field "$input" '.name') || fail "WorktreeCreate input needs string name"
  session_id=$(json_field "$input" '.session_id') || fail "WorktreeCreate input needs string session_id"
  cwd=$(json_field "$input" '.cwd') || fail "WorktreeCreate input needs string cwd"
  safe_segment "$suggested" || fail "WorktreeCreate name is not a safe path segment"
  safe_segment "$session_id" || fail "WorktreeCreate session_id is not a safe path segment"
  [[ ${#suggested} -le 100 && ${#session_id} -le 100 ]] \
    || fail "WorktreeCreate name or session id is too long"
  [[ "$cwd" == /* && -d "$cwd" && ! -L "$cwd" ]] \
    || fail "WorktreeCreate cwd must be an absolute, non-symlink directory"

  workspace_name="cc-${suggested}-${session_id}"
  workspace_id="$repo/$workspace_name"
  change_id="claude-code:$repo:$workspace_name"
  idempotency_key="claude-code-create:$repo:$workspace_name"
  base=$(git -C "$cwd" rev-parse --verify 'HEAD^{commit}' 2>/dev/null) \
    || fail "WorktreeCreate cwd has no resolvable Git HEAD commit"
  [[ "$base" =~ ^([0-9a-fA-F]{40}|[0-9a-fA-F]{64})$ ]] \
    || fail "git returned a non-full object id"

  response=""
  if ! response=$("$choir_bin" "${auth_args[@]}" workspace "$api" "$repo" "$workspace_name" \
    --base "$base" --owner "$owner" --change "$change_id" \
    --idempotency-key "$idempotency_key" 2>/dev/null); then
    choir_error "$response"
  fi
  path=$(json_field "$response" '.path') || fail "choir returned no workspace path"
  returned_workspace=$(json_field "$response" '.workspace') \
    || fail "choir returned no workspace identity"
  returned_change=$(json_field "$response" '.change_id') \
    || fail "choir returned no change identity"
  [[ "$returned_workspace" == "$workspace_id" && "$returned_change" == "$change_id" ]] \
    || fail "choir returned a different workspace binding"
  [[ "$path" == /* && -d "$path" && ! -L "$path" ]] \
    || fail "choir returned an inaccessible or unsafe workspace path"
  canonical_path=$(cd "$path" && pwd -P)
  [[ "$canonical_path" == "$path" ]] || fail "choir returned a symlinked workspace path"
  [[ -d "$canonical_path/.git" && ! -L "$canonical_path/.git" ]] \
    || fail "choir workspace has no standalone Git metadata"

  sidecar_dir="$canonical_path/.git/choir"
  sidecar="$sidecar_dir/claude-worktree.json"
  [[ ! -e "$sidecar_dir" || ! -L "$sidecar_dir" ]] \
    || fail "hook metadata directory must not be a symlink"
  umask 077
  mkdir -p "$sidecar_dir" || fail "cannot create hook metadata directory"
  tmp_sidecar=$(mktemp "$sidecar_dir/.claude-worktree.XXXXXX") \
    || fail "cannot create temporary hook metadata"
  jq -n \
    --arg repo "$repo" \
    --arg owner "$owner" \
    --arg name "$workspace_name" \
    --arg workspace "$workspace_id" \
    --arg change "$change_id" \
    --arg idempotency_key "$idempotency_key" \
    --arg path "$canonical_path" \
    '{repo: $repo, owner: $owner, name: $name, workspace: $workspace,
      change: $change, idempotency_key: $idempotency_key, path: $path}' \
    >"$tmp_sidecar" || fail "cannot write hook metadata"
  mv "$tmp_sidecar" "$sidecar" || fail "cannot install hook metadata"

  printf '%s\n' "$canonical_path"
  exit 0
fi

[[ "$event" == "WorktreeRemove" ]] || fail "remove expects a WorktreeRemove event"
worktree_path=$(json_field "$input" '.worktree_path') \
  || fail "WorktreeRemove input needs string worktree_path"
[[ "$worktree_path" == /* && ! -L "$worktree_path" ]] \
  || fail "WorktreeRemove worktree_path must be absolute and not a symlink"
workspace_name=${worktree_path##*/}
safe_segment "$workspace_name" || fail "WorktreeRemove path has an unsafe final component"
[[ "$workspace_name" == cc-* ]] || fail "worktree was not created by the choir adapter"

workspace_id="$repo/$workspace_name"
change_id="claude-code:$repo:$workspace_name"
idempotency_key="claude-code-create:$repo:$workspace_name"
if [[ -d "$worktree_path" ]]; then
  canonical_path=$(cd "$worktree_path" && pwd -P)
  [[ "$canonical_path" == "$worktree_path" ]] || fail "WorktreeRemove path is symlinked"
  sidecar="$canonical_path/.git/choir/claude-worktree.json"
  [[ -f "$sidecar" && -r "$sidecar" && ! -L "$sidecar" ]] \
    || fail "live worktree is missing safe choir hook metadata"
  jq -e \
    --arg repo "$repo" \
    --arg owner "$owner" \
    --arg name "$workspace_name" \
    --arg workspace "$workspace_id" \
    --arg change "$change_id" \
    --arg idempotency_key "$idempotency_key" \
    --arg path "$canonical_path" \
    '.repo == $repo and .owner == $owner and .name == $name and
     .workspace == $workspace and .change == $change and
     .idempotency_key == $idempotency_key and .path == $path' \
    "$sidecar" >/dev/null 2>&1 || fail "choir hook metadata does not match this worktree"
elif [[ -e "$worktree_path" ]]; then
  fail "WorktreeRemove path exists but is not a directory"
fi

[[ -f "$key_file" && -r "$key_file" && ! -L "$key_file" ]] \
  || fail "owner key file is missing, unreadable, or symlinked"
key_size=$(wc -c <"$key_file" | tr -d '[:space:]')
[[ "$key_size" == "32" ]] || fail "owner key file must contain exactly 32 bytes"

response=""
if ! response=$("$choir_bin" "${auth_args[@]}" workspace-archive "$api" "$key_file" \
  "$owner" "$repo" "$workspace_name" "$change_id" "$idempotency_key" 2>/dev/null); then
  choir_error "$response"
fi
returned_workspace=$(json_field "$response" '.workspace') \
  || fail "choir returned no archived workspace identity"
returned_change=$(json_field "$response" '.change_id') \
  || fail "choir returned no archived change identity"
[[ "$returned_workspace" == "$workspace_id" && "$returned_change" == "$change_id" ]] \
  || fail "choir archived a different workspace binding"

# WorktreeRemove has no decision output. Silence is the successful result.
exit 0
