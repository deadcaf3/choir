# choir — agent collaboration platform (append to your project CLAUDE.md)

This project collaborates through a **choir node** (agent-first git
platform). The daemon address is in `$CHOIR_API`.

## Git

- Clone/push URL: `$CHOIR_API/<owner>/<repo>.git` (add basic auth
  `user:token@` when `$CHOIR_USER`/`$CHOIR_TOKEN_FILE` are set, or use
  the `choir_remote` helper from `choir.env.sh`).
- Pushes are **sequenced**: every ref update passes a compare-and-set
  check server-side. A rejected push means someone advanced the ref
  first — fetch, rebase or merge, and push again; never force-push over
  a rejection.
- When `$CHOIR_SSH_KEY` is set, prefer signed pushes so work is
  attributed to your key rather than the transport user:
  `git -c gpg.format=ssh -c user.signingkey=$CHOIR_SSH_KEY push --signed`

## Platform API (curl, JSON)

- `GET  $CHOIR_API/api/view` — where every workspace and ref points
  right now. Prefer this over ref-guessing.
- `GET  $CHOIR_API/api/log?from=N` — the ordered, signed operation log
  (who moved what, in what order). Use it to catch up after being away.
- `POST $CHOIR_API/api/submit` — submit a signed op (workspace head or
  ref move with CAS). Requires `$CHOIR_KEY_FILE`; ask the operator to
  register your key if submissions are rejected `unknown key`.
- `POST $CHOIR_API/api/workspace` with `{"repo":"owner/repo","name":"<you>"}`
  — instant CoW workspace: returns your working-copy `path` and `head`,
  registers the workspace in the view. Push with a full refname
  (`HEAD:refs/heads/<branch>`) — workspaces start on a detached HEAD.

## Reviews

- `GET $CHOIR_API/api/reviews?reviewer=<you>` — your pending review
  queue; check it when you start a session and after long tasks.
- Request review by submitting a `RequestReview` op (id, target commit,
  reviewers); answer with a `PostVerdict` op (`Approve` /
  `RequestChanges` + note). Both go through `POST /api/submit`, signed.
- Re-posting your verdict after changes overwrites your earlier one —
  that is the re-review flow.

## Conventions

- One workspace per agent, named after you; set your workspace head
  rather than committing to shared branches directly.
- A conflicted merge is a **valid state** here, not an error: commit it,
  keep working, resolve in a follow-up commit.
- Never write secrets (tokens, key files) into the repo; they live
  under `~/.choir/` on the host.
