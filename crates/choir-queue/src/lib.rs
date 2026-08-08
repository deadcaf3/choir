//! L2 speculative merge queue (plan.md D5, Key Finding 4).
//!
//! Zuul-style dependent pipeline: changes are tested in parallel against the
//! speculative future state produced by everything queued ahead of them,
//! "exactly as if they had been tested one at a time." Window sizing follows
//! the TCP-flow-control-inspired algorithm from Zuul's docs: start at 20,
//! +1 per successful merge, halved per failure.
//!
//! Conflict policy (plan.md §B): a change whose merge conflicts is evicted as
//! a first-class conflict for its author to resolve; it is never silently
//! resolved and never blocks the changes behind it, which are retested
//! against a speculative state without it.
//!
//! CI is a caller-supplied verdict function behind [`CiRunner`] (the CI
//! executor seam, D18); tests use synthetic verdicts, production wires the
//! spindle/Firecracker executor.

pub mod blast;
pub mod corpus;

use choir_merge::{MergeOutcome, Pipeline};
use choir_oplog::MemLog;
use choir_sequencer::Sequencer;

/// Default initial speculation window (Zuul's documented default).
pub const DEFAULT_WINDOW: usize = 20;

/// A change submitted to the queue: a full-file edit carrying its own base.
///
/// The change's `base` is the content it was authored against (its parent),
/// which is what the 3-way merge must use; merging against the queue's
/// advanced tip as base would misread the proposal as reverting work merged
/// ahead of it. The single-file model is the Phase-0 abstraction; the real
/// tree diff arrives with the L1 integration.
#[derive(Debug, Clone)]
pub struct Change {
    /// Stable change identifier.
    pub id: u64,
    /// Workspace (agent) that produced the change.
    pub workspace: String,
    /// The content this change was authored against (its parent state).
    pub base: String,
    /// The file content this change proposes.
    pub proposed: String,
}

/// Why a change left the queue without merging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejection {
    /// Merge produced a first-class conflict; author must resolve and resubmit.
    Conflict,
    /// CI failed for this change on its speculative state.
    CiFailure,
}

/// The CI executor seam (D18): pass/fail verdict for a candidate state.
pub trait CiRunner {
    /// Runs CI for `change` applied on `speculative_state`; true = pass.
    fn verdict(&mut self, change: &Change, speculative_state: &str) -> bool;
}

impl<F: FnMut(&Change, &str) -> bool> CiRunner for F {
    fn verdict(&mut self, change: &Change, speculative_state: &str) -> bool {
        self(change, speculative_state)
    }
}

/// Outcome of draining a queue.
#[derive(Debug)]
pub struct QueueReport {
    /// Changes merged, in merge order.
    pub merged: Vec<u64>,
    /// Changes rejected, with reasons.
    pub rejected: Vec<(u64, Rejection)>,
    /// Final repository state after all merges.
    pub final_state: String,
    /// Window size after each head decision (for observing TCP dynamics).
    pub window_trace: Vec<usize>,
    /// Total CI executions, including retests behind failures.
    pub ci_runs: usize,
}

/// Single-shard speculative merge queue over one file.
pub struct MergeQueue {
    base: String,
    pipeline: Pipeline,
    window: usize,
    queue: std::collections::VecDeque<Change>,
}

impl MergeQueue {
    /// Creates a queue over `base` content with the default window.
    pub fn new(base: &str) -> Self {
        Self {
            base: base.to_string(),
            pipeline: Pipeline::default_v1(),
            window: DEFAULT_WINDOW,
            queue: std::collections::VecDeque::new(),
        }
    }

    /// Enqueues a change.
    pub fn submit(&mut self, change: Change) {
        self.queue.push_back(change);
    }

    /// Number of changes waiting.
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// Whether the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Drains the queue: speculatively merges up to `window` changes, runs CI
    /// on each against its speculative state, merges the passing prefix, and
    /// halves the window + retests behind on any failure.
    ///
    /// Every merged change is recorded through the single-writer `sequencer`
    /// (payload = the merged state), preserving the platform's total order.
    pub fn drain(&mut self, ci: &mut dyn CiRunner, sequencer: &Sequencer) -> QueueReport {
        let mut merged = Vec::new();
        let mut rejected = Vec::new();
        let mut window_trace = Vec::new();
        let mut ci_runs = 0usize;
        let handle = sequencer.handle();

        while !self.queue.is_empty() {
            // Build the speculative train: up to `window` changes, each merged
            // onto the state produced by the changes ahead of it.
            let take = self.window.min(self.queue.len());
            let mut train: Vec<(Change, String)> = Vec::with_capacity(take);
            let mut speculative = self.base.clone();
            let mut train_rejects: Vec<(u64, Rejection)> = Vec::new();
            for _ in 0..take {
                let change = self.queue.pop_front().unwrap();
                match self
                    .pipeline
                    .merge(&change.base, &speculative, &change.proposed)
                    .outcome
                {
                    MergeOutcome::Resolved(next) => {
                        speculative = next.clone();
                        train.push((change, next));
                    }
                    MergeOutcome::Conflict { .. } => {
                        // First-class conflict: evict, do not block the train.
                        train_rejects.push((change.id, Rejection::Conflict));
                    }
                    MergeOutcome::Unavailable(_) => unreachable!(),
                }
            }
            rejected.extend(train_rejects);

            // "Assume-pass": CI for every train member launches in parallel
            // against its speculative state, so every member costs a run even
            // when an earlier member fails (its result is then discarded).
            let mut failure_at: Option<usize> = None;
            for (i, (change, state)) in train.iter().enumerate() {
                ci_runs += 1;
                if !ci.verdict(change, state) && failure_at.is_none() {
                    failure_at = Some(i);
                }
            }

            match failure_at {
                None => {
                    // Whole train is green: merge it all.
                    for (change, state) in train {
                        self.base = state.clone();
                        handle.submit(&change.workspace, state.into_bytes());
                        merged.push(change.id);
                        self.window += 1;
                    }
                }
                Some(i) => {
                    // Merge the green prefix, reject the failure, requeue the
                    // rest for retesting against a state without the failure.
                    for (change, state) in train.drain(..i) {
                        self.base = state.clone();
                        handle.submit(&change.workspace, state.into_bytes());
                        merged.push(change.id);
                        self.window += 1;
                    }
                    let (failed, _) = train.remove(0);
                    rejected.push((failed.id, Rejection::CiFailure));
                    self.window = (self.window / 2).max(1);
                    for (change, _) in train.into_iter().rev() {
                        self.queue.push_front(change);
                    }
                }
            }
            window_trace.push(self.window);
        }

        QueueReport {
            merged,
            rejected,
            final_state: self.base.clone(),
            window_trace,
            ci_runs,
        }
    }
}

/// Convenience: drain `changes` through a fresh queue + sequencer and return
/// the report plus the sequencer's op count (which must equal merged count).
pub fn run_batch(
    base: &str,
    changes: Vec<Change>,
    ci: &mut dyn CiRunner,
) -> (QueueReport, u64) {
    let mut queue = MergeQueue::new(base);
    for c in changes {
        queue.submit(c);
    }
    let sequencer = Sequencer::spawn(Box::new(MemLog::new()));
    let report = queue.drain(ci, &sequencer);
    let log = sequencer.shutdown();
    (report, log.len())
}
