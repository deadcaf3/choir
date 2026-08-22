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
//! CI runs behind [`executor::CiExecutor`] (the CI executor seam, D18).
//! A verdict is four cases rather than a bool, because the queue's
//! response to a test failure and to a provider fault must differ: only
//! [`executor::Verdict::Failed`] ejects a change, and ejection takes
//! everything that transitively depends on it. Tests use
//! [`executor::Synthetic`]; [`local::LocalRunner`] runs real
//! subprocesses; production wires the spindle/Firecracker executor.
//!
//! # Where this sits
//!
//! `docs/architecture.md` is the map of the whole workspace.
//! This crate is L2, the speculative merge queue (D5).
//!
//! It builds on [`choir_hash`], [`choir_merge`], [`choir_oplog`], [`choir_sequencer`], [`choir_store`] and [`choir_view`].

pub mod blast;
pub mod corpus;
pub mod differential;
pub mod differential_ledger;
pub mod envelope;
pub mod executor;
pub mod identity;
pub mod local;
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
    /// Why the drain stopped early without blaming a change (D18).
    ///
    /// Set when the executor produced no verdict, or an inconclusive
    /// one: a provider that could not be reached, a batch whose
    /// verdict count did not match its jobs, a job that errored or
    /// timed out. Everything not landed is back in the queue, in
    /// order, and nothing was rejected on account of it -- which is the
    /// distinction the old `bool` seam could not make.
    pub provider_error: Option<String>,
}

/// Single-shard speculative merge queue over one file.
pub struct MergeQueue {
    base: String,
    pipeline: Pipeline,
    window: usize,
    queue: std::collections::VecDeque<Change>,
    memory: memory::ResolutionMemory,
    landed: std::collections::BTreeSet<String>,
    /// Where window changes are reported. Derived data: the queue never
    /// reads it back, and a journal that dropped every record would
    /// change no landing decision.
    journal: Box<dyn choir_sequencer::journal::Journal>,
    /// What to run for each candidate state (D18).
    template: JobTemplate,
}

/// How the queue turns a speculative state into a [`executor::Job`].
///
/// The queue knows which tree to test; it does not know what "test"
/// means for a repository, and inventing a default command would make
/// the wrong one silent. The default template has no command, which
/// every real executor answers with [`executor::Verdict::Errored`] --
/// loud, and never mistaken for a change that failed.
#[derive(Debug, Clone, Default)]
pub struct JobTemplate {
    /// The command, as argv.
    pub command: Vec<String>,
    /// Exactly what the child sees.
    pub environment: std::collections::BTreeMap<String, String>,
    /// Wall-clock ceiling per job. `None` uses
    /// [`executor::DEFAULT_DEADLINE`].
    pub deadline: Option<std::time::Duration>,
    /// Whether these jobs may write a shared build cache.
    pub may_write_cache: bool,
}

impl JobTemplate {
    /// The job testing `change` applied on `state`.
    #[must_use]
    pub fn job_for(&self, change: &Change, state: &str) -> executor::Job {
        let mut job = executor::Job::new(
            choir_hash::ContentHash::blake3(state.as_bytes()),
            self.command.clone(),
        );
        job.label = change.id.to_string();
        job.environment = self.environment.clone();
        job.may_write_cache = self.may_write_cache;
        if let Some(d) = self.deadline {
            job.deadline = d;
        }
        job
    }
}

impl MergeQueue {
    /// Moves the speculation window and records why.
    ///
    /// A window that shrank is the queue's loudest signal, and a size
    /// alone cannot say whether it shrank because a build failed or
    /// grew because a train landed. The cause travels with the number
    /// so the answer does not have to be inferred from timing.
    fn resize(&mut self, to: usize, cause: &str) {
        let from = self.window;
        self.window = to;
        if from != to {
            self.journal
                .record(choir_sequencer::journal::Event::WindowResize {
                    from,
                    to,
                    cause: cause.to_string(),
                });
        }
    }

    /// Sends window changes to `journal` instead of discarding them.
    pub fn set_journal(&mut self, journal: Box<dyn choir_sequencer::journal::Journal>) {
        self.journal = journal;
    }

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
            journal: Box::new(choir_sequencer::journal::NullJournal),
            template: JobTemplate::default(),
        }
    }

    /// Installs the job template (D18): what to run for each candidate
    /// state. Without one, every job is refused by the executor for
    /// having no command.
    pub fn set_job_template(&mut self, template: JobTemplate) {
        self.template = template;
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

    /// The current speculation window.
    ///
    /// Exposed because [`QueueReport::window_trace`] cannot answer for
    /// it in every case: a drain that stops on a provider fault breaks
    /// out before writing the trace, so a test reading only the report
    /// asserts over an empty vector and passes whatever the window did.
    /// One did, until a mutation that halved the window on a fault was
    /// not caught.
    #[must_use]
    pub fn window(&self) -> usize {
        self.window
    }

    /// Drains the queue: speculatively merges up to `window` changes, runs CI
    /// on each against its speculative state, merges the passing prefix, and
    /// halves the window + retests behind on any failure.
    ///
    /// Every merged change is recorded through the single-writer `sequencer`
    /// (payload = the merged state), preserving the platform's total order.
    pub fn drain(
        &mut self,
        ci: &mut dyn executor::CiExecutor,
        sequencer: &Sequencer,
    ) -> QueueReport {
        let mut merged = Vec::new();
        let mut rejected = Vec::new();
        let mut window_trace = Vec::new();
        let mut ci_runs = 0usize;
        let mut merge_invocations = 0usize;
        let mut replayed = Vec::new();
        let mut provider_error: Option<String> = None;
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

            // "Assume-pass": CI for every train member runs against its own
            // speculative state, so every member costs a run even when an
            // earlier member fails (its result is then discarded). The whole
            // train goes to the executor in one call, which is what lets a
            // provider be concurrent -- the one-job-at-a-time signature this
            // replaced made the parallelism this cost model assumes
            // impossible to implement, at any window size (D18).
            let jobs: Vec<executor::Job> = train
                .iter()
                .map(|(change, state)| self.template.job_for(change, state))
                .collect();
            ci_runs += jobs.len();
            let verdicts = match ci.run(&jobs) {
                Ok(v) if v.len() == jobs.len() => v,
                // Index alignment is the whole contract of the batch call.
                // A provider that returns a different count has attributed
                // somebody's result to somebody else, and no verdict in the
                // batch can be trusted.
                Ok(v) => {
                    provider_error = Some(format!(
                        "executor returned {} verdicts for {} jobs",
                        v.len(),
                        jobs.len()
                    ));
                    Vec::new()
                }
                Err(e) => {
                    provider_error = Some(e.to_string());
                    Vec::new()
                }
            };
            if provider_error.is_some() {
                // No verdicts at all, so nothing here is evidence about any
                // change. Requeue the whole train in order and stop. The
                // window is deliberately not halved: halving is the response
                // to changes that fail, and none of these did.
                for (change, _) in train.into_iter().rev() {
                    self.queue.push_front(change);
                }
                break;
            }

            // The first member that did not pass. Only a `Failed` may evict:
            // ejection is permanent for the change *and* everything that
            // transitively depends on it, so it is reserved for a verdict
            // that is actually about the change's content. An inconclusive
            // one stalls the train instead.
            let stop_at = verdicts
                .iter()
                .position(|v| !matches!(v, executor::Verdict::Passed));
            if let Some(i) = stop_at {
                if !verdicts[i].evicts() {
                    provider_error = Some(verdicts[i].to_string());
                    for (change, state) in train.drain(..i) {
                        self.base = state.clone();
                        self.landed.insert(identity::change_identity(&change));
                        handle.submit(&change.workspace, state.into_bytes());
                        merged.push(change.id);
                        self.resize(self.window + 1, "green prefix landed");
                    }
                    for (change, _) in train.into_iter().rev() {
                        self.queue.push_front(change);
                    }
                    break;
                }
            }
            let failure_at = stop_at;

            match failure_at {
                None => {
                    // Whole train is green: merge it all.
                    for (change, state) in train {
                        self.base = state.clone();
                        self.landed.insert(identity::change_identity(&change));
                        handle.submit(&change.workspace, state.into_bytes());
                        merged.push(change.id);
                        self.resize(self.window + 1, "train landed clean");
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
                        self.resize(self.window + 1, "green prefix landed");
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
                        self.resize((self.window / 2).max(1), "combined build failed");
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
            provider_error,
        }
    }
}

/// Convenience: drain `changes` through a fresh queue + sequencer and return
/// the report plus the sequencer's op count (which must equal merged count).
pub fn run_batch(
    base: &str,
    changes: Vec<Change>,
    ci: &mut dyn executor::CiExecutor,
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
