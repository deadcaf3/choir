# demo: the same twenty agents, against git alone and against choir

A two-pane terminal demo for one skeptic in particular: the engineer who
says "worktrees and branches give agents isolation, then you merge, what
does choir add?" Isolation is free and the left pane concedes it. The
demo is about what happens when twenty isolated branches have to become
one `main`: at the conflict, at the infrastructure failure, and in what
you can prove afterwards.

- **Left pane, his workflow done well.** A bare repo with
  `receive.denyNonFastForwards`, twenty worktrees on twenty branches, and
  a careful maintainer that merges in order and runs the tests after
  every merge, skipping a branch that conflicts and paging its author.
- **Right pane, choir.** The same twenty branches proposed to a node as
  `refs/for/main/<agent>/<topic>`, landed by one queue round.

Nothing is mocked or emulated. Every line on either side is the output
of git, curl, or the node. There is no fake forge: an emulated merge
queue would be a strawman to this audience, so the comparison is git
itself on the left and the properties a forge cannot add on the right.

## Run it

```bash
demo/run.sh                  # build into demo/.target, then play; Enter advances
demo/run.sh --auto           # advance by itself, 6 s per beat
demo/run.sh --auto 10        # 10 s per beat
demo/run.sh --no-build       # skip cargo; use the binaries as built
demo/run.sh --dump           # no screen: play everything and print both panes
demo/run.sh --port N         # if 8447 is taken
demo/run.sh --agents N       # default 20
```

Keys while playing: Enter or Space for the next beat, `a` toggles
autoplay, `q` quits. The screen needs 90 by 24 or more; 120 by 40 is
comfortable. One Ghostty tab is enough; there is no tmux.

Idempotent: each run kills the node the previous take left behind,
wipes `demo/.run/`, mints fresh keys, starts a fresh node. One take at
a time: a second one refuses to start while the first is playing,
because they would share `demo/.run/` and wipe each other mid-beat.
A failure in any beat stops the take with one line and the node log
path; it does not play on. Hermetic:
loopback only, no TLS, no network. Needs `cargo`, `git`, `curl`,
`python3` (standard library only; the screen is `curses`).

The first run builds `choir-node` and `choir` into `demo/.target/`, a
target dir of the demo's own, which takes a minute or two. Every later
run is seconds. The dir is private on purpose: a target dir shared
across checkouts hands whoever builds last the `debug/choir-node` path,
and a take then runs a node built from somebody else's sources, which
happened while writing this.

## The beats

Each beat plays both panes at once. Machine time for the whole take is
about 35 s; the recording is as long as your pauses.

| beat | what happens | what to say |
|------|--------------|-------------|
| 0 | left: bare repo, fast-forward only. right: node up. Same seed on both: `config.toml`, `test.sh`, twenty `svc/` files | one substrate, two integration models |
| 1 | twenty worktrees, twenty branches, twenty pushes on each side; agents 03 and 07 also change the greeting line | isolation is free, and neither side rejects a push. Concede it |
| 2 | left: the maintainer merges 01 to 20 in order with tests after each; 07 conflicts, is skipped, its author paged. right: one round; 07 evicted as `Conflict`, the other nineteen land behind it. Counters: merged, blocked, CI runs, wall time, and the sequencer's decision latency | the conflict is a verdict in the round, not a stop. Wall time is a wash at this scale and the take prints it either way |
| 3 | left: agent-12 needs 07's change and can only get it by taking on 07's conflict in its own worktree. right: 07 pushes the conflict to `main` as-is; the node's web view reads it three-sided; 12 builds on top and lands; 07 resolves later | nobody can build on a conflict that lives on a branch. Here it is a value on `main` that anyone can pick up |
| 4 | left: `git log`, and what it proves: parent pointers, no signatures, no test verdicts. right: the op log by kind, the last landings decoded, and `choir log --verify` over all of it | the commit graph is honest and good. The chain is what a forge does not give you: every landing and every check verdict, verified offline with one key |
| 5 | two more branches: 05 breaks the test, 09 adds a note. Both CI runners lose their exec bit. left: both merges go red (exit 126), both authors paged. right: `Errored`, nothing evicted. Runners back: left reverts 05 and merges 09; right evicts 05 as `CiFailure` and lands 09 | a red runner is not a red change. Only the change at fault pays |

The closing lines are the concession and the claim, one per pane: for
disjoint work at a low conflict rate, branches and a merge queue are
fine; the claim is what happens at the conflict, at the infra failure,
and in what you can prove after.

## Numbers you will see, and what they mean

- **Pushes rejected: 0 on both sides.** Branch-per-agent has no push
  contention. Do not pretend otherwise.
- **Left integration: 19 merges, 19 CI runs, one after another, about
  5 s.** Serial by construction.
- **Right round: 19 landings, 20 CI runs in one speculative train,
  about 15 s.** Slower, and the take says so. The time is git process
  overhead: the speculator checks out and merges each candidate in a
  worktree and the runner provisions a worktree per job, about seven
  `git` invocations per candidate at macOS process-spawn cost. An
  in-memory `git merge-tree --write-tree` speculator would remove most
  of it; it is the obvious next optimisation and is not done.
- **Sequencer decision latency, p50 about 2 ms, p99 under the 100 ms
  gate**, read from `/api/view`'s `sequencer_lag` for this process.
  Percentiles are power-of-two bucket upper bounds, so they
  over-estimate by at most 2x and never under-estimate.
- **`choir log --verify`: about 95 entries, chain holds, all signatures
  verified**, with the node's public key as the only input.

## What changed in the node for this demo

Both are real fixes, both tested in `crates/choir-node/tests/it/node_queue.rs`.

1. **A queue landing moves git's own ref in the same request.** Before,
   git's ref lagged until startup reconciliation, and every push to the
   branch in between lost its CAS. `queue_api::run` writes the ref after
   the round, CAS'd on the round's base, and answers `git_lag`.
2. **Landed proposals do not re-enter later rounds.** Proposal refs
   survive their landing by design, but the node builds a fresh queue
   per round and forgot what had landed, so every later round re-merged
   them as no-ops. `ProposalRound::already_integrated` reads ancestry
   from git and the round skips those; the answer names them under
   `already_integrated`.

## Gaps still open

1. **`TreeEntry::Conflict` and `Commit::resolves` are not reachable from
   git or the API.** The git-side form of the same fact is a merge
   commit with diff3 markers, which the browse surface renders
   three-sided (`browse.rs`, `Region::Conflict`); beat 3 shows that.
   The resolution is a plain child commit with no `resolves` link, and
   the narration does not claim one.
2. **The window is not observable from the node.** `window_trace` is not
   in the endpoint's answer and the per-round queue starts at 20 each
   round. "Not halved on `Errored`" is true in `MergeQueue::drain` and
   cannot be shown on this surface; beat 5 shows its consequence.
3. **`choir log` prints raw entries**; `show.py log` decodes them.
4. **`choir checks` wants a bare 40-hex oid** while the view's keys carry
   the `11-` codec prefix; checks are keyed by speculative candidate,
   not by proposal head.
5. **The `<who>` segment of `refs/for/<branch>/<who>/<topic>` is unchecked
   without an ACL.**
6. **Round cost is dominated by git subprocesses**, see the numbers above.

## Files

- `run.sh`: builds into `demo/.target`, then starts `tui.py`.
- `tui.py`: the presenter and every beat; standard library only.
- `show.py`: compact renderings of the node's JSON and HTML, shared
  with `tui.py`; nothing computed.
- `.run/`, `.target/`: everything a take leaves behind; ignored.
