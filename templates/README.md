# choir agent templates

Snippets that teach an agent harness a choir node's conventions.

## Install

One environment file for all harnesses:

1. Source [`choir.env.sh`](choir.env.sh); set at least `CHOIR_API`.
2. Git basic auth: `CHOIR_USER` and a token-only `CHOIR_TOKEN_FILE`. The
   `choir` CLI and MCP adapter take `--auth-file <path> --auth-user <name>`
   explicitly; the file is `user:token` per line.
3. `CHOIR_KEY_FILE` for signed platform operations, `CHOIR_SSH_KEY` for
   signed pushes. Keep them under `~/.choir/` at mode 0600, never in a
   repository.

Then per harness:

| Harness | File | Where it goes |
|---|---|---|
| Claude Code | [`claude-code/CLAUDE.snippet.md`](claude-code/CLAUDE.snippet.md) | append to the project's `CLAUDE.md` |
| Codex | [`codex/AGENTS.snippet.md`](codex/AGENTS.snippet.md) | append to the project's `AGENTS.md` |
| Cursor | [`cursor/choir.mdc`](cursor/choir.mdc) | copy to `.cursor/rules/choir.mdc` |

## Symphony workspace backend

[`symphony/choir-workspace-backend.sh`](symphony/choir-workspace-backend.sh)
implements a versioned external backend contract: exact-base create/reuse,
strict revision checkpoint, recoverable archive. Pinned contract and
workspace-manager seam: [`Symphony integration guide`](symphony/README.md).

Do not wire it through Symphony's current workspace hooks: they ignore
`before_remove` failures and delete the directory.

## Claude Code isolated workspaces

[`claude-code/choir-worktree.sh`](claude-code/choir-worktree.sh) replaces
Claude Code's Git worktrees for `WorktreeCreate` and `WorktreeRemove`.
Requires `bash`, `jq` and the `choir` CLI.

Install outside the repository:

```bash
install -d "$HOME/.choir/hooks"
install -m 0755 templates/claude-code/choir-worktree.sh \
  "$HOME/.choir/hooks/choir-worktree.sh"
```

Create a mode-0600 config such as `$HOME/.choir/claude-worktree.json`,
identities and paths only:

```json
{
  "api": "http://127.0.0.1:8417",
  "repo": "owner/repo",
  "owner": "operator/claude",
  "key_file": "/absolute/path/to/claude.key",
  "auth_file": "/absolute/path/to/auth",
  "auth_user": "choir",
  "choir_bin": "/absolute/path/to/choir"
}
```

Copy the hook entries from
[`claude-code/settings.worktree.example.json`](claude-code/settings.worktree.example.json)
into `.claude/settings.local.json`, replacing both paths. Merge with
existing `hooks`; do not overwrite. Neither hook takes a matcher.

Creation binds the checkout's exact `HEAD` to one owner and one change;
retries reuse the workspace. Removal validates non-secret metadata under
the clone's `.git/`, then archives (owner-signed, recoverable). Nothing
commits, pushes or checkpoints.

Limits:

- The selected `HEAD` must exist in the Choir bare repository.
- The node and Claude Code must share the path Choir returns.
- A custom create hook replaces Claude's default Git behavior, including
  `.worktreeinclude`.
- A failing `WorktreeRemove` hook does not stop Claude. Check its debug
  log and rerun the adapter by hand.

Input/output follows the
[Claude Code hooks reference](https://code.claude.com/docs/en/hooks#worktreecreate).

## Optional MCP adapter

Register `choir-mcp` as a stdio server:

```text
choir-mcp <api> [--auth-file <path>] [--auth-user <name>]
```

`--auth-user` picks an entry when the file has several. Seven platform
operations; no discovery documents or Git hook.

## What the templates teach an agent

- Remote-URL shape and auth for git over the daemon.
- Pushes are sequenced (CAS): on rejection, integrate and retry, never
  force-push.
- Signed pushes (`gpg.format=ssh`) for per-key attribution.
- `/api/view` (state), `/api/log` (signed history), `/api/submit-batch`
  (signed operations).
- A conflicted merge is a valid committed state.

Command lists come from `crates/choir-cli/src/surface.rs`; refresh with
`cargo run -p choir-cli --example gen-surface`. Protocol:
[sync contract](../SYNC.md), [rejection-code catalog](../ERRORS.md),
main [README](../README.md).
