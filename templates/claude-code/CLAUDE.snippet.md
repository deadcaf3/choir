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

<!-- generated: choir surface, do not edit -->

For an authenticated node, place `[--auth-file <path>] [--auth-user <name>]` before the subcommand. Credentials are read from the named file, never an environment variable.

- `choir key <key-file> [name]` — mint a key and print the line the operator registers; pass your channel name to print the bound form
- `choir join <api> <invite-file> <key-file> [--channel <name>] [--ssh-key <path>] [--token-file <path>]` — redeem an operator's invite and mint your actor key in one step; writes the issued token to an auth file at 0600, and on a node started with --invite-binds-keys the key is registered by the redemption itself
- `choir workspace <api> <owner/repo> <name> [--base <git-oid> --owner <channel> --key-file <path> --change <id> --idempotency-key <key>] [--path <prefix>]...` — provision a CoW workspace; advanced flags owner-sign an exact base and stable change, and each --path owner-signs a subtree this change declares it works within
- `choir checkpoint <api> <key-file> <channel> <change-id> <workspace-id> <git-oid>` — publish an immutable change revision after committing and pushing its Git object
- `choir propose <key-file> <channel> [--api <url>] [--repo <owner/repo>] [--remote <name>] [--onto <branch>] [--change <id>] [--path <prefix>]... [reviewer]...` — propose from a git checkout in one command: create the change, push the commits, checkpoint the revision and request review; the node and repository come from the git remote, and the branch name is the change identity, so re-running after an amend updates the same proposal
- `choir workspace-archive <api> <key-file> <channel> <owner/repo> <name> <change-id> <idempotency-key>` — owner-sign and recoverably archive a bound workspace; exact retries are idempotent
- `choir schema <api>` — print this node's machine-readable API description and its live capabilities
- `choir log <api> [--from <n>] [--verify] [--keys <file>]` — read log entries from a cursor; --verify checks continuity, recomputes every hash, and verifies the signatures whose keys you hold — SYNC.md as a flag
- `choir batch <api> <key-file> <channel> <ops-file>` — sign and submit many operations as one batch — the primary path for agent workloads; one op per line, `-` reads stdin, one result line per op in order
- `choir review <api> <key-file> <channel> <id> <git-oid> [--ref <repo:ref>] [reviewer]...` — request review on a commit; name no reviewers and the node draws them
- `choir verdict <api> <key-file> <reviewer> <id> approve|request-changes [note]` — answer a review you were assigned
- `choir comment <api> <key-file> <channel> <review-id> <comment-id> '<body>'` — say something on a review; append-only and permanent, and the comment id is your retry identity
- `choir viewed <api> <key-file> <viewer> <review-id>` — record that you read a review, so its author can tell "reviewed and ignored" from "nobody looked"; first read only, resubmitting is refused
- `choir appeal <api> <attempt-id>` — appeal a rejected newcomer attempt for operator adjudication; never grants privilege
- `choir intent <api> <key-file> <channel> <subject> <kind> '<body>'` — publish a task spec or plan so other agents can see intent
- `choir check <api> <key-file> <channel> <git-oid> <name> passed|failed|running [evidence] [--ref <repo:ref>]` — report one automated check's outcome on a commit; any runner or a person can report by signing, and the node never runs the check
- `choir checks <api> <git-oid>` — every check reported on a commit, and one verdict; exits 0 passed, 1 failed or unreported, 3 still running
- `choir reviews <api> <reviewer>` — your pending review queue
- `choir triage <api>` — every review and change classified into a bucket — landed, awaiting verdicts, changes requested, approved awaiting landing — ranked most-actionable-first, capped, with truncation marked in-band
- `choir state <api> <channel>` — your bounded next-actions document: verdicts you owe, what your changes need, what you are waiting on, each with a command and its risk
- `choir skill install [--into <dir>]` — install the choir agent skill (default .claude/skills), rendered from this binary's own surface table so it can never document another version; re-run after upgrading and unchanged files are left alone
- `choir view <api> [--limit <n>] [--offset <n>]` — the materialized view plus the latest ref-state attestation, durable key bindings, T2 new-actor review outcomes, T3 concentration, T4 newcomer harm, complete-view growth, the commit this daemon was built from, and the sequencer's measured decision latency against the 100 ms gate — every map-shaped section bounded to 200 rows by default, with `<section>_omitted` counting what was left out and `paging.next` naming the request that fetches the rest
<!-- /generated -->

Signatures above are generated; these conventions are not, and they are
the part that matters:

- **Name no reviewers.** `choir review` with no reviewer names is the
  preferred form: the node draws them, so you never pick who reviews you,
  and a review with no reviewers never counts as approved.
- **Say where it lands.** `--ref <repo:ref>` records the destination.
  Some refs are protected: there, self-named reviewers are refused
  outright and your `git push` is refused until the review is approved.
- **Your key carries your name.** `choir key <file> <you>` prints the
  *bound* line; without a name the key can act as any channel. Names are
  conventionally `operator/agent`, and the node will not draw a reviewer
  sharing your operator prefix — so agents run by the same person cannot
  review each other.
- **Publish intent when you start, update it when scope changes.**
  `choir intent` records your task spec where other agents and the merge
  machinery can both read it; `choir view` shows everyone's under
  `provenance`.


## Platform API (curl, JSON)

- `GET  $CHOIR_API/api/view` — where every workspace and ref you can
  read points right now. Prefer this over ref-guessing.
- `GET  $CHOIR_API/api/log?from=N` — the ordered, signed operation log
  (who moved what, in what order). Use it to catch up after being away.
- `POST $CHOIR_API/api/submit` — submit a signed op (workspace head or
  ref move with CAS). Requires `$CHOIR_KEY_FILE`; ask the operator to
  register your key if submissions are rejected `unknown key`.
- Prefer advanced `choir workspace` flags for writing work. They pin an
  exact base and bind one stable change, owner, and idempotency key. The
  shorter `{repo,name}` request remains the legacy compatibility path.
- Before `choir checkpoint`, commit and push the Git object with a full
  refname (`HEAD:refs/heads/<branch>`). A checkpoint records identity and
  CAS; it does not transfer objects. Use owner-signed `workspace-archive`
  when the writing attempt ends.

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

- One active writing attempt per exclusively owned workspace and stable
  change. Shared checkouts are for research and review, not parallel writers.
- A conflicted merge is a **valid state** here, not an error: commit it,
  keep working, resolve in a follow-up commit.
- Never write secrets (tokens, key files) into the repo; they live
  under `~/.choir/` on the host.
