# choir agent templates (D21 adoption lever)

Drop-in configuration that makes an agent harness a choir client. The
premise: agents adopt infrastructure through their config files far
faster than humans switch forges, so the templates ARE the adoption
surface — install once, and every session in that harness knows how to
collaborate through a choir node.

## Install

All harnesses share one environment file:

1. `source templates/choir.env.sh` from your shell profile (or your
   harness's env mechanism), setting at minimum `CHOIR_API`.
2. Optional, per agent: `CHOIR_USER` + `CHOIR_TOKEN_FILE` (basic auth),
   `CHOIR_KEY_FILE` (platform-API signing), `CHOIR_SSH_KEY` (signed
   pushes). Keys live under `~/.choir/`, never in a repo.

Then per harness:

| Harness | File | Where it goes |
|---|---|---|
| Claude Code | `claude-code/CLAUDE.snippet.md` | append to the project's `CLAUDE.md` |
| Codex | `codex/AGENTS.snippet.md` | append to the project's `AGENTS.md` |
| Cursor | `cursor/choir.mdc` | copy to `.cursor/rules/choir.mdc` |

## What the templates teach an agent

- The remote-URL shape and auth for git over the choir daemon.
- That pushes are sequenced (CAS): a rejection means integrate and
  retry, never force-push.
- Signed pushes (`gpg.format=ssh`) for per-key attribution when the
  agent has a key.
- The platform API: `/api/view` (current state), `/api/log` (ordered
  signed history), `/api/submit` (signed ops).
- First-class conflicts: a conflicted merge is a valid committed state
  to build on, not an error to block on.

Keep the snippets short: they are conventions, not documentation. The
daemon's rustdoc is the reference.
