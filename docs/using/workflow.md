# The contribution workflow

One change, from an empty workspace to a landed ref. Every step is a signed
operation the node put in a total order, so the same sequence works for one
agent or fifty.

| Step | Command | What it records |
|:--|:--|:--|
| 1 | `choir workspace` | a copy-on-write workspace bound to an exact base commit |
| 2 | `choir intent` | what this change is trying to do, before it exists |
| 3 | `choir checkpoint` | an immutable revision of the change |
| 4 | `choir review` | a request for verdicts on an exact commit |
| 5 | `choir verdict` | one reviewer's answer, signed by their own key |
| 6 | landing | the ref moves, carrying the basis that admitted it (D43) |

`choir propose` collapses steps 1, 3 and 4 into one command when you are
already in a git checkout. `choir state` answers "what should I do next"
from the node's own view rather than from memory.

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

**Review rules that matter in practice**

- Name **no** reviewers on `choir review`; empty list ⇒ node assignment. Self-picked lists may be refused under `--require-assignment` / protected refs.
- Channel names: `operator/agent`. Same-operator agents cannot review each other.
- Bind keys when registering: `choir key ~/.choir/agent.key myop/agent >> ~/.choir/keys`.
- Protected landing with `--require-review` needs approval weight **2** (two distinct operators), unless somebody holds `own` over the repository, in which case one owner's assent lands it and nothing else does (D42). See `scripts/flip/RUNBOOK.md` to enable gates on the dogfood node.
- Operators can invalidate a bad approval with `choir slash`; it lowers future approval weight and marks re-review required, but never rewrites an already-landed ref.
- An optional reviewer conflict graph excludes operators within the configured hop distance from the requester. It is re-read per draw and fails closed by leaving the review unassigned.
- `choir view` reports T3 concentration using exact counts and integer shares. Active branches mean last attributable mover, and protected updates mean admitted ref updates under the current policy; unknown and ambiguous attribution stay visible and make the overall status `indeterminate` rather than a pass.
- `choir view` also reports `view_growth`: record counts and compact JSON bytes for workspaces, refs, reviews, and provenance. `total_authoritative_view` covers exactly those four sections and excludes runtime projections. This measures complete-view growth; it does not prune or expire anything.
- `choir view` reports `newcomer_harm` when the operator enables the two 0600 audit files. A rejected signed-API newcomer can run `choir appeal <api> <attempt-id>`; the appeal requests separate operator adjudication and never grants privilege. Thresholds stay unset until the first real adoption-gate measurement.
- Prefer `POST /api/submit-batch` for multiple ops (one durability barrier).

Optional forge follower / speculative GitHub queue: `choir-bridge`; see the crate docs (`cargo doc -p choir-bridge`).

Bridge utility modes mint or inspect its identity (`--pubkey`), inspect GitHub App installations (`app-debug`), exercise one commit-status write (`post-status`), replay existing merge commits for offline D23 calibration (`calibrate`), and mine a mirrored foreign history for real semantic-conflict specimens (`harvest`, D27: offline, no forge access, landing policy untouched). Queue mode can optionally run the advisory three-worktree D23 detector; it never changes the landing condition. Grant only the permissions in the [bridge permission model](../../crates/choir-bridge/PERMISSIONS.md); `queue --land` is the only routine mode that needs contents write access.

## Agent templates

Teach Claude Code / Codex / Cursor to speak choir: see [`templates/README.md`](../../templates/README.md).
That guide also includes a tested Claude Code `WorktreeCreate` and
`WorktreeRemove` adapter for Choir-backed isolated sessions.

```bash
source templates/choir.env.sh   # sets CHOIR_API; optional user/token/key
# then install the harness snippet listed in templates/README.md
```
