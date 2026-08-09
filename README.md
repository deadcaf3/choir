# choir

An agent-first code collaboration platform: many agents working concurrently on one repository, ordered by a single-writer sequencer, with first-class merge conflicts.

Git forges today assume human tempo: a handful of contributors, a handful of branches, merges that are rare and negotiated. Coding agents invert that. Published measurements put the conflict rate on agent-authored pull requests between 15 and 32 percent, and even after a clean structural merge a 5 to 10 percent residual semantic-conflict rate remains. choir is a forge built for that regime instead of retrofitted to it.

**Status: research prototype.** The Phase-0 feasibility gate passed on the Linux target; Phase 1 is in progress. Nothing here is production software, there is no CI, and the gate is run by hand.

For reference material, see the [sync contract](SYNC.md), [rejection-code catalog](ERRORS.md), [agent templates](templates/README.md), and [bridge permission model](crates/choir-bridge/PERMISSIONS.md). The [design blueprint](plan.md) and [Phase-0 evidence log](PHASE0.md) preserve the reasoning and measurements behind the implementation.

## The four ideas

1. **One writer, one order.** Every change to a repository, whether it arrives as a signed API operation or as a `git push`, is serialized by a single-writer sequencer thread into one total order. The result is a hash-chained, per-actor-signed operation log that any reader can replay and verify independently.
2. **A conflict is a value, never a failure.** A conflicted tree entry is a legal committed state that child commits build on top of. A merge strategy that cannot resolve returns a conflict rather than picking a side. There is no silent auto-resolve anywhere in the codebase.
3. **Speculative merge trains.** A Zuul-style queue assumes every change will pass, tests them in parallel, and evicts the failure. Window sizing follows TCP flow control: start at 20, add one per success, halve on failure.
4. **Workspaces in milliseconds.** Agent workspaces are provisioned by a single tree-level copy-on-write clone (btrfs subvolume snapshot or APFS `clonefile`), measured at p50 4 ms on the Linux target, so starting an agent costs nothing worth budgeting for.

## Quick start

```bash
cargo test --workspace          # full suite, hermetic: no network, no external services
cargo run -p choir-demo         # narrated walkthrough of every layer, including a real git push
cargo run -p choir-spike --release   # the Phase-0 gate as a binary; exits nonzero on gate failure
```

`choir-demo` is the fastest way to see what this is. It mints keys, submits signed operations, shows a rejection, commits a first-class conflict, undoes work by prefix replay, and pushes through the daemon over real git, narrating each step.

Requirements: stable Rust and Cargo. The repository currently builds with Rust 1.97.1 and uses edition 2021, but does not declare a minimum supported Rust version. Keep `git`, `curl`, `openssl`, and `ssh-keygen` on `PATH`; integration tests drive them as real subprocesses.

## Running a node

```bash
cargo run -p choir-node -- <repo-root> <port> [--create owner/name.git]... \
  [--auth-file f] [--keys-file f] [--reviewers-file f] [--protected-refs f] \
  [--require-assignment] [--require-review] [--review-retention count] \
  [--review-lapse-after-secs seconds] [--bind addr] \
  [--tls-cert c --tls-key k]
```

With no arguments, the binary defaults to `./repos` on port 8417. Pass the port explicitly for any configured invocation; the current positional parser starts reading flags after that slot.

The daemon serves git over smart-HTTP (via `git http-backend` as CGI) and a platform API on the same port. HTTP basic auth is mandatory when an auth file is given, and the node refuses a non-loopback bind without TLS: that is the privacy rule expressed as code, not as a note.

Repositories created by the daemon get a `pre-receive` hook that calls back into the API, so a `git push` becomes a node-signed ref operation in the same total order as everything else. A repository created any other way is not sequenced.

Operator files are line-oriented and should be mode 0600 outside the repository: `--auth-file` uses `user:token`; `--keys-file` uses `<public-key-hex>` or `<channel> <public-key-hex>`; `--reviewers-file` uses one channel per line; and `--protected-refs` uses one `<repo>:<refname>` pattern per line, with an optional trailing `*`. `--require-assignment` and `--protected-refs` need a reviewer file. `--require-review` also needs protected refs and requires approval weight from two distinct operators before a protected ref can move.

Review retention is opt-in. `--review-retention N` archives completed reviews when more than `N` remain live. Incomplete reviews never lapse unless `--review-lapse-after-secs` is also set; that flag is invalid without a retention count.

### API surface

<!-- generated: choir surface, do not edit -->

| Endpoint | Purpose |
|---|---|
| `POST /api/submit` | Submit one signed operation (hex payload, hex signature) |
| `POST /api/submit-batch` | Same, in array order; the primary path for agent workloads (throughput figures live in PHASE0.md, not here, so they cannot go stale) |
| `GET /api/view` | The materialized view: workspace heads, refs, reviews, provenance |
| `GET /api/log?from=N` | Ordered log entries, the catch-up and sync primitive. Absolute `from`: entries evicted from the in-memory window are served from the persisted log (`source` says which), and a node that cannot reach that far back answers 409 rather than a page with a hole in it. Each entry carries its hash, parent and author signature so pages can be chained and verified without trusting the node; SYNC.md is that procedure |
| `POST /api/workspace` | Provision a copy-on-write workspace and register it in the view |
| `GET /api/reviews?reviewer=X` | One actor's pending review queue |
| `GET /llms.txt` | This surface, as text, for an agent that has never seen choir |
| `GET /sync.md` | The sync contract, in full: cursor semantics and how to verify a page's hash chain and author signatures without trusting the node serving them |
| `POST /api/git-update` | Internal: the pre-receive hook callback |

### The `choir` CLI

```text
usage:
  choir [--auth-file <path>] [--auth-user <name>] <command> ...

commands:
  choir key <key-file> [name]
  choir workspace <api> <owner/repo> <name>
  choir submit <api> <key-file> <channel> '<op-json>'
  choir review <api> <key-file> <channel> <id> <git-oid> [--ref <repo:ref>] [reviewer]...
  choir verdict <api> <key-file> <reviewer> <id> approve|request-changes [note]
  choir intent <api> <key-file> <channel> <subject> <kind> '<body>'
  choir reviews <api> <reviewer>
  choir view <api>

Exit codes: 0 accepted, 1 the node rejected (its JSON error body is printed), 2 usage error.
```
<!-- /generated -->

`choir review` with no reviewer names is the preferred form: the node draws reviewers from an operator-curated pool and signs the assignment itself, so a requester cannot pick their own reviewers. `--require-assignment` makes that the only form node-wide; `--protected-refs` makes it the only form for reviews whose `--ref` names a protected ref, which is how "privilege-bearing" gets a definition the code can read.

A trusted-keys line may bind a key to a channel name — `<name> <hex>` instead of a bare `<hex>` — and a bound key's `RequestReview` or `PostVerdict` is refused on any other channel. Names are conventionally `operator/agent`, and the reviewer draw excludes everyone sharing the requester's operator prefix, taking at most one seat per operator: without that, someone running three agents in the pool satisfies two-person review by themselves. An unprefixed name is its own operator, so a flat pool behaves as it did before. Without it, "the verdict's reviewer matches the signed submission channel" only proves a claim is self-consistent, not that it is true: any trusted key could post as any name. Binding is opt-in per key and additive, so an existing keys file keeps working unchanged, and an edit takes effect on the next request.

Adding `--require-review` turns that from a convention into a gate: a protected ref only moves to a commit some approved review with weight from at least two distinct operators already named as its destination, and cannot be deleted at all. Each operator contributes at most one approval unit even if it controls several agent channels; `/api/view` exposes the resulting `approval_weight`. There is no exemption for the daemon's own key, because every `git push` reaches the sequencer as a node-signed `SetRef` — so switching it on means this node's own repository can only be advanced through a review. Creating a protected ref is still allowed; nothing exists yet to hijack, and since deletion is refused, delete-then-recreate is not a way back in.

### The MCP adapter

`choir-mcp` exposes the six public platform operations as a synchronous stdio MCP server. It keeps no platform state; each tool call crosses the node's HTTP and authentication boundary.

```text
choir-mcp <api> [--auth-file <path>] [--auth-user <name>]
```

Register that command as a stdio server in the agent harness. The optional credential file uses the node's `user:token` format. If it contains more than one entry, select one with `--auth-user`. Discovery documents and the internal git hook are intentionally not exposed as tools.

### The bridge

```bash
cargo run -p choir-bridge -- <upstream-url> <mirror-path> <api-base> <bridge-key-file> <label> [--once]
cargo run -p choir-bridge -- queue <app-id> <pem-path> <owner/repo> <workdir> [--land] [--watch <secs>]
cargo run -p choir-bridge -- --pubkey <key-file>
cargo run -p choir-bridge -- app-debug <app-id> <pem-path>
cargo run -p choir-bridge -- post-status <app-id> <pem-path> <owner/repo> <sha> <state> <description>
```

The bridge mirrors an existing forge and submits the ref delta as signed operations, so choir follows and upstream stays canonical. There is no dual-write. The `queue` mode runs speculative merge trains against a GitHub repository as a bot: build the train, push it so the host's CI runs on it, post a per-pull-request verdict, optionally fast-forward the base branch, and revert the whole train if the landed commit later fails.

The utility modes mint or inspect bridge identity (`--pubkey`), show each GitHub App installation's granted permissions (`app-debug`), and exercise one commit-status write (`post-status`). Grant only the permissions in the [bridge permission model](crates/choir-bridge/PERMISSIONS.md); `queue --land` is the only routine mode that needs contents write access.

The point of the bridge is that the queue sees things per-branch CI cannot. In the live run, a pull request whose own CI was green got a red verdict because it conflicted with the train.

## Architecture

```text
choir-hash      content-address envelope (codec byte + digest); BLAKE3 and git-oid codecs
  choir-oplog   L1 wire format: hash-chained OpEntry, OpLog seam (MemLog | FileLog)
    choir-view       L1 typed ViewOp -> pure fold -> View (workspace heads + refs)
    choir-identity   L8 ed25519 per actor; actor id = hash(pubkey)
    choir-sequencer  L2 single-writer thread owns the log; SubmitPolicy seam
      choir-queue    L2 speculative merge train (Zuul window: start 20, +1/pass, halve/fail)
      choir-actor    L2 same contract on the Rivet actor runtime (2nd seam impl)
    choir-merge   ordered 3-way strategies: trivial -> line (diffy) -> mergiraf subprocess
  choir-store   L0 BLAKE3 + FastCDC chunk store (MemStore | FsStore)

choir-node    L3 daemon: git smart-HTTP via `git http-backend` CGI + the platform API
choir-cli     the agent-facing command line
choir-bridge  forge bridge: mirrors an upstream, runs speculative trains as a bot
choir-demo    composes everything, narrated
choir-spike   the Phase-0 gate measurement, as a binary with an exit code
```

### One operation, end to end

A client builds a `ViewOp` and serializes it, signs `(workspace, payload)` with its actor key, and calls `SequencerHandle::try_submit`, which sends it over a channel to the one writer thread. That thread verifies the signature, trial-applies the operation against a cloned view for the compare-and-swap check, and on acceptance stamps `seq` and `parent`, hashes the entry, folds it into the live view, and appends. The caller gets back `Accepted { seq, hash, decision_latency }`. Readers either poll `GET /api/view` or replay the log themselves.

Note what the author does **not** sign: `seq` and `parent` are assigned after signing. Replaying a signed operation at a different position is blocked by the compare-and-swap value inside the payload, not by the signature.

## Invariants

These are the one-way doors. Breaking one is a data migration, not a refactor. The [design blueprint](plan.md) carries the reasoning and the tripwire that would reverse each bet.

1. Every persisted struct carries a `format_version`, and new fields are additive, so old logs still decode and still hash identically.
2. Hashes are self-describing. A hash always carries its codec byte, which is how git object ids live in the operation log without pretending to be BLAKE3.
3. Canonical serialization is load-bearing. Map fields in hashed structs are `BTreeMap` for exactly this reason; switching one to `HashMap` silently breaks every hash.
4. The author signs `(workspace, payload)` only.
5. Only the sequencer thread appends.
6. A conflict is a value, never a failure.
7. Witness fields exist and stay empty until the transparency-log phase. Do not remove them to tidy up.
8. Mergiraf is GPLv3 and runs as a subprocess only. Linking it as a crate would GPL our binaries.
9. The node refuses a non-loopback bind without TLS.
10. The bridge follows, never co-leads.

## House conventions

Several deliberate choices look like omissions:

- Synchronous and thread-based everywhere except `choir-actor`. No tokio in the rest of the workspace.
- No HTTP client crate. Outbound HTTP shells out to `curl`.
- No base64, ssh-format or JWT crates. Those are hand-rolled; RS256 signing shells out to `openssl` so a private key never becomes parsed key material in our address space.
- Rust binaries take configuration through CLI flags and named files. Agent templates and operator wrappers may use environment variables to assemble those explicit arguments.
- Dependencies are added reluctantly. A test that needs randomness hand-rolls an xorshift rather than pulling in `rand`.
- `missing_docs` is a warning, so omissions remain visible during compilation; broken intra-doc links are denied. Generated-surface tests keep the CLI, API, MCP catalog, and agent-facing command lists aligned.

Testing follows two rules. A seam is only real when a shared conformance suite plus a second implementation both pass it, so `MemLog` and `FileLog`, `MemStore` and `FsStore`, and the in-process and Rivet sequencers each run the same assertions. Integration tests use the real thing rather than mocks: they bind a real node on port 0 and drive it with real `git`, `curl` and `openssl` subprocesses. Gate thresholds are assertions, not reports.

## Measured results

From the Linux target (GCP n2-standard-4, Debian 12, btrfs), continuous integration excluded per the gate definition:

| Measurement | Result | Target |
|---|---|---|
| Workspace provisioning (btrfs snapshot, 2000-file tree) | p50 4 ms, p90 5 ms | p50 under 50 ms |
| Firecracker snapshot restore | p50 10 ms, p90 35 ms | under 500 ms |
| Merge decision latency, integrated spike | p99 114 microseconds | under 100 ms |
| Silent merge picks | 0 | 0 |

On the daemon itself, measured with the ForgeMark harness on a laptop over loopback: roughly 46 pushes/s and 86 shallow clones/s at 32 concurrent clients, against Phase-1 targets of 5 commits/s/repo through the queue and 10,000 clones/hour.

Full evidence, including the corrections and the failed approaches, is in the [Phase-0 log](PHASE0.md).

## Repository map

- [`plan.md`](plan.md) is the design blueprint. Its decision register classifies every bet as a one-way or two-way door and records the tripwire that reverses it. Code comments cite these rows by number.
- [`PHASE0.md`](PHASE0.md) is the running build log and gate tracker, and the source of truth for status.
- `crates/` holds the 14 workspace crates listed above.
- [`templates/`](templates/README.md) is a deliverable, not configuration: snippets that teach someone else's coding agent (Claude Code, Codex, Cursor) how to talk to a choir node.
- [`scripts/`](scripts/flip/RUNBOOK.md) holds the benchmarks and operator tooling. `sh scripts/choirctl` with no arguments lists the node commands (`install`, `status`, `logs`, `stop`, `uninstall`, `sync`, `push`, `mirror`, `url`).

## Notes on running your own

Keys, tokens and PEM files belong under `~/.choir/` at mode 0600, and the daemon's own key lives at `<root>/.choir/node.key`. Never in the repository, never in a commit message, never in a document. Host addresses and account names stay as placeholders in tracked files.

The `choir-actor` conformance test is ignored by default because it spawns a local Rivet engine. Run it with `RIVETKIT_ENGINE_AUTO_DOWNLOAD=1 cargo test -p choir-actor -- --ignored`. That download is broken upstream in rivetkit 2.3.10; the [Phase-0 log](PHASE0.md) records the four workarounds, one of which is the `LIBSQLITE3_FLAGS` setting in `.cargo/config.toml`. Do not remove it, every build in the workspace needs it.

## License

Crates are declared `MIT OR Apache-2.0` in the workspace manifest. License files are not yet in the tree. Mergiraf, invoked as an optional subprocess, is GPLv3 and is deliberately never linked.
