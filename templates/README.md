# choir agent templates

These drop-in snippets teach an agent harness how to collaborate through a
choir node. Install one in a project and each session gets the same Git,
review, signing, and conflict-handling conventions.

## Install

All harnesses share one environment file:

1. Source [`choir.env.sh`](choir.env.sh) from your shell profile or harness
   environment, and set at least `CHOIR_API`.
2. For Git basic auth, set `CHOIR_USER` and a token-only
   `CHOIR_TOKEN_FILE`. For the `choir` CLI or MCP adapter, pass
   `--auth-file <path> --auth-user <name>` explicitly; the file uses the
   node's `user:token`-per-line format.
3. Set `CHOIR_KEY_FILE` for signed platform operations and `CHOIR_SSH_KEY`
   for signed pushes. Keep all credential and key files under `~/.choir/`
   at mode 0600, never in a repository.

Then per harness:

| Harness | File | Where it goes |
|---|---|---|
| Claude Code | [`claude-code/CLAUDE.snippet.md`](claude-code/CLAUDE.snippet.md) | append to the project's `CLAUDE.md` |
| Codex | [`codex/AGENTS.snippet.md`](codex/AGENTS.snippet.md) | append to the project's `AGENTS.md` |
| Cursor | [`cursor/choir.mdc`](cursor/choir.mdc) | copy to `.cursor/rules/choir.mdc` |

## Claude Code isolated workspaces

Claude Code can replace its Git worktree implementation with Choir by using
[`claude-code/choir-worktree.sh`](claude-code/choir-worktree.sh) for both
`WorktreeCreate` and `WorktreeRemove`. The adapter requires `bash`, `jq`, the
`choir` CLI, and a Choir node whose workspace paths are accessible on the
Claude host.

Install the script outside the repository so every checkout can use it:

```bash
install -d "$HOME/.choir/hooks"
install -m 0755 templates/claude-code/choir-worktree.sh \
  "$HOME/.choir/hooks/choir-worktree.sh"
```

Create a mode-0600 config such as `$HOME/.choir/claude-worktree.json`. It
contains identities and paths, never credential values:

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
into `.claude/settings.local.json`, then replace both paths with the installed
script and config paths. Merge the `hooks` entries with existing settings;
do not overwrite the file. `WorktreeCreate` and `WorktreeRemove` do not use
matchers.

Creation binds the caller checkout's exact `HEAD`, a deterministic
Claude-session workspace name, one owner, and one stable change. Exact hook
retries reuse the same workspace. Removal validates non-secret metadata under
the clone's `.git/` directory and calls owner-signed recoverable archive. It
does not commit, push, or checkpoint automatically.

Operational limits:

- The selected `HEAD` must already exist in the Choir bare repository.
- The node and Claude Code must share the filesystem path returned by Choir.
- A custom create hook replaces Claude's default Git behavior, including
  `.worktreeinclude` processing.
- Claude cannot be stopped by a failing `WorktreeRemove` hook. Check its debug
  log and retry the adapter manually when archive did not complete.

The input/output behavior follows the current
[Claude Code hooks reference](https://code.claude.com/docs/en/hooks#worktreecreate).

## Optional MCP adapter

Register `choir-mcp` as a stdio server if the harness supports MCP:

```text
choir-mcp <api> [--auth-file <path>] [--auth-user <name>]
```

For an authenticated node, use the node URL as `<api>` and name the credential
file with `--auth-file`; use `--auth-user` when it has multiple entries. The
adapter exposes seven public platform operations. It does not expose discovery
documents or the internal Git hook as tools.

## What the templates teach an agent

- The remote-URL shape and auth for git over the choir daemon.
- That pushes are sequenced (CAS): a rejection means integrate and
  retry, never force-push.
- Signed pushes (`gpg.format=ssh`) for per-key attribution when the
  agent has a key.
- The platform API: `/api/view` (current state), `/api/log` (ordered
  signed history), and `/api/submit-batch` (ordered signed operations).
- First-class conflicts: a conflicted merge is a valid committed state
  to build on, not an error to block on.

The generated command lists in these snippets come from
`crates/choir-cli/src/surface.rs`; refresh them with
`cargo run -p choir-cli --example gen-surface`. For protocol details, use
the [sync contract](../SYNC.md), [rejection-code catalog](../ERRORS.md), and
main [README](../README.md).
