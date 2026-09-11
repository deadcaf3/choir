# demo: why a skeptic's forge is not enough for many agents

A scripted, re-runnable terminal demo against a live choir node, driven by
plain `git` clients. Target: a screen recording under two minutes, for an
engineer whose model is "branches, worktrees and per-branch CI on a forge
are enough". Every beat ends on the thing a forge does not do.

- **A. Ordering that never blocks.** A conflicting proposal is evicted from
  the queue while the change behind it lands in the same round. The
  conflict then lands on `main` as a committed value the node reads as
  one, with base and both sides. Work builds on top of it without
  waiting. The resolution is a later commit. The whole sequence is a
  signed, hash-chained order anyone verifies offline with one public key.
- **B. Blame only what was tested.** A CI *infrastructure* failure is
  `Errored`, not `Failed`: the queue requeues the changes and records
  why. A real red test is `Failed` and evicted, and the green change in
  the same round still lands.

## Run it

```bash
demo/run.sh              # plays every beat, no waits; about 10 s of machine time
demo/run.sh --pause      # waits for Enter after each beat: use this to record
demo/run.sh --no-build   # skip cargo; use the binaries as built (see below)
demo/run.sh --port N     # if 8447 is taken
```

Idempotent: each run kills the node the previous take left behind, wipes
`demo/.run/`, mints fresh keys, starts a fresh node. Hermetic: loopback
only, no TLS, no network. Needs `cargo`, `git`, `curl`, `python3`.

The first run builds `choir-node` and `choir` into `demo/.target/`, a
target dir of the demo's own, which takes a minute or two; the script
says so on its first line. Every later run is seconds. The dir is private on
purpose: a target dir shared across checkouts hands whoever builds last
the `debug/choir-node` path, and a take then runs a node built from
somebody else's sources (this happened while writing this). Build once,
then record with `--no-build`. The node is left running after the last
beat so you can poke at it (`demo/.run/node.log`,
`http://127.0.0.1:8447/`); the next take replaces it.

Recording setup: 100 columns by 40 rows fits every beat on one screen.
The script prints its own prompts (`agent2 $ ...`), so the shell's `PS1`
does not matter; run `clear` and then the script. Colors follow
`NO_COLOR`.

Everything on screen is real output. `demo/show.py` only trims the node's
JSON (`/api/view`, `/api/log`, `/api/queue/run`) and the web view's HTML
to the rows a beat is about; `choir view` and `choir log` print the same
documents in full.

## Shot list

Timestamps assume the suggested pauses; the machine time inside each
beat is well under a second, so say so rather than filling it. The
narration lines beginning `# on a forge ...` are the differentiators;
each is a generic claim about forge merge queues and PR flows, not a
statement about one vendor's current behaviour, so keep them that way
on camera.

| beat | at   | what happens                                                                                   | what it proves                                                                                                                                                        | pause |
|------|------|------------------------------------------------------------------------------------------------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------|-------|
| 0    | 0:00 | node up on loopback, one empty repo, `healthz 200`                                             | there is a daemon with a merge queue; git is the only client                                                                                                          | 5 s   |
| 1    | 0:05 | three clones of one base; agent1 edits the line, `git push`                                    | an ordinary push through smart-HTTP, sequenced as a node-signed `SetRef` op                                                                                           | 8 s   |
| 2    | 0:13 | agent2 (same line) and agent3 (new file) both propose; one queue round                         | **A, the queue never blocks.** `rejected [(1, Conflict)]`, `merged [2]`: agent2 is evicted first-class, agent3 behind it lands, main moves. A forge would not queue agent2 at all | 15 s  |
| 3    | 0:28 | agent2 pulls, gets CONFLICT, commits the markers, pushes to main; the web view reads that file | **A, conflict as a value.** The node renders base, HEAD and the other side as a conflict at line 1, not as a broken file. The push was accepted like any ref move    | 20 s  |
| 4    | 0:48 | agent3 pulls the conflict commit, adds to NOTES, pushes; agent2 resolves; `git log --graph`   | **A, nobody waits.** A change whose parent is an unresolved conflict lands; the resolution follows as one more commit; `test.sh` is green again                        | 20 s  |
| 5    | 1:08 | `log_tail` from the round: landing, conflict, work on top, resolution; `choir log --verify`    | **A, the order.** Each entry names the tip it replaced and the entry before it. 22 entries, chain holds, 22 signatures verified offline with the node's public key    | 15 s  |
| 6a   | 1:23 | two new proposals; the runner loses its exec bit; one round                                    | **B, part 1.** `stalled: could not start ... Permission denied`, `merged []`, `rejected []`, both candidates `Errored`, `choir checks` exits 4                          | 15 s  |
| 6b   | 1:38 | runner restored; same round                                                                    | **B, part 2.** Red test: `CiFailure`, evicted. Green proposal: landed, and `git pull` sees it. Only the change at fault pays                                           | 12 s  |
|      | 1:50 | done                                                                                           |                                                                                                                                                                       |       |

What to say over beat 4, because it is the whole pitch: git would let
you commit markers anywhere. What a forge does not give you is an order
in which the eviction, the committed conflict, the work on top of it and
the resolution are four accepted entries that nobody had to wait for,
verifiable in beat 5 without trusting the server.

## What `cargo run -p choir-demo` already covered

`choir-demo` is an in-process walkthrough: `MemLog`, `MemStore`, three
keys, one signed op rejected, a `TreeEntry::Conflict` commit built by
hand and accepted as a workspace head, time travel, then one real `git
push` through a `Node`. It shows claim A in the view model, not through
git, and it does not run the merge queue at all, so claim B is not in
it. Nothing here reuses it; this demo is git clients against a daemon
binary, which is the audience's frame.

## Fixed while building this

**A queue landing now moves git's own ref in the same request.** Before,
`POST /api/queue/run` landed through the log and left git's ref where it
was until the next startup reconcile; in between, `git ls-remote` showed
the old tip, `GET /api/ref-agreement` said `git_behind`, and every
`git push` to the branch lost its CAS. `queue_api::run` now writes the
ref after the round, CAS'd on the round's base at the git level too, and
answers `git_lag: null` (or the reason it could not). The log still
leads; git follows sooner. `node_queue::the_endpoint_runs_a_round_and_makes_its_own_tree`
asserts the ref moved and that a push after the round succeeds. Beats 3
and 6b depend on it.

## Gaps still open

Candidates for the register, in the order they bit. Each is what the
current surface does, verified on this tree, not a proposal.

1. **A landed proposal is re-merged every later round.** Proposal refs
   survive their landing by design (the test says sweeping them would
   decide on the author's behalf), but the node builds a fresh
   `MergeQueue` per round, so its `landed` set is empty each time and
   the landed proposal joins the next train again as a no-op merge. The
   script has agent3 retire its landed proposal before beat 6a so the
   round shows only the two new ones.
2. **`TreeEntry::Conflict` and `Commit::resolves` are not reachable from
   git or the API.** They live in `choir-view`'s commit model and are
   constructed only by `choir-demo` and the queue's resolution memory.
   The git-side form of the same fact is a merge commit carrying diff3
   markers, which the browse surface renders three-sided
   (`browse.rs`, `Region::Conflict`); beat 3 shows that. Beat 4's
   resolution is a plain child commit with no `resolves` link, and the
   narration does not claim one.
3. **The window is not observable from the node.** `QueueReport.window_trace`
   exists but `/api/queue/run` does not return it, and the per-round
   queue starts at 20 every round regardless of the last verdict. "Not
   halved on `Errored`" is true in `MergeQueue::drain` and cannot be
   shown on this surface; 6a shows its consequence instead.
4. **`choir log` prints raw entries** (`payload_hex`), one JSON object per
   line, wider than any screen; there is no human rendering of an op.
   `show.py log` decodes the payload for the screen.
5. **`choir checks <git-oid>` wants a bare 40-hex oid**, while every key in
   the view's `checks` and `refs` sections carries the `11-` codec prefix.
   The script strips it.
6. **Checks are keyed by candidate, not by proposal.** The queue records
   its verdict on the speculative merge commit it tested, so
   `choir checks <proposal head>` answers `unreported` even after a round
   judged that proposal.
7. **The `<who>` segment of `refs/for/<branch>/<who>/<topic>` is unchecked
   without an ACL.** Any anonymous client can push as `agent3`; the
   ownership rule (`proposal_denial`) only applies to `propose`-level
   grants. Fine for a loopback demo, worth knowing before it is shown as
   attribution.

## Files

- `run.sh`: the whole take, narration inline (`# ...` lines before each
  command say what happens and what to watch).
- `show.py`: compact renderings of the node's JSON and HTML; nothing
  computed.
- `.run/`: everything a take leaves behind; ignored, recreated each run.
