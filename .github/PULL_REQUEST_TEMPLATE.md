<!--
Subject lines are lowercase, imperative, and describe the effect on
behaviour rather than the edit. Look at `git log` before writing one.
No AI or tool attribution in commit messages.
-->

## What changes, and why

<!-- What was wrong, why this is the right shape, and what you
     deliberately did not change. -->

## Checklist

`CONTRIBUTING.md` → **Submitting** is the long form of these three.

- [ ] **`./gate` is green on this tree.** The full lane, not `fast` — its
      receipt describes the tree as of its start, so it was run after the
      last edit. CI runs `./gate quick`, which compiles nothing and is
      therefore not evidence that this builds.
- [ ] **Decisions touched are named.** If a comment on the code here
      cites a `D<n>`, that row was read. If this change contradicts one,
      it is argued below rather than quietly reversed.
- [ ] **New dependencies are justified.** Dependencies are added
      reluctantly here; a test needing randomness hand-rolls an xorshift
      rather than pulling `rand`. If this adds one, say what it buys and
      what was tried without it. Tick this if there are none.

## Decisions touched

<!-- D<n>, and what this does to it. "None" is a fine answer. -->

## Anything you were unsure about

<!-- Expect questions about dependencies, seams without a second
     implementation, and any hashed struct you added. -->
