# Contributing

Changes are welcome. This file exists because several conventions here
are deliberate and none of them are guessable from the code.

## Before you change anything

**Read the decision register.** [`DECISIONS.md`](DECISIONS.md) holds every
significant choice as a numbered row, each classified as a one-way or
two-way door. Code comments cite these by number (`D6`, `D16`, ...) over a
thousand times across the crates, so **a decision's number is effectively
part of the API**. If a comment on the code you are touching cites a decision,
read that row first. If your change contradicts it, say so in the pull
request and argue the case; do not quietly reverse it.

## The gate

One script runs every check, and it **fails closed**: it exits nonzero if
formatting, tests, clippy, rustdoc, the spike, freshness or the history
scan fails.

```bash
./gate touched   # inner loop: tests and lints for the crates this tree changed
./gate quick     # sub-minute: every check that never invokes the compiler
./gate fast      # edit loop: skips the timing-gated stages
./gate           # every release check, 5 to 10 minutes
```

Four lanes, not two. An unrecognised lane exits 2 rather than falling
back to the full one.

- Run `./gate touched` while editing and `./gate fast` before a commit.
- **Run the full lane once your piece of work is finished**, not once per
  commit. A chain of small commits does not need one apiece.
- **The fast lane is not a substitute for the full one.** The receipt the
  full lane prints describes the tree as of its start. Edit anything
  afterwards, including a comment, and the receipt is void. That is what
  the freshness check enforces.

CI runs `./gate quick` on every push and pull request. It compiles
nothing, which is how it answers inside a minute. A green CI is therefore
**not** evidence that your change builds; the full lane on your machine is.

Some stages are local-only. The history scan needs a needle list that is
untracked by design, so it is skipped in CI and runs for you.

## Keep `.cargo/config.toml`

Every build in the workspace needs the `LIBSQLITE3_FLAGS` it sets. Do not
remove it. **The failure it prevents does not name itself**, so a build
that breaks after you delete it will not tell you why.

## Invariants: do not break these without reading the register

These are one-way doors. Breaking one is a data migration, not a refactor.

1. **Every persisted struct carries `format_version`.** New fields are
   additive (`#[serde(default, skip_serializing_if = "Option::is_none")]`)
   so old logs still decode *and* still hash identically.
2. **Hashes are self-describing.** Never store a bare digest; a
   `ContentHash` always carries its codec byte.
3. **Canonical serialization is load-bearing.** Hashes are BLAKE3 over
   `serde_json::to_vec`. Map fields in hashed structs are `BTreeMap` for
   exactly this reason: switching one to `HashMap` silently breaks every
   hash. A source-scanning test enforces this and will fail your build.
4. **The author signs `(workspace, payload)` only.** `seq` and `parent`
   are assigned after signing. Replay at another position is blocked by
   the compare-and-swap inside the payload, not by the signature.
5. **Only the sequencer thread appends**, and storage decides before any
   projection does.
6. **A conflict is a value, never a failure.** A merge strategy that
   cannot resolve returns a conflict rather than picking a side. There is
   no silent auto-resolve anywhere.
7. **`OpEntry.witnesses` stays empty, permanently** (D67). It is inside
   the bytes the content hash covers. Do not remove the field to tidy up;
   it is load-bearing for format stability.
8. **Mergiraf is GPLv3 and runs as a subprocess only** (D4). Linking it
   as a crate would GPL the binaries.
9. **The node refuses a non-loopback bind without TLS.** Do not soften
   this. It is a privacy rule expressed as code.
10. **The bridge follows, never co-leads.** Upstream is canonical and
    choir mirrors it (D21). No dual-write.

## Testing conventions

- **A seam is only real when a conformance suite plus a second
  implementation both pass.** Not a trait with one impl: an actual second
  backend that could disagree. The pattern is a private
  `fn conformance(x: &mut dyn Trait)` holding the shared assertions,
  called from one `#[test]` per backend. Adding a backend means adding a
  call, not writing a new suite.
- **Integration tests use the real thing, not mocks.** They bind a real
  node on port 0 and drive it with real `git`, `curl`, `openssl` and
  `ssh-keygen` subprocesses.
- **Integration tests live in one `tests/it/` harness per crate**, because
  cargo links one binary per `tests/*.rs` file. A new test file is a `mod`
  line in that crate's `tests/it/main.rs`. **Without that line the file
  compiles into nothing and runs zero tests, silently.** This is the
  single easiest mistake to make here.
- Those modules share a process and run on parallel threads, so a merged
  module must not assert on wall-clock time, mutate process globals (env,
  cwd, allocator), bind a fixed port, or reuse another module's temp-dir
  name. Files that break those rules stay their own binary on purpose.
- **Gate thresholds are assertions**, not reports. They must keep passing.
- No fixtures and no golden files; test data is generated inline.

## House conventions

Deliberate choices that look like omissions:

- **Synchronous and thread-based everywhere** except `choir-actor`. No
  tokio in the rest of the workspace. Do not introduce async to a crate to
  satisfy a dependency; find a blocking one.
- **No HTTP client crate.** All outbound HTTP shells out to `curl`, using
  `-w '\n%{http_code}'` to carry the status code on stdout.
- **No base64, ssh-format or JWT crates.** These are hand-rolled. RS256
  signing shells out to `openssl` so a private key never becomes parsed
  key material in our address space.
- **Dependencies are added reluctantly.** A test needing randomness
  hand-rolls an xorshift rather than pulling `rand` as a dev-dependency.
  Match that bar, and expect to justify a new dependency in the pull
  request.
- `missing_docs`, `broken_intra_doc_links` and `private_intra_doc_links`
  are workspace lints. Every public item needs a doc comment, and each
  crate's module doc carries a `# Examples` doctest that runs under
  `cargo test`. **The rustdoc lints only bite under `cargo doc`**, which
  is why it is a separate gate stage.

## Commits

Subject lines are lowercase, imperative, and describe **the effect on
behaviour** rather than the edit. Look at `git log` before writing one.

The body is where the work is. Explain what was wrong, why the fix is the
right shape, and what you deliberately did not change. A commit that
alters a decision cites its number.

Do not put AI or tool attribution in commit messages.

## Submitting

1. `./gate` green on your final tree.
2. A pull request describing the behaviour change, the decisions it
   touches, and anything you were unsure about.
3. Expect questions about dependencies, seams without a second
   implementation, and any hashed struct you added.

Security issues do not go here. See [`SECURITY.md`](SECURITY.md).

## For the operator

What has to be set in the GitHub UI rather than in a file — repository
settings, Pages, the custom domain and the two repository variables
`.github/workflows/pages.yml` reads — is a checklist in
[`.github/README-metadata.md`](.github/README-metadata.md).
