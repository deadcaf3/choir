# Architecture

choir is an agent-first code collaboration platform. Many agents work on one
repository at once, a single-writer sequencer puts every change in one total
order, and merge conflicts are first-class values rather than errors.

This page is the map: the layers, which crate implements each one, and what
travels between them. Every crate's own `//!` header assumes you have read
it.

## The one-paragraph version

A change is not a diff against a branch. It is a **signed operation**
appended to an **append-only log**. One writer thread per repository decides
the order, stamps a sequence number, and appends. Every other structure in
the system — the set of refs, the review queue, who holds which workspace —
is a **fold over that log**, recomputed rather than stored. That is what
makes undo a pure function of position, and what makes two agents racing the
same ref a compare-and-swap rather than a lock.

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

Crate names are written here as plain code rather than as links on purpose.
No crate depends on all fourteen others, so a workspace-wide map cannot be
expressed in rustdoc links that resolve; each crate's own header links the
crates it actually depends on, and those links are checked by the gate.

## What one operation does

An agent submitting a change walks the whole stack. Following one operation
end to end is the fastest way to see how the crates fit.

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

Two properties fall out of that shape, and both are load-bearing:

- **The order is decided in exactly one place.** Round-robin fairness
  decides who is *asked* next; it never decides who lands first. The writer
  still stamps `seq` one at a time, alone, and appends in the order it
  decided.
- **Every check before the writer is advisory about identity.** The actor
  key the fairness queue buckets on is a *claim*; the signature is verified
  on the writer thread. That is enough to bound an honest flooder and
  nothing more, and nothing in front of the writer may ever become
  load-bearing for authorization.

## A conflict is a value

Most of this system is ordinary. The part that is not is that
`choir-view`'s `TreeEntry` has a `Conflict` variant, and a commit
containing one is a **valid commit** — it hashes, it is signed, it is
appended, and an agent can keep working on top of it.

That is why the merge queue never blocks: a change that conflicts is evicted
from the speculative train as a first-class conflict rather than parked
behind a lock, and the queue keeps moving. It is also why
`choir-merge` is a *pipeline* of strategies rather than one merge
algorithm — trivial merge, then line merge, then optionally Mergiraf as a
subprocess — and why a strategy declining is a normal outcome rather than an
error.

## Replay purity

The view is a fold, so replaying the log from position zero must produce
exactly the state the node is serving. That is promoted to a contract (D40),
and three things follow from it:

1. **Derived data is never a durability barrier.** The request log, the
   decision journal and the lag log are all one-way records nothing replays.
   A journal that could block the single writer would be a durability
   barrier wearing an observability costume.
2. **Some projections are folded out of the log rather than persisted.** The
   per-user workspace tally that `--quota-workspaces` enforces is one: a
   restart rebuilds it from the same log that rebuilds everything else, so
   the ceiling survives a restart with no new file and no second durability
   barrier.
3. **Some state is deliberately outside the log.** Accounts and credentials
   are (D36), because the log is append-only and cannot forget a credential,
   and revoking one has to be deletion rather than a record.

## What the format versions are for

`FORMAT_VERSION` appears in `choir-store`, `choir-oplog` and
`choir-view`, and the one-way rows of `DECISIONS.md` are mostly about
those three numbers. The rule the workspace holds itself to:

- Hashes are **self-describing** — a codec byte names the hash function — so
  a future hash migration adds a codec instead of rewriting every stored
  identifier.
- A field that will be needed later is present from day one rather than
  added later. `OpEntry::witnesses` has existed since the first entry, empty,
  so the D16 swap to a witnessed log is a value change and not a format
  change.
- Adding an operation variant is **one-way for readers**: the feature can
  stop being written at any time, but the first accepted entry of that shape
  permanently ends replay compatibility with earlier binaries.

## Where to go next

| You want | Read |
|:--|:--|
| To run one | `docs/operating/running-a-node.md` |
| To use one | `docs/using/cli.md` |
| Why a decision went the way it did | `DECISIONS.md` |
| To catch up on a log without trusting the node | `SYNC.md` |
