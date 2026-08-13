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

### Option A2 — serving beyond loopback (TLS)

A non-loopback bind requires TLS (invariant 9), so going public is a certificate step, not a flag you can just add. On a Linux node host:

```bash
sh scripts/flip/setup_tls.sh <your.domain> [port]   # certbot + renewal hook + marker
sh scripts/flip/install_node_linux.sh <port> '' ~/bin   # re-render the unit
```

`setup_tls.sh` writes `~/.choir/tls.enabled` (cert path, then key path). That marker is what flips the rendered unit to `--bind 0.0.0.0 --tls-cert … --tls-key …`; delete it and reinstall to go back to loopback. Keep inbound **80** open permanently — renewals rebind it — and your serving port open too.

Auth stays mandatory when public: anonymous requests get **401** on both the API and git, and a browser opening the URL gets a login prompt. That is the expected state, not a misconfiguration. Give each additional person their own line in `~/.choir/auth`:

```bash
printf 'alice:%s\n' "$(openssl rand -hex 32)" >> ~/.choir/auth   # hand the token over out of band
```

On its own that token grants read and write on **every** repository the node serves (see the warning under file formats). Pair it with an `--acl-file` line before handing it over, or you are giving full node access.

Operator scripts follow the public name automatically if you put it in an untracked `~/.choir-public-url`; without that file they use the loopback tunnel. Details and the operator checklist: `scripts/flip/RUNBOOK.md`.

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

Useful flags: `--bind`, `--tls-cert` / `--tls-key`, `--acl-file <file>` (required before a second credential), `--require-assignment`, `--protected-refs <file>`, `--require-review`, `--reviewer-conflict-graph <file>` with `--reviewer-conflict-distance <hops>`, `--review-retention <count>`, and `--review-lapse-after-secs <seconds>`. Flag reference: module docs at the top of `crates/choir-node/src/main.rs`, or `agents.md`.

**File formats (all mode 0600)**

| File | Format |
|---|---|
| `--auth-file` | `user:token` per line — authentication only; pair with `--acl-file` |
| `--acl-file` | `<user> <repo\|*\|@node> <level>` per line; levels `read` < `write`, and `auditor` on `@node` |
| `--keys-file` | `<64-hex>` or `<channel> <64-hex>` (bound key) |
| `--reviewers-file` | channel name per line; re-read on each draw |
| `--protected-refs` | `owner/repo.git:refs/heads/main` (trailing `*` ok) |
| `--reviewer-conflict-graph` | undirected `operator operator` edges; pair with an explicit maximum hop distance |

Hot-reload: trusted keys, channel bindings, push-certificate signers, reviewers, and ACL grants take effect on the next request.

> **Without `--acl-file`, every credential reaches every repository.** The auth file authenticates and nothing else; the node prints a line saying so at startup. A second `user:token` line can then clone every repo, push to any unprotected ref, and provision workspaces anywhere. Protected refs and the review requirement still hold, so it cannot land on a gated `main` unreviewed. **Do not issue a second credential without an ACL.**

### Per-repository authorization (D29)

`--acl-file` gates each repository per user. Three whitespace-separated columns, `#` comments, and the same append-a-line discipline as the keys file:

```
# <user>   <repo|*|@node>   <level>
alice      owner/demo       write
bob        owner/demo       read
bob        owner/notes      write
carol      *                read
dave       @node            auditor
```

`read` clones and fetches; `write` adds push, workspace provisioning, and submitting ops that touch that repository. There is no `admin`: no endpoint performs a repository-scoped administrative action, since repo creation and ref protection are operator-side flags.

The operator's own credential usually wants two lines, since neither covers the other:

```
myself     *      write
myself     @node  write
```

`*` covers every repository and never covers `@node`. `@node` is the node itself: `auditor` reads `/api/log` and `/api/ref-agreement`, which are gated rather than filtered because the log is a hash chain and the attestation covers the complete ref state. `@node write` is needed for ops that name no repository, such as key bindings.

Fail closed: with the flag set, anything not granted is refused. A repository you cannot read answers `404` rather than `403`, so a denial never confirms that it exists. The flag requires `--auth-file` — an ACL over anonymous requests would grade everyone the same. A malformed file refuses to start; a malformed *edit* keeps the previous table and complains, so a typo cannot silently revoke access.

`/api/view`, `/api/reviews` and the browser page are narrowed to the repositories a credential may read, so a grant on one repository does not disclose that the others exist. Node-wide sections of the view (the ref-state attestation, key bindings, and the concentration, growth, newcomer and lag telemetry) need `@node auditor`; the log head and build stamp reach everyone, since a writer needs them to submit. A review you were assigned to still reaches you, on any repository — that is what an invitation is.

Review retention is opt-in. `--review-retention N` archives completed reviews when more than `N` remain live. Incomplete reviews never lapse unless `--review-lapse-after-secs` is also set; that flag is invalid without a retention count.

### Webhooks: something landed, go run this (D32)

`--hooks-file` posts to a URL you name whenever a ref you name moves. One subscription per line, `#` comments, and the same append-a-line discipline as every other policy file:

```
# <repo:refname pattern>        <url>                        <secret>   [allow-private]
owner/demo:refs/heads/main      https://ci.example/choir      <SECRET>
owner/demo:refs/heads/*         https://ci.example/branches   <SECRET>
owner/notes:refs/tags/*         http://127.0.0.1:9000/hook    <SECRET>   allow-private
```

Patterns are the `--protected-refs` grammar: a trailing `*` is a prefix, anything else is exact, and the string matched is the view's `<repo>:<refname>` key. The file is re-read when its mtime moves, so adding a subscription is appending a line. It needs `--keys-file`, since refs reach the log through the platform sequencer. Delivery records go to `<repo-root>/.choir/hooks.jsonl`.

The body says what moved, and nothing else:

```json
{"format_version":1,"event":"ref-landed","repo":"owner/demo","ref":"refs/heads/main",
 "ref_key":"owner/demo:refs/heads/main","old":"<git oid>","new":"<git oid>","seq":41,
 "entry":"<entry hash>","actor":"<channel>","key_id":"<signing key id>"}
```

`old` is null for a created ref and `new` is null for a deleted one. `entry` is the log entry's content hash: it is unique per event, so a receiver that has already acted on one can discard a repeat.

**Verify the secret.** The delivery carries `X-Choir-Hook-Secret: <secret>`, the secret on that subscription's line, and a receiver should compare it before acting — otherwise anything that can reach the URL can pretend to be your node. It is a bearer secret rather than a signature over the body, so give each subscription its own (`openssl rand -hex 32`) and keep the file mode 0600. A non-loopback target must therefore be `https`; the node refuses to send the secret in clear.

**Best-effort, never silent.** Three attempts per delivery, and every attempt, every refusal and every dropped event is a JSON line in `hooks.jsonl`. Deliveries are *not* at-least-once: a webhook runs on its own thread behind a bounded queue, and when a receiver is slower than the node produces refs, events are dropped and counted rather than allowed to delay op admission. A receiver that may not miss a ref polls `GET /api/log?from=N` instead, which is what agents already do for catch-up.

**Targets are vetted.** This is the node's only outbound request to an address someone else chose, so it refuses loopback, private, carrier-NAT, link-local (including the `169.254.169.254` metadata service), unique-local and unspecified addresses unless the line ends in `allow-private`; it connects to the address it vetted rather than re-resolving the name; and it follows no redirects.

## Use the node

Auth on the CLI is flags, not env:

```bash
choir --auth-file ~/.choir/auth --auth-user choir <command> ...
```

Exit codes: **0** accepted, **1** rejected (JSON body printed — see `ERRORS.md`), **2** usage.

The signed-operation API is the primary agent path: it carries actor identity and batches many operations behind one durability barrier. `git push` remains the compatibility and bulk-transfer path.

### Browser surface

Open the node's base URL (`/`) in a browser and it serves one read-only page: refs grouped by repository, the review queue with approval weights and verdicts, the latest ref-state attestation, workspaces, and sequencer health against the 100 ms gate. It is behind the same auth wall as everything else, so a browser prompts for a `--auth-file` user and token — anonymous readers get `401`, on the page exactly as on the API.

It is deliberately not an app. The page is server-rendered from the same `/api/view` payload the API serves (so it cannot drift from the API), cached by view sequence, and revalidated with an `ETag` — a repeat visit on unchanged state returns `304` with no body, so refreshing or polling it costs the node nothing. No JavaScript, no build step, no external fetch, so it works offline and inside networks with no route to the internet.

Writes are not available from the browser and are not planned without their own decision: every write still goes through the signed-operation API.

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
| `GET /api/view` | The materialized view plus the latest ref-state attestation, durable key bindings, T2 new-actor review outcomes, T3 concentration, T4 newcomer harm, complete-view growth, the commit this daemon was built from, and the sequencer's measured decision latency against the 100 ms gate. On a node running an ACL you are served your own slice: the repositories your credential may read, plus reviews you were assigned to; the node-wide sections need a node-wide grant. A repository missing from the response is one you were not granted, not one that is gone |
| `POST /api/appeal` | Record an appeal for a rejected newcomer attempt; it requests operator adjudication and never changes privilege |
| `GET /api/log?from=N` | Ordered log entries, the catch-up and sync primitive. Absolute `from`: entries evicted from the in-memory window are served from the persisted log (`source` says which), and a node that cannot reach that far back answers 409 rather than a page with a hole in it. Each entry carries its hash, parent and author signature so pages can be chained and verified without trusting the node; SYNC.md is that procedure |
| `POST /api/workspace` | Provision a copy-on-write workspace and register it in the view |
| `GET /api/reviews?reviewer=X` | One actor's pending review queue |
| `GET /llms.txt` | This surface, as text, for an agent that has never seen choir |
| `GET /sync.md` | The sync contract, in full: cursor semantics and how to verify a page's hash chain and author signatures without trusting the node serving them |
| `GET /api/ref-agreement` | Where the op log and the bare repos disagree about a ref, read-only |
| `POST /api/git-update` | Internal: the pre-receive hook callback |
| `POST /api/git-abort` | Internal: retracts a refused push's already-accepted refs |

#### The `choir` CLI

```text
usage:
  choir [--auth-file <path>] [--auth-user <name>] <command> ...

commands:
  choir key <key-file> [name]
  choir workspace <api> <owner/repo> <name>
  choir submit <api> <key-file> <channel> '<op-json>'
  choir review <api> <key-file> <channel> <id> <git-oid> [--ref <repo:ref>] [reviewer]...
  choir verdict <api> <key-file> <reviewer> <id> approve|request-changes [note]
  choir slash <api> <node-key-file> <id> <reviewer> '<reason>'
  choir abandon <api> <node-key-file> <id>
  choir bind <api> <node-key-file> <operator> <key-hex> [channel]
  choir revoke <api> <node-key-file> <key-hex> '<reason>'
  choir appeal <api> <attempt-id>
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

# 1. Confirm the node
choir "${A[@]}" view "$API"

# 2. CoW workspace (repo must exist on the node with at least one commit)
choir "${A[@]}" workspace "$API" owner/demo agent-a

# 3. Publish intent, then request review (name no reviewers — node draws)
choir "${A[@]}" intent "$API" "$HOME/.choir/agent.key" myop/agent HEAD task 'ship feature X'
choir "${A[@]}" review "$API" "$HOME/.choir/agent.key" myop/agent rev-1 "$(git rev-parse HEAD)" \
  --ref owner/demo.git:refs/heads/main

# 4. Drawn reviewers answer
choir "${A[@]}" reviews "$API" otherop/reviewer
choir "${A[@]}" verdict "$API" "$HOME/.choir/other.key" otherop/reviewer rev-1 approve
```

**Review rules that matter in practice**

- Name **no** reviewers on `choir review`; empty list ⇒ node assignment. Self-picked lists may be refused under `--require-assignment` / protected refs.
- Channel names: `operator/agent`. Same-operator agents cannot review each other.
- Bind keys when registering: `choir key ~/.choir/agent.key myop/agent >> ~/.choir/keys`.
- Protected landing with `--require-review` needs approval weight **2** (two distinct operators). See `scripts/flip/RUNBOOK.md` to enable gates on the dogfood node.
- Operators can invalidate a bad approval with `choir slash`; it lowers future approval weight and marks re-review required, but never rewrites an already-landed ref.
- An optional reviewer conflict graph excludes operators within the configured hop distance from the requester. It is re-read per draw and fails closed by leaving the review unassigned.
- `choir view` reports T3 concentration using exact counts and integer shares. Active branches mean last attributable mover, and protected updates mean admitted ref updates under the current policy; unknown and ambiguous attribution stay visible and make the overall status `indeterminate` rather than a pass.
- `choir view` also reports `view_growth`: record counts and compact JSON bytes for workspaces, refs, reviews, and provenance. `total_authoritative_view` covers exactly those four sections and excludes runtime projections. This measures complete-view growth; it does not prune or expire anything.
- `choir view` reports `newcomer_harm` when the operator enables the two 0600 audit files. A rejected signed-API newcomer can run `choir appeal <api> <attempt-id>`; the appeal requests separate operator adjudication and never grants privilege. Thresholds stay unset until the first real adoption-gate measurement.
- Prefer `POST /api/submit-batch` for multiple ops (one durability barrier).

Optional forge follower / speculative GitHub queue: `choir-bridge` — see [`internal/design.md`](internal/design.md#bridge).

Bridge utility modes mint or inspect its identity (`--pubkey`), inspect GitHub App installations (`app-debug`), exercise one commit-status write (`post-status`), replay existing merge commits for offline D23 calibration (`calibrate`), and mine a mirrored foreign history for real semantic-conflict specimens (`harvest`, D27 — offline, no forge access, landing policy untouched). Queue mode can optionally run the advisory three-worktree D23 detector documented in `internal/design.md`; it never changes the landing condition. Grant only the permissions in the [bridge permission model](crates/choir-bridge/PERMISSIONS.md); `queue --land` is the only routine mode that needs contents write access.

### Agent templates

Teach Claude Code / Codex / Cursor to speak choir: see [`templates/README.md`](templates/README.md).

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
| Browser asks for a username/password | Auth is mandatory on every endpoint, including public TLS binds | Enter a user and token from `--auth-file`. Nothing is served anonymously by design |
| Push not in `/api/view` | Repo created without `--create` | Recreate via node/`choirctl` so `pre-receive` exists |
| `unknown_key` | Key not in `--keys-file` | `choir key … [channel] >> keys-file` (hot-reloaded) |
| `bad_signature` | Signature does not cover the bytes sent; key **is** trusted | Re-sign the exact `(channel, payload)`. Registering a key does not help. Unexpected → someone replayed a signature |
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
| [`internal/measurements.md`](internal/measurements.md) | Phase-0 numbers |

## License

Workspace crates: `MIT OR Apache-2.0` (declared in the manifest; license files not yet in-tree). Mergiraf (optional subprocess) is GPLv3 and is never linked.
