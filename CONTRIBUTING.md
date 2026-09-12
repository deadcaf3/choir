# Contributing

## Before you change anything

**Read the decision register.** [`DECISIONS.md`](DECISIONS.md) holds every
significant choice as a numbered row, one-way or two-way door. Code cites
them by number (`D6`, `D16`, ...) over a thousand times, so **a decision's
number is part of the API**. Read the row a comment cites before touching
that code; contradict it openly in the pull request, never quietly.

## The gate

One script runs every check and **fails closed** on formatting, tests,
clippy, rustdoc, the spike, freshness and the history scan.

```bash
./gate touched   # inner loop: tests and lints for the crates this tree changed
./gate quick     # sub-minute: every check that never invokes the compiler
./gate fast      # edit loop: skips the timing-gated stages
./gate           # every release check, 5 to 10 minutes
```

An unrecognised lane exits 2.

- `./gate touched` while editing, `./gate fast` before a commit.
- **Full lane once a piece of work is finished**, not per commit.
- **The fast lane is not a substitute.** The full lane's receipt covers
  the tree as of its start; any later edit voids it.

CI runs `./gate quick` only, which compiles nothing, so green CI is
**not** evidence your change builds. The history scan needs an untracked
needle list and runs locally only.

## Keep `.cargo/config.toml`

Every build needs its `LIBSQLITE3_FLAGS`. **The failure it prevents does
not name itself.**

## Invariants: do not break these without reading the register

One-way doors. Breaking one is a data migration.

1. **Every persisted struct carries `format_version`.** New fields are
   additive (`#[serde(default, skip_serializing_if = "Option::is_none")]`)
   so old logs still decode and hash identically.
2. **Hashes are self-describing.** A `ContentHash` always carries its
   codec byte.
3. **Canonical serialization is load-bearing.** Hashes are BLAKE3 over
   `serde_json::to_vec`. Map fields in hashed structs are `BTreeMap`; a
   `HashMap` silently breaks every hash. A test enforces this.
4. **The author signs `(workspace, payload)` only.** `seq` and `parent`
   come after signing. Replay is blocked by the compare-and-swap in the
   payload.
5. **Only the sequencer thread appends**, and storage decides before any
   projection does.
6. **A conflict is a value, never a failure.** No silent auto-resolve.
7. **`OpEntry.witnesses` stays empty, permanently** (D67). It is inside
   the hashed bytes. Do not remove it.
8. **Mergiraf is GPLv3 and runs as a subprocess only** (D4).
9. **The node refuses a non-loopback bind without TLS.**
10. **The bridge follows, never co-leads.** Upstream is canonical (D21).
    No dual-write.

## Testing conventions

- **A seam is real only when a conformance suite plus a second
  implementation both pass.** Pattern: a private
  `fn conformance(x: &mut dyn Trait)`, called from one `#[test]` per
  backend.
- **Integration tests use the real thing, not mocks.** A real node on
  port 0, driven by real `git`, `curl`, `openssl` and `ssh-keygen`.
- **One `tests/it/` harness per crate.** A new test file is a `mod` line
  in `tests/it/main.rs`. **Without it the file runs zero tests,
  silently.**
- Harness modules share a process and run in parallel: no wall-clock
  assertions, process globals (env, cwd, allocator), fixed ports or
  reused temp-dir names.
- The harness's open-file count grows with its test count, because every
  test's node keeps its listener and log open until the process exits.
  `./gate` raises the soft limit to 4096; a bare `cargo test -p
  choir-node --test it` from a terminal at the macOS default of 256
  fails with `Too many open files`, which is the limit, not a bug.
- **Gate thresholds are assertions.**
- No fixtures or golden files; test data is generated inline.

## House conventions

Deliberate choices that look like omissions:

- **Synchronous and thread-based everywhere** except `choir-actor`. No
  tokio elsewhere; find a blocking dependency.
- **No HTTP client crate.** Outbound HTTP shells out to `curl` with
  `-w '\n%{http_code}'` for the status code.
- **No base64, ssh-format or JWT crates.** Hand-rolled. RS256 signing
  shells out to `openssl` so a private key is never parsed in-process.
- **Dependencies are added reluctantly.** A test hand-rolls an xorshift
  rather than adding `rand`. Justify any new one in the pull request.
- `missing_docs`, `broken_intra_doc_links` and `private_intra_doc_links`
  are workspace lints. Every public item gets a doc comment; each crate's
  module doc has a `# Examples` doctest. **Rustdoc lints only bite under
  `cargo doc`**, a separate gate stage.

## Commits

Subject: lowercase, imperative, **the effect on behaviour**. Body: what
was wrong, why this shape, what you left alone. A commit that alters a
decision cites its number. No AI or tool attribution.

## Submitting

1. `./gate` green on your final tree.
2. A pull request describing the behaviour change, the decisions it
   touches, and anything you were unsure about.

Security issues go to [`SECURITY.md`](SECURITY.md).

## For the operator

GitHub UI settings (Pages, the custom domain, the two repository
variables `.github/workflows/pages.yml` reads):
[`.github/README-metadata.md`](.github/README-metadata.md).
