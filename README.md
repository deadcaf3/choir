# choir

Agent-first code collaboration: many agents on one repo, one total order from a single-writer sequencer, merge conflicts as first-class values.

**Status:** research prototype. Phase-0 gate passed; Phase 1 in progress. No CI — run the gate by hand. Not production software.

| You want to… | Start here |
|---|---|
| See it work once | [Try it](#try-it) |
| Install tools / unblock build | [Prerequisites](#prerequisites) |
| Run a local node | [Run a node](#run-a-node) |
| Push, review, provision workspaces | [Use the node](#use-the-node) |
| Wire coding agents | [Agent templates](#agent-templates) |
| Design / numbers / invariants | [`internal/`](internal/design.md) |

## Prerequisites

| Tool | Required? | Notes |
|---|---|---|
| Rust stable + Cargo | **yes** | Built with **1.97.1**, edition 2021. Install via [rustup](https://rustup.rs/) or Homebrew. |
| `git` | **yes** | Smart-HTTP CGI + all integration tests. |
| `curl` | **yes** | Only HTTP client the crates use. |
| `openssl` | **yes** | Auth tokens, bridge RS256; tests shell out to it. |
| `ssh-keygen` | **yes** | Integration tests / signed-push setup. |
| `mergiraf` | optional | Structured merge slot. Without it, line merge + first-class conflicts still work. Homebrew: `brew install mergiraf`. |
| `jj` | optional | Not required to build or run choir. |

**OS / filesystem**

- **macOS (APFS):** supported. Fast CoW workspaces via `clonefile` / `cp -Rc`. Dogfood installer uses **launchd** (`scripts/choirctl`).
- **Linux:** supported. Prefer a **btrfs** volume for workspace snapshots (Phase-0 gate used btrfs). Without CoW, provisioning still works but is slower.
- **Non-loopback bind** requires TLS (`--tls-cert` + `--tls-key`). Plain HTTP is loopback-only by design.

**Do not remove** `.cargo/config.toml` (`LIBSQLITE3_FLAGS`). Every workspace build needs it (rivetkit / sqlite workaround).

**Secrets:** keys, tokens, PEMs under `~/.choir/` at mode `0600` (daemon key: `<repo-root>/.choir/node.key`). Never commit them.

## Build

```bash
git clone <this-repo> && cd choir
cargo build --release -p choir-node -p choir-cli
```

Default `cargo build` / `cargo test` skip `choir-actor` (heavy Rivet dep). Full gate:

```bash
cargo test --workspace
cargo clippy --workspace --all-targets
cargo run -p choir-spike --release
```

Put the CLI on your PATH (or use `cargo run -p choir-cli -- …`):

```bash
export PATH="$PWD/target/release:$PATH"
```

## Try it

```bash
cargo run -p choir-demo         # narrated walkthrough: keys, ops, conflict, real git push
cargo run -p choir-spike --release   # Phase-0 gate binary; nonzero exit = gate fail
cargo test --workspace          # hermetic: no network, no external services
```

`choir-demo` is the fastest “what is this?” path.

## Run a node

The daemon serves **git smart-HTTP** and the **platform API** on one port (default **8417**). Repos must be created with `--create` (or the installer) so the `pre-receive` hook is installed — a bare repo made any other way is **not** sequenced. With no arguments, the binary uses `./repos` and port 8417; configured invocations must supply both `<repo-root>` and `<port>` before any flags.

### Option A — macOS dogfood (supervised)

```bash
sh scripts/choirctl install              # build, mint ~/.choir secrets, load launchd
sh scripts/choirctl status
sh scripts/choirctl url                  # clone/push URL with credentials
sh scripts/choirctl logs
# sh scripts/choirctl stop | uninstall   # stop keeps data under ~/.choir
```

Override port with `CHOIR_PORT`. Full flip procedure: `scripts/flip/RUNBOOK.md`.

### Option B — any Unix (foreground)

```bash
mkdir -p /tmp/choir-repos ~/.choir
# user:token per line, mode 0600
printf 'choir:%s\n' "$(openssl rand -hex 32)" > ~/.choir/auth && chmod 600 ~/.choir/auth
: > ~/.choir/keys && chmod 600 ~/.choir/keys
cargo run -p choir-cli -- key ~/.choir/agent.key myop/agent >> ~/.choir/keys
printf '# reviewer channels, one per line\n' > ~/.choir/reviewers && chmod 600 ~/.choir/reviewers

cargo run -p choir-node -- /tmp/choir-repos 8417 \
  --create owner/demo.git \
  --auth-file ~/.choir/auth \
  --keys-file ~/.choir/keys \
  --reviewers-file ~/.choir/reviewers
```

Useful flags: `--bind`, `--tls-cert` / `--tls-key`, `--require-assignment`, `--protected-refs <file>`, `--require-review`, `--review-retention <count>`, and `--review-lapse-after-secs <seconds>`. Flag reference: module docs at the top of `crates/choir-node/src/main.rs`, or `agents.md`.

**File formats (all mode 0600)**

| File | Format |
|---|---|
| `--auth-file` | `user:token` per line |
| `--keys-file` | `<64-hex>` or `<channel> <64-hex>` (bound key) |
| `--reviewers-file` | channel name per line; re-read on each draw |
| `--protected-refs` | `owner/repo.git:refs/heads/main` (trailing `*` ok) |

Hot-reload: trusted keys, channel bindings, push-certificate signers, and reviewers take effect on the next request.

Review retention is opt-in. `--review-retention N` archives completed reviews when more than `N` remain live. Incomplete reviews never lapse unless `--review-lapse-after-secs` is also set; that flag is invalid without a retention count.

## Use the node

Auth on the CLI is flags, not env:

```bash
choir --auth-file ~/.choir/auth --auth-user choir <command> ...
```

Exit codes: **0** accepted, **1** rejected (JSON body printed — see `ERRORS.md`), **2** usage.

The signed-operation API is the primary agent path: it carries actor identity and batches many operations behind one durability barrier. `git push` remains the compatibility and bulk-transfer path.

### Git compatibility path

```bash
# after choirctl install:
git clone "$(sh scripts/choirctl url owner/repo.git)"
# or manually:
# git clone http://choir:<token>@127.0.0.1:8417/owner/demo.git

git push origin HEAD:main
```

Pushes are CAS-sequenced. On rejection: fetch, rebase/merge, push again — **never force-push** over a sequencer rejection.

### Signed-operation CLI and API (primary agent path)

<!-- generated: choir surface, do not edit -->

#### HTTP endpoints

| Endpoint | Purpose |
|---|---|
| `POST /api/submit` | Submit one signed operation (hex payload, hex signature) |
| `POST /api/submit-batch` | Same, in array order; the primary path for agent workloads (throughput figures live in PHASE0.md, not here, so they cannot go stale) |
| `GET /api/view` | The materialized view: changes, workspace heads, refs, reviews, provenance |
| `GET /api/log?from=N` | Ordered log entries, the catch-up and sync primitive. Absolute `from`: entries evicted from the in-memory window are served from the persisted log (`source` says which), and a node that cannot reach that far back answers 409 rather than a page with a hole in it. Each entry carries its hash, parent and author signature so pages can be chained and verified without trusting the node; SYNC.md is that procedure |
| `POST /api/workspace` | Provision a CoW workspace; optional exact base/change binding makes retries idempotent |
| `POST /api/workspace/archive` | Recoverably archive a change-bound workspace and remove it from the active view |
| `GET /api/reviews?reviewer=X` | One actor's pending review queue |
| `GET /llms.txt` | This surface, as text, for an agent that has never seen choir |
| `GET /sync.md` | The sync contract, in full: cursor semantics and how to verify a page's hash chain and author signatures without trusting the node serving them |
| `POST /api/git-update` | Internal: the pre-receive hook callback |

#### The `choir` CLI

```text
usage:
  choir [--auth-file <path>] [--auth-user <name>] <command> ...

commands:
  choir key <key-file> [name]
  choir workspace <api> <owner/repo> <name> [--base <git-oid> --owner <channel> --key-file <path> --change <id> --idempotency-key <key>]
  choir checkpoint <api> <key-file> <channel> <change-id> <workspace-id> <git-oid>
  choir workspace-archive <api> <key-file> <channel> <owner/repo> <name> <change-id> <idempotency-key>
  choir submit <api> <key-file> <channel> '<op-json>'
  choir review <api> <key-file> <channel> <id> <git-oid> [--ref <repo:ref>] [reviewer]...
  choir verdict <api> <key-file> <reviewer> <id> approve|request-changes [note]
  choir intent <api> <key-file> <channel> <subject> <kind> '<body>'
  choir reviews <api> <reviewer>
  choir view <api>

Exit codes: 0 accepted, 1 the node rejected (its JSON error body is printed), 2 usage error.
```
<!-- /generated -->

Live surface on a running node: `GET /llms.txt`. Sync verification: `SYNC.md` / `GET /sync.md`.

### MCP adapter

For MCP clients, run the synchronous stdio adapter. It maps generated tools onto the same HTTP endpoints and owns no second implementation or session state.

```bash
choir-mcp http://127.0.0.1:8417 --auth-file ~/.choir/auth --auth-user choir
```

It serves the measured legacy handshakes and the stateless 2026-07-28 request path. Tool order and schemas come from `crates/choir-cli/src/surface.rs`.

### Minimal day-one loop

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
- Protected landing with `--require-review` needs approval weight **2** (two distinct operators). See `scripts/flip/RUNBOOK.md` to enable gates on the dogfood node.
- Prefer `POST /api/submit-batch` for multiple ops (one durability barrier).

Optional forge follower / speculative GitHub queue: `choir-bridge` — see [`internal/design.md`](internal/design.md#bridge).

Bridge utility modes mint or inspect its identity (`--pubkey`), inspect GitHub App installations (`app-debug`), and exercise one commit-status write (`post-status`). Grant only the permissions in the [bridge permission model](crates/choir-bridge/PERMISSIONS.md); `queue --land` is the only routine mode that needs contents write access.

### Agent templates

Teach Claude Code / Codex / Cursor to speak choir: see [`templates/README.md`](templates/README.md).
That guide also includes a tested Claude Code `WorktreeCreate` and
`WorktreeRemove` adapter for Choir-backed isolated sessions.

```bash
source templates/choir.env.sh   # sets CHOIR_API; optional user/token/key
# then install the harness snippet listed in templates/README.md
```

## Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| `cargo build` pulls huge tree / sqlite errors | `choir-actor` / rivetkit | Keep `.cargo/config.toml`. Default members already exclude actor; use `-p choir-actor` only when needed. |
| `choir-actor` ignored test fails / download broken | rivetkit 2.3.10 auto-download | Workarounds in `PHASE0.md`. Run: `RIVETKIT_ENGINE_AUTO_DOWNLOAD=1 cargo test -p choir-actor -- --ignored` |
| Node refuses bind address | Non-loopback without TLS | Add `--tls-cert` / `--tls-key`, or stay on `127.0.0.1` / SSH tunnel |
| `/api/view` → 401 | Auth enabled (expected) | Pass `-u user:token` or `--auth-file` / `--auth-user` |
| Push not in `/api/view` | Repo created without `--create` | Recreate via node/`choirctl` so `pre-receive` exists |
| `unknown_key` | Key not in `--keys-file` | `choir key … [channel] >> keys-file` (hot-reloaded) |
| `stale_head` | CAS lost the race | Re-read `/api/view`, rebase on `actual`, resubmit |
| `assignment_error` / empty reviewers | Empty `--reviewers-file` | Add at least two `operator/…` channels for meaningful review |
| `review_required` | Protected ref, insufficient weight | Node-drawn review + two operators approve, then push |
| Workspace slow / fails | No CoW FS | Use APFS or btrfs; see `internal/measurements.md` |
| Lost submit response | Network blip after accept | Resubmit **identical** signed bytes → `already_applied: true` (`ERRORS.md`) |

Rejection code table: [`ERRORS.md`](ERRORS.md).

## Docs map

| Doc | What it is |
|---|---|
| [`agents.md`](agents.md) | Agent-facing surface (generated; edit `crates/choir-cli/src/surface.rs`) |
| [`ERRORS.md`](ERRORS.md) | Rejection codes and repair hints |
| [`SYNC.md`](SYNC.md) | Log catch-up + hash/signature verification |
| [`templates/`](templates/README.md) | Drop-in agent harness snippets |
| [`scripts/choirctl`](scripts/choirctl) | Dogfood node operator entrypoint |
| [`scripts/flip/RUNBOOK.md`](scripts/flip/RUNBOOK.md) | Supervised install + protected-ref gates |
| [`PHASE0.md`](PHASE0.md) | Build log and gate status (source of truth) |
| [`plan.md`](plan.md) | Design blueprint + decision register |
| [`internal/design.md`](internal/design.md) | Architecture, invariants, conventions |
| [`internal/integration-workflows.md`](internal/integration-workflows.md) | Agent workflow targets, identifier contract, implementation order |
| [`internal/measurements.md`](internal/measurements.md) | Phase-0 numbers |

## License

Workspace crates: `MIT OR Apache-2.0` (declared in the manifest; license files not yet in-tree). Mergiraf (optional subprocess) is GPLv3 and is never linked.
