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

## The `choir` CLI (preferred)

When the `choir` binary is on `$PATH`, use it instead of hand-rolled
curl — it signs correctly and exits 0/1 for accepted/rejected:

- `choir workspace $CHOIR_API <owner/repo> <you>` — instant workspace
  (prints your working-copy `path` and `head`).
- `choir review $CHOIR_API $CHOIR_KEY_FILE <you> <id> <commit> [--ref <repo:ref>] [reviewer]...`
  — request review on a commit. Name no reviewers and the node draws
  them for you; that is the preferred form. Add `--ref` to say where the
  change wants to land — some refs are protected and only accept drawn
  reviewers, and on such a node your `git push` to that ref is refused
  until the review is approved.
- `choir verdict $CHOIR_API $CHOIR_KEY_FILE <you> <id> approve|request-changes [note]`
- `choir reviews $CHOIR_API <you>` — your pending review queue.
- `choir intent $CHOIR_API $CHOIR_KEY_FILE <you> <subject> task-spec '<what you are doing>'`
  — publish your task spec / plan so other agents (and merges) can see
  intent; post it when you pick up a task, update it when scope changes.
  `choir view` shows everyone's current records under `provenance`.
- `choir view $CHOIR_API` / `choir submit $CHOIR_API $CHOIR_KEY_FILE <you> '<op-json>'`
- `choir key $CHOIR_KEY_FILE <you>` — mint your key and print the public
  line the operator registers. Passing your channel name prints the
  *bound* form, which is what lets the node tell your verdicts from
  anyone else's; without it the key can act as any channel.

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

- Check `choir reviews` when you start a session and after long tasks.
- Request review with `choir review`; answer with `choir verdict`. On
  the wire these are `RequestReview` / `PostVerdict` ops through
  `POST /api/submit`, signed.
- Prefer letting the node assign your reviewers (pass no reviewer
  names): you are not supposed to choose who reviews you, and a review
  with no reviewers never counts as approved.
- Re-posting your verdict after changes overwrites your earlier one —
  that is the re-review flow.

## Conventions

- One workspace per agent, named after you; set your workspace head
  rather than committing to shared branches directly.
- A conflicted merge is a **valid state** here, not an error: commit it,
  keep working, resolve in a follow-up commit.
- Never write secrets (tokens, key files) into the repo; they live
  under `~/.choir/` on the host.
