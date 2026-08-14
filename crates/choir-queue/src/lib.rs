//! L2 speculative merge queue (DECISIONS.md D5).
//!
//! Dependent speculative pipeline: changes are tested in parallel against the
//! speculative future state produced by everything queued ahead of them,
//! "exactly as if they had been tested one at a time." Window sizing follows
//! a TCP-flow-control-inspired algorithm: start at 20,
//! +1 per successful merge, halved per failure.
//!
//! Conflict policy (DECISIONS.md): a change whose merge conflicts is evicted as
//! a first-class conflict for its author to resolve; it is never silently
//! resolved and never blocks the changes behind it, which are retested
//! against a speculative state without it.
//!
//! CI is a caller-supplied verdict function behind [`CiRunner`] (the CI
//! executor seam, D18); tests use synthetic verdicts, production wires the
//! spindle/Firecracker executor.

pub mod blast;
pub mod corpus;
pub mod differential;
pub mod differential_ledger;
pub mod envelope;
pub mod identity;
pub mod memory;

use choir_merge::{safety, MergeOutcome, Pipeline};
use choir_oplog::MemLog;
use choir_sequencer::Sequencer;

/// Default initial speculation window (a documented default).
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
    /// Ids of queued changes this one declares it depends on.
    /// Declared-only — the queue never infers dependencies
    /// from overlap; inference is a separate decision. Empty (the
    /// default for all existing traffic) keeps the legacy
    /// halve-and-retest behavior on failure; see [`MergeQueue::drain`].
    pub depends: Vec<u64>,
}

/// Why a change left the queue without merging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejection {
    /// Merge produced a first-class conflict; author must resolve and resubmit.
    Conflict,
    /// CI failed for this change on its speculative state.
    CiFailure,
    /// A strategy resolved, but its output edits the speculative state beyond
    /// what the change proposed: silently reverted
    /// or injected lines. Treated like a conflict — evicted first-class,
    /// never landed, never blocking the train — but reported separately
    /// because the author's change may be fine and the *strategy* at fault.
    SafetyViolation {
        /// The strategy whose resolution violated the invariant.
        strategy: &'static str,
        /// The unattributable lines, as evidence for escalation.
        violation: choir_merge::safety::Violation,
    },
    /// Ejected because it (transitively) declared a dependency on a
    /// change whose combined build failed. Not a verdict
    /// on this change itself: resubmit once the dependency is fixed.
    DependencyEjection {
        /// The CI-failing change this one depends on.
        on: u64,
    },
    /// A change with this [`identity::change_identity`] already landed
    /// through this queue: the resubmission — typically
    /// the same edit rebased after the train rewrote the tip — is
    /// refused without re-merging, so it cannot land twice.
    AlreadyLanded,
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
    /// Total strategy-pipeline invocations. A change resolved from
    /// [`memory::ResolutionMemory`] does not invoke the pipeline, which
    /// is what a test counts to prove a replay happened.
    pub merge_invocations: usize,
    /// Changes whose conflict was resolved from memory, in train order.
    /// Every one of them still ran CI: replay produces a candidate,
    /// never a landing.
    pub replayed: Vec<u64>,
}

/// Single-shard speculative merge queue over one file.
pub struct MergeQueue {
    base: String,
    pipeline: Pipeline,
    window: usize,
    queue: std::collections::VecDeque<Change>,
    memory: memory::ResolutionMemory,
    landed: std::collections::BTreeSet<String>,
}

impl MergeQueue {
    /// Creates a queue over `base` content with the default window.
    pub fn new(base: &str) -> Self {
        Self::with_pipeline(base, Pipeline::default_v1())
    }

    /// Creates a queue with a caller-supplied strategy pipeline — the D4/D19
    /// widening seam (structured or LLM slots appended by the caller). Every
    /// resolution is still safety-checked in [`MergeQueue::drain`], which is
    /// what makes the non-deterministic slots admissible at all.
    pub fn with_pipeline(base: &str, pipeline: Pipeline) -> Self {
        Self {
            base: base.to_string(),
            pipeline,
            window: DEFAULT_WINDOW,
            queue: std::collections::VecDeque::new(),
            memory: memory::ResolutionMemory::new(),
            landed: std::collections::BTreeSet::new(),
        }
    }

    /// Installs a resolution memory (item B): a conflict whose triple it
    /// remembers is replayed as a candidate instead of re-conflicting.
    /// The default is an empty memory, which changes nothing.
    pub fn set_memory(&mut self, memory: memory::ResolutionMemory) {
        self.memory = memory;
    }

    /// Seeds a landed change identity (item 4), for a queue picking up
    /// where an earlier instance left off. `drain` records identities of
    /// everything it lands through the same set.
    pub fn mark_landed(&mut self, identity: String) {
        self.landed.insert(identity);
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
        let mut merge_invocations = 0usize;
        let mut replayed = Vec::new();
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
                // Stable change identity (item 4): a resubmission of an
                // already-landed change — the same position-independent
                // edit, however rebased — is refused before any merge
                // work, so the train neither re-merges nor duplicates it.
                if self.landed.contains(&identity::change_identity(&change)) {
                    train_rejects.push((change.id, Rejection::AlreadyLanded));
                    continue;
                }
                // Resolution memory (item B): a remembered triple is
                // replayed without re-invoking the strategy pipeline.
                // The replay is a *candidate* — it joins the train and
                // runs the same CI verdict as everything else, and it
                // can only exist because an author already committed
                // this exact resolution as a value (item A's link).
                if let Some(remembered) =
                    self.memory
                        .recall(&change.base, &speculative, &change.proposed)
                {
                    let next = remembered.to_string();
                    speculative = next.clone();
                    replayed.push(change.id);
                    train.push((change, next));
                    continue;
                }
                merge_invocations += 1;
                let resolution = self
                    .pipeline
                    .merge(&change.base, &speculative, &change.proposed);
                match resolution.outcome {
                    MergeOutcome::Resolved(next) => {
                        // Merge-safety invariant:
                        // a resolution may only apply edits the change
                        // proposed. A strategy that quietly reverts work
                        // already in the speculative state is evicted like
                        // a conflict, before CI ever sees it.
                        match safety::check(&change.base, &speculative, &change.proposed, &next) {
                            safety::SafetyVerdict::Upholds => {
                                speculative = next.clone();
                                train.push((change, next));
                            }
                            safety::SafetyVerdict::Violation(violation) => {
                                train_rejects.push((
                                    change.id,
                                    Rejection::SafetyViolation {
                                        strategy: resolution.strategy,
                                        violation,
                                    },
                                ));
                            }
                        }
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
                        self.landed.insert(identity::change_identity(&change));
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
                        self.landed.insert(identity::change_identity(&change));
                        handle.submit(&change.workspace, state.into_bytes());
                        merged.push(change.id);
                        self.window += 1;
                    }
                    let (failed, _) = train.remove(0);
                    let failed_id = failed.id;
                    rejected.push((failed_id, Rejection::CiFailure));

                    // Dependency-aware ejection: when any
                    // waiting change declares dependencies, the failure
                    // ejects exactly the failing change plus everything
                    // that (transitively) depends on it, and the window
                    // is not halved — the blast radius is named by the
                    // declarations, not guessed by shrinking the train.
                    // Survivors are requeued in order and land in this
                    // same drain. With no declarations anywhere (all
                    // existing traffic) the legacy halving path runs
                    // unchanged.
                    let declares = |c: &Change| !c.depends.is_empty();
                    let dependency_mode = declares(&failed)
                        || train.iter().any(|(c, _)| declares(c))
                        || self.queue.iter().any(declares);
                    if dependency_mode {
                        let mut ejected = std::collections::BTreeSet::from([failed_id]);
                        loop {
                            let dependent = |c: &Change| {
                                !ejected.contains(&c.id)
                                    && c.depends.iter().any(|d| ejected.contains(d))
                            };
                            let next: Vec<u64> = train
                                .iter()
                                .map(|(c, _)| c)
                                .chain(self.queue.iter())
                                .filter(|c| dependent(c))
                                .map(|c| c.id)
                                .collect();
                            if next.is_empty() {
                                break;
                            }
                            ejected.extend(next);
                        }
                        for (change, _) in train.into_iter().rev() {
                            if ejected.contains(&change.id) {
                                rejected.push((
                                    change.id,
                                    Rejection::DependencyEjection { on: failed_id },
                                ));
                            } else {
                                self.queue.push_front(change);
                            }
                        }
                        let waiting = std::mem::take(&mut self.queue);
                        for change in waiting {
                            if ejected.contains(&change.id) {
                                rejected.push((
                                    change.id,
                                    Rejection::DependencyEjection { on: failed_id },
                                ));
                            } else {
                                self.queue.push_back(change);
                            }
                        }
                    } else {
                        self.window = (self.window / 2).max(1);
                        for (change, _) in train.into_iter().rev() {
                            self.queue.push_front(change);
                        }
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
            merge_invocations,
            replayed,
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
