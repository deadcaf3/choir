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
