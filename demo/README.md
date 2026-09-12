# demo: the same twenty agents, against git alone and against choir

A two-column terminal demo for one skeptic in particular: the engineer who
says "worktrees and branches give agents isolation, then you merge, what
does choir add?" Isolation is free and the left column concedes it. The
demo is about what happens when twenty isolated branches have to become
one `main`: at the conflict, at the infrastructure failure, and in what
you can prove afterwards.

- **Left column, his workflow done well.** A bare repo whose `main` is
  fast-forward only, twenty worktrees on twenty branches, and a batch
  queue the way bors runs one: merge everything pending in order, test
  once, and on red bisect for the culprit, paging its author. A branch
  that conflicts is skipped and its author paged.
- **Right column, choir.** The same twenty branches proposed to a node as
  `refs/for/main/<agent>/<topic>`, landed by one queue round: a
  speculative train, every candidate tested on top of the ones before
  it, all at the same time.

The work is the same on both sides and it is not tidy on purpose:
every agent enables its service; the overlap agents (03 and 07 by
default, `--overlap` for more) each rewrite the same greeting line;
agent-07 also switches on a feature that agent-12's later change needs;
and agent-18 takes agent-15's port, a change that merges cleanly and
fails the tests only next to 15's. The repository's `test.sh` has three
checks, each a line of shell, so nothing about a verdict is hidden.

GitHub's merge queue is a train like choir's, not a bisecting batch, so
against it the beat 2 wall times are closer than what the left column
shows. The differences that survive that are beats 3 and 5, and what
beat 4 can verify.

Nothing is mocked or emulated. Every line on either side is the output
of git, curl, or the node. There is no fake forge: an emulated merge
queue would be a strawman to this audience, so the comparison is git
itself on the left and the properties a forge cannot add on the right.

## Run it

```bash
demo/run.sh                  # build into demo/.target, then play, both columns at once
demo/run.sh --no-build       # skip cargo; use the binaries as built
demo/run.sh --plain          # no colour, no live rows: for a pipe or a file
demo/run.sh --port N         # default 8447, or the next free port
demo/run.sh --agents N       # default 20, at least 18
demo/run.sh --overlap K      # agents that rewrite the greeting line, default 2
demo/run.sh --ci-seconds S   # how long test.sh takes, default 3
```

Nothing waits for a key. Both columns run their whole script from the
first second, each at its own pace, and every row goes straight into
the terminal's own scrollback as it happens, stamped with the seconds
since the take began. Scroll up afterwards and the two columns read as
one timeline: what the right side was doing at 20 s while the left was
still merging. The last line of the screen is live while a side works,
a spinner, its counters and a clock, and it never enters the
scrollback. Ctrl-C stops the take and the node. One Ghostty tab is
enough; there is no tmux and no curses. Any width from 90 columns
works; wider is nicer, the columns split the width in half.

The transcript of the last take is `demo/.run/take.log`: every row of
both columns with its time, the status lines, and the terminal width.
That file is what to send when a take looked wrong.

Idempotent: each run kills the node the previous take left behind,
wipes `demo/.run/`, mints fresh keys, starts a fresh node. One take at
a time: a second one refuses to start while the first is playing,
because they would share `demo/.run/` and wipe each other mid-beat. A
failure on one side is shown in that column and the other side plays
on; the take then exits nonzero with the transcript and node log paths.
Hermetic: loopback only, no TLS, no network. Needs `cargo`, `git`,
`curl`, `python3` (standard library only).

The first run builds `choir-node` and `choir` into `demo/.target/`, a
target dir of the demo's own, which takes a minute or two. Every later
run is seconds. The dir is private on purpose: a target dir shared
across checkouts hands whoever builds last the `debug/choir-node` path,
and a take then runs a node built from somebody else's sources, which
happened while writing this.

## The beats

Each column plays its six beats in order, with its own headers and its
own clock; the two are not kept in step, and that is the comparison.
With the default 3 s test the right column is finished in about 32 s
and the left in about 43 s; the gap is the bisection in beat 2 and the
serial batches in beat 5.

| beat | what happens | what to say |
|------|--------------|-------------|
| 0 | left: bare repo, `main` fast-forward only through an `update` hook. right: node up. Same seed on both: `config.toml`, `test.sh` with three checks, twenty `svc/` files | one substrate, two integration models |
| 1 | twenty worktrees, twenty branches, twenty pushes on each side; 03 and 07 rewrite the greeting line, 07 switches the farewell feature on, 18 takes 15's port | isolation is free, and neither side rejects a push. Concede it |
| 2 | left: one batch of 19 (07 skipped for its conflict), the test goes red on the port clash, five bisection runs find 18, one more lands the rest: 6 test runs in series. right: one round, 21 CI runs at once, 07 evicted as `Conflict`, 18 as `CiFailure`, 18 land. Then 18 fixes its port on both sides | a conflict and a semantic failure are both verdicts inside the round, each with its reason. The batch queue finds them one test run at a time |
| 3 | left: agent-12 adds farewell, its test fails (feature off), the feature is on only in 07's skipped branch, merging that is 07's conflict. right: 07 lands the conflict on `main` as-is, the node's view reads it three-sided, 12 pulls it, its farewell check passes, the one red is 07's markers, 12 lands; 07 resolves, tests green | the dependency is real and the only way through on a branch is to take on somebody else's conflict. Here the conflict is a value on `main` and 12's work does not wait on it |
| 4 | left: `git log`: parent pointers, no signatures, no verdicts; a forge adds check runs and an audit log you trust. right: the op log by kind, the last landings decoded, `choir log --verify` with the node's one key | the chain proves order and that nothing was rewritten, and you verify it offline. It does not prove who pushed: say so before he does |
| 5 | three pending changes: 05 breaks the greeting test, 09 adds a note, 18 is back with a free port. Both runners lose their exec bit. left: 6 test runs, exit 126 each, all three authors paged. right: `Errored`, nothing evicted. Runners back: left bisects 05 out in 4 runs and lands the rest; right evicts 05 as `CiFailure`, lands 09 and 18, 5 runs | a red runner is not a red change. Only the change at fault pays, and nobody's time is spent finding that out |

When both sides are done, each column prints its start-to-finish time,
then the concession and the claim, one per column: for disjoint work at
a low conflict rate, branches and a merge queue are fine; the claim is
what happens at the conflict, at the infra failure, and in what you can
prove after.

## Numbers you will see, and what they mean

- **Pushes rejected: 0 on both sides.** Branch-per-agent has no push
  contention. Do not pretend otherwise.
- **Left beat 2: 18 merged, 1 blocked, 1 evicted, 6 test runs one after
  another, about 23 s** with the 3 s test: one run for the batch, four
  to bisect the port clash down to agent-18, one to land the rest.
- **Right beat 2: 18 landed, 2 evicted, 21 CI runs in one train, about
  11 s.** Twenty candidates plus one re-speculation behind the failure,
  all at once, plus about 4 s of fixed cost that is git process
  overhead: the speculator checks out and merges each candidate in a
  worktree and the runner provisions a worktree per job. An in-memory
  `git merge-tree --write-tree` speculator would remove most of that;
  it is the obvious next optimisation and is not done.
- **The two scale differently, which is why the test takes time.** With
  an instant test the round is the slower side, from that fixed cost. A
  real suite takes minutes, and then the left column pays it once per
  bisection step and the right once per round.
- **Beat 5 with the runner broken: left 6 runs, 3 authors paged; right
  3 runs, `Errored`, nobody paged.** Exit 126 is instant, so this costs
  the left no wall time here; in a real queue it costs a human each.
- **`choir log --verify`: about 130 entries, chain holds, all signatures
  verified**, with the node's public key as the only input.
- **`--overlap 5`** puts four skips and four evictions in beat 2 instead
  of one each, and the rebases after are one batch or one round on
  either side. The conflict rate becomes a knob you turn in front of
  him instead of a number you assert. Your own positioning says the
  real rate is unmeasured; this does not measure it either.

## What changed in the node for this demo

All three are small and tested in `crates/choir-node/tests/it/node_queue.rs`.

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
3. **The round's answer carries `ci_runs`.** The train's own cost
   multiplier, per D23, without a borrowed rate; the right column prints
   it instead of assuming one run per candidate.

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
- `tui.py`: the two-column presenter and both scripts; standard library only.
- `show.py`: compact renderings of the node's JSON and HTML, shared
  with `tui.py`; nothing computed.
- `.run/take.log`: a transcript of the last take, every row either
  column showed with its time, the status lines and the terminal
  width. Read it, or send it, when a take looked wrong.
- `.run/`, `.target/`: everything a take leaves behind; ignored.
