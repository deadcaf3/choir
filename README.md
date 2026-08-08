# choir

An agent-first code collaboration platform: many agents working concurrently on one repository, ordered by a single-writer sequencer, with first-class merge conflicts.

Git forges today assume human tempo: a handful of contributors, a handful of branches, merges that are rare and negotiated. Coding agents invert that. Published measurements put the conflict rate on agent-authored pull requests between 15 and 32 percent, and even after a clean structural merge a 5 to 10 percent residual semantic-conflict rate remains. choir is a forge built for that regime instead of retrofitted to it.

**Status: research prototype.** The Phase-0 feasibility gate passed on the Linux target; Phase 1 is in progress. Nothing here is production software, there is no CI, and the gate is run by hand.

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

Requirements: stable Rust (built with cargo 1.97.1, edition 2021), plus `git`, `curl`, `openssl` and `ssh-keygen` on PATH, which the integration tests drive as real subprocesses.

## Running a node

```bash
cargo run -p choir-node -- <repo-root> [port] [--create owner/name.git]... \
  [--auth-file f] [--keys-file f] [--reviewers-file f] [--bind addr] \
  [--tls-cert c --tls-key k]
```

The daemon serves git over smart-HTTP (via `git http-backend` as CGI) and a platform API on the same port. HTTP basic auth is mandatory when an auth file is given, and the node refuses a non-loopback bind without TLS: that is the privacy rule expressed as code, not as a note.

Repositories created by the daemon get a `pre-receive` hook that calls back into the API, so a `git push` becomes a node-signed ref operation in the same total order as everything else. A repository created any other way is not sequenced.

### API surface

| Endpoint | Purpose |
|---|---|
| `POST /api/submit` | Submit one signed operation (hex payload, hex signature) |
| `POST /api/submit-batch` | Same, in array order, roughly 1,130 signed ops/s measured |
| `GET /api/view` | The materialized view: workspace heads, refs, reviews, provenance |
| `GET /api/log?from=N` | Ordered log entries, the catch-up and sync primitive. Absolute `from`: entries evicted from the in-memory window are served from the persisted log (`source` says which), and a node that cannot reach that far back answers 409 rather than a page with a hole in it |
| `POST /api/workspace` | Provision a copy-on-write workspace and register it in the view |
| `GET /api/reviews?reviewer=X` | One actor's pending review queue |
| `POST /api/git-update` | Internal: the pre-receive hook callback |

### The `choir` CLI

```text
choir key <key-file>
choir workspace <api> <owner/repo> <name>
choir submit <api> <key-file> <channel> '<op-json>'
choir review <api> <key-file> <channel> <id> <git-oid> [reviewer]...
choir verdict <api> <key-file> <reviewer> <id> approve|request-changes [note]
choir intent <api> <key-file> <channel> <subject> <kind> '<body>'
choir reviews <api> <reviewer>
choir view <api>
```

Exit codes: 0 accepted, 1 rejected by the node with the error body printed, 2 usage error.

`choir review` with no reviewer names is the preferred form: the node draws reviewers from an operator-curated pool and signs the assignment itself, so a requester cannot pick their own reviewers.

### The bridge

```bash
cargo run -p choir-bridge -- <upstream-url> <mirror-path> <api-base> <bridge-key-file> <label> [--once]
cargo run -p choir-bridge -- queue <app-id> <pem-path> <owner/repo> <workdir> [--land] [--watch <secs>]
```

The bridge mirrors an existing forge and submits the ref delta as signed operations, so choir follows and upstream stays canonical. There is no dual-write. The `queue` mode runs speculative merge trains against a GitHub repository as a bot: build the train, push it so the host's CI runs on it, post a per-pull-request verdict, optionally fast-forward the base branch, and revert the whole train if the landed commit later fails.

The point of the bridge is that the queue sees things per-branch CI cannot. In the live run, a pull request whose own CI was green got a red verdict because it conflicted with the train.

## Architecture

```
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

These are the one-way doors. Breaking one is a data migration, not a refactor. `plan.md` carries the reasoning and the tripwire that would reverse each bet.

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
- No environment variables. Everything is CLI flags and files.
- Dependencies are added reluctantly. A test that needs randomness hand-rolls an xorshift rather than pulling in `rand`.
- `missing_docs` is a warning and broken intra-doc links are denied, so every public item is documented and every crate's module doc carries a runnable example.

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

Full evidence, including the corrections and the failed approaches, is in `PHASE0.md`.

## Repository map

- `plan.md` is the design blueprint. Its decision register classifies every bet as a one-way or two-way door and records the tripwire that reverses it. Code comments cite these rows by number.
- `PHASE0.md` is the running build log and gate tracker, and the source of truth for status.
- `crates/` holds the 14 workspace crates listed above.
- `templates/` is a deliverable, not configuration: snippets that teach someone else's coding agent (Claude Code, Codex, Cursor) how to talk to a choir node.
- `scripts/` holds the benchmarks and the operator tooling. `sh scripts/choirctl` with no arguments lists the node commands (`install`, `status`, `logs`, `stop`, `uninstall`, `sync`, `push`, `mirror`, `url`).

## Notes on running your own

Keys, tokens and PEM files belong under `~/.choir/` at mode 0600, and the daemon's own key lives at `<root>/.choir/node.key`. Never in the repository, never in a commit message, never in a document. Host addresses and account names stay as placeholders in tracked files.

The `choir-actor` conformance test is ignored by default because it spawns a local Rivet engine. Run it with `RIVETKIT_ENGINE_AUTO_DOWNLOAD=1 cargo test -p choir-actor -- --ignored`. That download is broken upstream in rivetkit 2.3.10; `PHASE0.md` records the four workarounds, one of which is the `LIBSQLITE3_FLAGS` setting in `.cargo/config.toml`. Do not remove it, every build in the workspace needs it.

## License

Crates are declared `MIT OR Apache-2.0` in the workspace manifest. License files are not yet in the tree. Mergiraf, invoked as an optional subprocess, is GPLv3 and is deliberately never linked.
