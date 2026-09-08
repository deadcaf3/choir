# Architecture

Many agents work on one repository; a single-writer sequencer puts every
change in one total order; merge conflicts are values, not errors. This page
maps the layers and the crate for each.

## The model

A change is a **signed operation** appended to an **append-only log**. One
writer thread per repository decides the order, stamps a sequence number and
appends. Refs, the review queue and workspace ownership are **folds over that
log**, recomputed rather than stored. Undo is a function of position; two
agents racing one ref get a compare-and-swap.

## Layers

Layer numbers match code comments and `DECISIONS.md`.

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

Beside the stack:

| Crate | What it is |
|:--|:--|
| `choir-sequencer` | The single writer (D2), decision journal, fairness queue, lag meter |
| `choir-actor` | A second implementation of the actor-runtime seam, on Rivet (D3); both pass one conformance suite |
| `choir-merge` | The merge-strategy pipeline (D4/D19), cheapest first, Mergiraf as an optional subprocess |

Tools: `choir-cli` (the `choir` binary and the surface table every generated
document renders from), `choir-bridge` (forge follower, D21), `choir-demo`
(narrated walkthrough), `choir-spike` (Phase-0 gate binary). `choir-fs`
holds the atomic-write and lock primitives the binaries share. `choir-guards`
holds source-scanning tripwires (D77).

## What one operation does

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

- **The order is decided in one place.** Fairness decides who is asked
  next; the writer stamps `seq` alone and appends in that order.
- **Checks before the writer are advisory.** The actor key the fairness
  queue buckets on is a claim; the signature is verified on the writer
  thread.

> [!IMPORTANT]
> Nothing in front of the writer may become load-bearing for authorization.

## A conflict is a value

`TreeEntry::Conflict` in `choir-view` is a **valid commit**: hashed, signed,
appended, and buildable on. The merge queue never blocks; a conflicting
change is evicted from the speculative train as a conflict and the queue
keeps moving. `choir-merge` is a pipeline (trivial, line, Mergiraf) where
each strategy may decline.

## Replay purity

Replaying the log from zero must produce exactly the served state (D40).
So:

1. **Derived data is never a durability barrier.** Request log, decision
   journal and lag log are one-way records.
2. **Some projections are folded, not persisted.** The per-user workspace
   tally behind `--quota-workspaces` is rebuilt from the log on restart.
3. **Accounts and credentials sit outside the log (D36).** Revocation is
   deletion.

## What the format versions are for

`FORMAT_VERSION` appears in `choir-store`, `choir-oplog` and `choir-view`.

- Hashes are **self-describing**: a codec byte names the function, so a
  migration adds a codec.
- Fields needed later exist from day one: `OpEntry::witnesses` has always
  been present and empty (D16, D67).
- Adding an operation variant is **one-way for readers**.

## Where to go next

| You want | Read |
|:--|:--|
| To run one | `docs/operating/running-a-node.md` |
| To use one | `docs/using/cli.md` |
| Why a decision went the way it did | `DECISIONS.md` |
| To catch up on a log and check a served page | `SYNC.md` |
