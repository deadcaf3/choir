# Architecture

choir is an agent-first code collaboration platform. Many agents work on one
repository at once, a single-writer sequencer puts every change in one total
order, and merge conflicts are first-class values rather than errors.

This page maps the layers, the crate that implements each, and what travels
between them. Every crate's `//!` header assumes it.

## The model

A change is a **signed operation** appended to an **append-only log**. One
writer thread per repository decides the order, stamps a sequence number and
appends. Every other structure, the set of refs, the review queue and who
holds which workspace, is a **fold over that log**, recomputed rather than
stored. Undo is therefore a pure function of position, and two agents racing
the same ref get a compare-and-swap.

## Layers

The layer numbers are the ones code comments and `DECISIONS.md` use.

| Layer | Concern | Crate | Decision |
|:--|:--|:--|:--|
| L0 | Content-addressed storage: BLAKE3 + FastCDC chunking | `choir-store`, `choir-hash` | D6 (**one-way**) |
| L1 | The op log's wire format, and the log-backend seam | `choir-oplog` | D1, D16 |
| L1 | The view: typed operations folded into workspaces, refs, reviews | `choir-view` | D1, D9 |
| L2 | Speculative merge queue with a TCP-like window | `choir-queue` | D5 |
| L3 | The node daemon: git smart-HTTP plus the platform API | `choir-node` | D12 |
| L5 | Content-addressed provenance | `choir-view` (`Provenance`) | D11 (**one-way**) |
| L8 | Identity: one ed25519 key per actor, signatures over log entries | `choir-identity` | D9 (**one-way**) |
| L10 | Transport: centralized now, peer-to-peer later | `choir-node` | D14 (gated) |

Three crates sit beside the stack rather than inside it:

| Crate | What it is |
|:--|:--|
| `choir-sequencer` | The single writer itself (D2), plus the decision journal, the fairness queue and the lag meter |
| `choir-actor` | A *second* implementation of the same actor-runtime seam, on Rivet (D3). It exists to prove the seam is a seam: both implementations pass one conformance suite |
| `choir-merge` | The merge-strategy pipeline (D4/D19), cheapest strategy first, Mergiraf as an optional subprocess |

Four are tools rather than layers: `choir-cli` (the `choir` binary, and the
surface table every generated document is rendered from), `choir-bridge`
(the forge follower, D21), `choir-demo` (a narrated walkthrough of the whole
stack) and `choir-spike` (the Phase-0 gate binary). One is neither:
`choir-fs`, the durable-file primitives — atomic writes and a working-
directory lock — that the binaries share.

Each crate's own header links the crates it depends on, and the gate checks
those links.

## What one operation does

An agent submitting a change walks the whole stack.

```text
  agent
    │  choir submit / choir batch          (choir-cli)
    ▼
  POST /api/submit                          (choir-node)
    │  authenticate            --auth-file
    │  authorize               --acl-file            D29
    │  meter                   --rate-limit-api      D33
    │  bound the body          --quota-push-bytes    D37
    ▼
  fairness queue                            (choir-sequencer::fairness)
    │  one bounded window per actor, served round-robin
    ▼
  THE SINGLE WRITER                         (choir-sequencer)
    │  verify the signature                          (choir-identity)  L8
    │  run the admission policy                      (choir-view)
    │  compare-and-swap the ref it touches
    │  stamp seq, append                             (choir-oplog)     L1
    │  record the decision     --journal
    ▼
  the view is refolded                      (choir-view)
    │  refs, workspaces, reviews, provenance
    ▼
  ref landed → webhook        --hooks-file            D32
```

Two properties of that shape are load-bearing:

- **The order is decided in exactly one place.** Round-robin fairness
  decides who is *asked* next. The writer stamps `seq` one at a time, alone,
  and appends in the order it decided.
- **Every check before the writer is advisory about identity.** The actor
  key the fairness queue buckets on is a *claim*, and the signature is
  verified on the writer thread. It bounds an honest flooder.

> [!IMPORTANT]
> Nothing in front of the writer may become load-bearing for authorization.

## A conflict is a value

`choir-view`'s `TreeEntry` has a `Conflict` variant, and a commit containing
one is a **valid commit**: it hashes, it is signed, it is appended, and an
agent can keep working on top of it.

The merge queue therefore never blocks. A change that conflicts is evicted
from the speculative train as a first-class conflict and the queue keeps
moving. `choir-merge` is a *pipeline* of strategies for the same reason:
trivial merge, then line merge, then Mergiraf as an optional subprocess,
each free to decline as a normal outcome.

## Replay purity

The view is a fold, so replaying the log from position zero must produce
exactly the state the node is serving. That is promoted to a contract (D40),
and three things follow from it:

1. **Derived data is never a durability barrier.** The request log, the
   decision journal and the lag log are one-way records nothing replays.
2. **Some projections are folded out of the log rather than persisted.** The
   per-user workspace tally that `--quota-workspaces` enforces is one: a
   restart rebuilds it from the same log that rebuilds everything else.
3. **Accounts and credentials sit outside the log (D36).** The log is
   append-only, and revoking a credential has to be deletion.

## What the format versions are for

`FORMAT_VERSION` appears in `choir-store`, `choir-oplog` and `choir-view`,
and the one-way rows of `DECISIONS.md` are mostly about those three numbers.
Three rules govern them:

- Hashes are **self-describing**: a codec byte names the hash function, so a
  hash migration adds a codec rather than rewriting every stored identifier.
- A field that will be needed later is present from day one.
  `OpEntry::witnesses` has existed since the first entry, empty, making the
  D16 swap to a witnessed log a value change.
- Adding an operation variant is **one-way for readers**. The first accepted
  entry of that shape permanently ends replay compatibility with earlier
  binaries.

## Where to go next

| You want | Read |
|:--|:--|
| To run one | `docs/operating/running-a-node.md` |
| To use one | `docs/using/cli.md` |
| Why a decision went the way it did | `DECISIONS.md` |
| To catch up on a log and check the page you were served | `SYNC.md` |
