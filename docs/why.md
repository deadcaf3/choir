# Why choir exists

## The problem

Several coding agents on one repository collide over the *turn*, not the
code: each needs to know the tip of `main`, and the answer is only true
until another agent pushes. On a forge built for people, a human arbitrates
by merging and rebasing, so every agent queues behind that person.

Merging is solved. The bottleneck is **ordering**: deciding which
concurrent attempt came first, without a person.

## The mechanism

A change is a **signed operation** appended to a hash-chained log.

- One writer thread per repository decides the order. It is the only thing
  that appends, so "what happened first" has exactly one answer.
- Every operation carries its author's signature over `(workspace,
  payload)`; every entry carries the hash of the previous one.
- Refs, workspaces, changes, reviews and approvals are **folds over that
  log**. Nothing is stored twice, so nothing can disagree with the order.

A `git push` joins the same order: the `pre-receive` hook turns each ref
update into a signed operation with git's old object id as the
compare-and-swap precondition. The git client is unmodified.

## Three consequences

**A race is a compare-and-swap.** The later of two pushes to one ref gets a
rejection naming the head it lost to. Fetch, integrate, submit again. No
lock is held.

**A conflict is a committed value.** A strategy that cannot resolve returns
a conflict, which is appended like any other state and built on. Nothing
silently picks a side.

**Undo is arithmetic.** The state at operation 4,110 is a replay, not a
restore. A bad landing is subtracted by a compensating operation, and the
history stays readable.

## Who it is for

Someone running coding agents against a repository, in three roles:

| Role | What they do | Start at |
|:--|:--|:--|
| Operator | Runs the node, issues credentials, sets policy | [Running a node](operating/running-a-node.md) |
| Contributor | Human or agent: workspace, propose, push | [The contribution workflow](using/workflow.md) |
| Reviewer | Answers drawn reviews, lands what passes | [The contribution workflow](using/workflow.md) |

If one agent works on your repository at a time, you do not have this
problem.

## What it is not

**Not a GitHub replacement.** No organizations, marketplace, actions, or
wiki. The surface is repositories, changes, reviews, and the log.

**You do not have to move.** The [bridge](../crates/choir-bridge/PERMISSIONS.md)
mirrors an upstream GitHub repository and runs the sequencer beside it.
Upstream stays canonical; there is no dual-write.

Next: [Architecture](architecture.md) or [The CLI and HTTP API](using/cli.md).
