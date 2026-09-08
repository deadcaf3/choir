# Why choir exists

## The problem

Point three coding agents at one repository and they do not collide over
the code. They collide over the *turn*. Each one wants to know what the
tip of `main` is right now, and each one's answer is only true until
another agent pushes. On a forge built for people, the arbiter of that
question is a human: someone merges, someone rebases, someone re-runs the
checks. Every agent that wanted a decision is blocked behind that person,
and adding agents makes the queue longer rather than the work faster.

The bottleneck is not merging. Three-way merge is a solved problem, and
the tools are good. The bottleneck is **ordering**: agreeing which of the
concurrent attempts happened first, cheaply enough that nobody waits for a
person to say so.

## The mechanism

A change is a **signed operation** appended to a hash-chained log.

- One writer thread per repository decides the order. It is the only thing
  that appends, so there is exactly one answer to "what happened first",
  and producing it costs a lock-free channel send and a check, not a
  meeting.
- Every operation carries its author's signature over `(workspace,
  payload)`, and every entry carries the hash of the one before it.
- Every other structure — refs, workspaces, changes, reviews, approvals —
  is a **fold over that log**. Nothing is stored twice, so nothing can
  disagree with the order.

A `git push` joins the same order: the repository's `pre-receive` hook
turns each ref update into a signed operation with git's old object id as
the compare-and-swap precondition. The git client is unmodified.

## Three consequences

**A race is a compare-and-swap.** Two agents pushing the same ref do not
deadlock and do not need a human. The later one is answered with a
rejection naming the head it lost to, which is a fact it can act on
immediately: fetch, integrate, submit again. The decision is bounded, it
is not a lock, and nothing is held while it is made.

**A conflict is a committed value.** A merge strategy that cannot resolve
returns a conflict, and that conflict is appended like any other state.
Later work builds on top of it. Nobody's push is refused because two
people touched the same function, and no strategy anywhere silently picks
a side.

**Undo is arithmetic.** The log is append-only and every projection is a
fold over it, so "what did this look like at operation 4,110" is a replay,
not a restore. A bad landing is subtracted by folding a compensating
operation on top, and the history that produced the mistake stays readable.

## Who it is for

Someone running coding agents against a repository, in three roles:

| Role | What they do | Start at |
|:--|:--|:--|
| Operator | Runs the node, issues credentials, sets the authorization policy | [Running a node](operating/running-a-node.md) |
| Contributor | Human or agent: takes a workspace, proposes a change, pushes | [The contribution workflow](using/workflow.md) |
| Reviewer | Answers the reviews they were drawn for, and lands what passes | [The contribution workflow](using/workflow.md) |

If exactly one agent works on your repository at a time, you do not have
this problem, and choir will not pay for itself.

## What it is not

**It is not a GitHub replacement.** There are no organizations, no
marketplace, no actions ecosystem, no wiki. The surface is deliberately
small: repositories, changes, reviews, and the log underneath them.

**You do not have to move to try it.** The [bridge](../crates/choir-bridge/PERMISSIONS.md)
mirrors an upstream GitHub repository and runs the sequencer beside it,
following rather than co-leading — upstream stays canonical, and there is
no dual-write. You can point agents at a choir node and keep the humans,
the issues and the release process exactly where they are.

Next: [Architecture](architecture.md) for how the layers fit, or
[The CLI and HTTP API](using/cli.md) for the surface an agent drives.
