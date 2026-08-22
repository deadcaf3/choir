//! The speculation seam (D5): what it means to put one change on top of
//! another.
//!
//! [`MergeQueue`](crate::MergeQueue) is a policy — a window that grows
//! by one and halves on failure, a dependency-aware blast radius, a
//! refusal to land the same change twice, and a rule that only a real
//! test failure evicts anybody. None of that policy is about text. It
//! was written against text anyway, because the queue called
//! [`choir_merge::Pipeline`] directly and [`crate::Change`] carried two
//! file bodies, which is the Phase-0 abstraction its doc comment always
//! named. The consequence was structural rather than cosmetic: nothing
//! holding a repository could construct a queue, so the queue was
//! composed into nothing and its test numbers were a simulation of a
//! train rather than a train.
//!
//! A state here is an opaque [`String`]. The text implementation reads
//! it as file content; the git one reads it as a commit id. The queue
//! reads it as neither — it moves states around, hands them to CI, and
//! never looks inside one. That is the whole trick, and it is why this
//! seam needs no generic parameter and no change to
//! [`crate::Change`].
//!
//! Per the house rule, a seam is real when a conformance suite and a
//! second implementation both pass: [`TextSpeculator`] is the extracted
//! original and [`crate::git::GitSpeculator`] is the second.

use choir_hash::ContentHash;
use choir_merge::{safety, MergeOutcome, Pipeline};

use crate::Change;

/// What a speculator did with one change.
#[derive(Debug)]
pub enum Step {
    /// The change applied. This is the new speculative state.
    Advanced(String),
    /// First-class conflict: the change is evicted for its author to
    /// resolve, and the train continues without it (D6).
    Conflict,
    /// A resolution applied edits the change never proposed.
    ///
    /// Distinct from [`Step::Conflict`] because the author's change may
    /// be perfectly fine and the *strategy* at fault, which is a
    /// different escalation. An implementation with a single
    /// non-negotiable merge rule — git's — can never return this, and
    /// saying so is more useful than pretending the case is shared.
    Unsafe {
        /// The strategy whose resolution violated the invariant.
        strategy: &'static str,
        /// The unattributable lines, as evidence for escalation.
        violation: safety::Violation,
    },
    /// The speculator could not form an opinion: a broken worktree, a
    /// state that is not a state, git not on the path.
    ///
    /// Not a conflict and not a failure. Blaming an author for our own
    /// inability to run a merge is the mistake D18's four verdicts
    /// exist to prevent, one level up; the queue stalls on this exactly
    /// as it stalls on a provider fault, and nobody is evicted.
    Unavailable(String),
}

/// How the queue puts one change on top of a speculative state.
///
/// Implementations own three things the queue deliberately does not
/// know: what a merge is, when two changes are the same change, and how
/// a state is named to CI.
pub trait Speculator: Send {
    /// Stable identifier, for reports and errors.
    fn name(&self) -> &'static str;

    /// Merges `proposed` — authored against `base` — onto `onto`.
    ///
    /// `base` is passed rather than derived because a 3-way text merge
    /// has no way to find it, and merging against the train's advanced
    /// tip instead would misread the proposal as reverting everything
    /// merged ahead of it. An implementation that can find its own
    /// merge base is free to ignore the argument, and git's does.
    fn step(&mut self, base: &str, onto: &str, proposed: &str) -> Step;

    /// A position-independent identity for `change`.
    ///
    /// The same logical edit authored against two different bases —
    /// before and after the train rewrote the tip under it — must
    /// produce the same identity, or a rebased resubmission lands
    /// twice.
    fn identity(&self, change: &Change) -> String;

    /// The content hash naming `state`.
    ///
    /// This is what CI is asked about and what a landing records as the
    /// workspace head, so the two cannot disagree about which tree a
    /// verdict was for. It is not a hash of *our* choosing: a git state
    /// must be named by its git oid, or every check written down is
    /// about an id no git client can resolve.
    fn subject(&self, state: &str) -> ContentHash;
}

/// The original speculator: one file's content, merged 3-way.
///
/// Every state is a full file body, and `base`/`onto`/`proposed` are
/// the three sides of [`choir_merge::Pipeline::merge`]. The safety
/// check that policed strategies stays here, because it is a statement
/// about lines and has no meaning for an implementation whose states
/// are not text.
pub struct TextSpeculator {
    pipeline: Pipeline,
}

impl TextSpeculator {
    /// A speculator over the given strategy pipeline — the D4/D19
    /// widening seam, with structured or LLM slots appended by the
    /// caller. Every resolution is still safety-checked here, which is
    /// what makes the non-deterministic slots admissible at all.
    #[must_use]
    pub fn new(pipeline: Pipeline) -> Self {
        Self { pipeline }
    }
}

impl Default for TextSpeculator {
    fn default() -> Self {
        Self::new(Pipeline::default_v1())
    }
}

impl Speculator for TextSpeculator {
    fn name(&self) -> &'static str {
        "text"
    }

    fn step(&mut self, base: &str, onto: &str, proposed: &str) -> Step {
        let resolution = self.pipeline.merge(base, onto, proposed);
        match resolution.outcome {
            MergeOutcome::Resolved(next) => {
                // A resolution may only apply edits the change
                // proposed. A strategy that quietly reverts work
                // already in the speculative state is evicted like a
                // conflict, before CI ever sees it.
                match safety::check(base, onto, proposed, &next) {
                    safety::SafetyVerdict::Upholds => Step::Advanced(next),
                    safety::SafetyVerdict::Violation(violation) => Step::Unsafe {
                        strategy: resolution.strategy,
                        violation,
                    },
                }
            }
            MergeOutcome::Conflict { .. } => Step::Conflict,
            // `Pipeline::merge` returns the last conflict rather than
            // an unavailable strategy; it panics before it can hand one
            // back.
            MergeOutcome::Unavailable(_) => unreachable!(),
        }
    }

    fn identity(&self, change: &Change) -> String {
        crate::identity::change_identity(change)
    }

    fn subject(&self, state: &str) -> ContentHash {
        ContentHash::blake3(state.as_bytes())
    }
}
