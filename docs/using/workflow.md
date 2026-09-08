# The contribution workflow

One change, from an empty workspace to a landed ref. Every step is a signed
operation in the node's total order.

| Step | Command | What it records |
|:--|:--|:--|
| 1 | `choir workspace` | a copy-on-write workspace bound to an exact base commit |
| 2 | `choir intent` | what this change is trying to do |
| 3 | `choir checkpoint` | an immutable revision of the change |
| 4 | `choir review` | a request for verdicts on an exact commit |
| 5 | `choir verdict` | one reviewer's signed answer |
| 6 | landing | the ref moves, carrying the basis that admitted it (D43) |

`choir propose` does steps 1, 3 and 4 from a git checkout. `choir state`
answers "what next" from the node's view.

## Minimal day-one loop

```bash
API=http://127.0.0.1:8417
A=(--auth-file "$HOME/.choir/auth" --auth-user choir)
OWNER=myop/agent
CHANGE=change-1
WORKSPACE=owner/demo/agent-a

# 1. Confirm the node
choir "${A[@]}" view "$API"

# 2. Exact-base CoW workspace and stable change
choir "${A[@]}" workspace "$API" owner/demo agent-a \
  --base "$(git rev-parse HEAD)" --owner "$OWNER" --change "$CHANGE" \
  --idempotency-key request-1

# 3. Publish intent. After editing, commit and push before checkpointing.
choir "${A[@]}" intent "$API" "$HOME/.choir/agent.key" "$OWNER" "$CHANGE" task 'ship feature X'
git push origin HEAD:refs/heads/agent-a
choir "${A[@]}" checkpoint "$API" "$HOME/.choir/agent.key" "$OWNER" \
  "$CHANGE" "$WORKSPACE" "$(git rev-parse HEAD)"

# 4. Request review with no reviewer names so the node draws them
choir "${A[@]}" review "$API" "$HOME/.choir/agent.key" "$OWNER" rev-1 "$(git rev-parse HEAD)" \
  --ref owner/demo.git:refs/heads/main

# 5. Drawn reviewers answer
choir "${A[@]}" reviews "$API" otherop/reviewer
choir "${A[@]}" verdict "$API" "$HOME/.choir/other.key" otherop/reviewer rev-1 approve
```

**Review rules**

- Name no reviewers on `choir review`; the node draws them. Self-picked
  lists may be refused under `--require-assignment` or protected refs.
- Channel names are `operator/agent`. Same-operator agents cannot review
  each other.
- Register keys with `choir key ~/.choir/agent.key myop/agent >> ~/.choir/keys`.
- Protected landing with `--require-review` needs approval weight **2**
  (two operators), unless somebody holds `own`, in which case one owner's
  assent lands it (D42). `scripts/flip/RUNBOOK.md` enables the gates.
- `choir slash` invalidates a bad approval and requires re-review; it never
  rewrites a landed ref.
- An optional reviewer conflict graph excludes operators within a hop
  distance of the requester; it fails closed by leaving the review
  unassigned.
- `choir view` reports T3 concentration with exact counts; unknown
  attribution makes the status `indeterminate`.
- `choir view` reports `view_growth`; `total_authoritative_view` covers
  workspaces, refs, reviews and provenance and excludes runtime projections.
- `choir view` reports `newcomer_harm` when the two 0600 audit files are
  enabled. A rejected newcomer can `choir appeal <api> <attempt-id>`; an
  appeal never grants privilege.
- Prefer `POST /api/submit-batch` for several ops (one durability barrier).

Forge follower and speculative GitHub queue: `choir-bridge`
(`cargo doc -p choir-bridge`). Utility modes: `--pubkey`, `app-debug`,
`post-status`, `calibrate`, `harvest` (D27, offline). Queue mode can run the
advisory D23 detector; it never changes the landing condition. Grant only the
[bridge permission model](../../crates/choir-bridge/PERMISSIONS.md);
`queue --land` alone needs contents write.

## Agent templates

Snippets for Claude Code, Codex and Cursor, plus a tested Claude Code
`WorktreeCreate`/`WorktreeRemove` adapter:
[`templates/README.md`](../../templates/README.md).

```bash
source templates/choir.env.sh   # sets CHOIR_API; optional user/token/key
# then install the harness snippet listed in templates/README.md
```
