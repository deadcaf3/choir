//! Single-writer per-repo sequencer (DECISIONS.md D2): the Phase-0 spike.
//!
//! One OS thread owns the op log; all workspaces submit ops through cloned
//! handles and receive the assigned (seq, hash) synchronously. This is the
//! in-process second implementation required by the actor-runtime seam (D3);
//! the Rivet-backed implementation must pass the same conformance tests.
//!
//! # Examples
//!
//! ```
//! use choir_oplog::MemLog;
//! use choir_sequencer::Sequencer;
//!
//! let sequencer = Sequencer::spawn(Box::new(MemLog::new()));
//! let handle = sequencer.handle();
//! let accepted = handle.submit("agent-1", b"op".to_vec());
//! assert_eq!(accepted.seq, 0);
//! let log = sequencer.shutdown();
//! assert_eq!(log.len(), 1);
//! ```
//!
//! # Where this sits
//!
//! `docs/architecture.md` is the map of the whole workspace.
//! This crate is the single writer itself (D2): one thread per repository decides the total order.
//!
//! It builds on [`choir_oplog`].

pub mod fairness;
pub mod journal;
pub mod lag;

use choir_oplog::{ContentHash, OpEntry, OpLog, Witness, FORMAT_VERSION};
use lag::LagMeter;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// An operation offered to the sequencer on an attribution channel.
pub struct Submission {
    /// Signature-covered collaboration channel submitting the op.
    pub channel: String,
    /// Opaque operation body, stored as [`OpEntry::payload`].
    pub payload: Vec<u8>,
    /// Author signature over `(channel, payload)`; carried into
    /// [`OpEntry::author_sig`]. `None` for unsigned (pre-L8) clients.
    pub author_sig: Option<Witness>,
}

/// Admission policy run inside the writer thread, before an op is
/// ordered. This is where L8 signature verification and L1 view CAS
/// plug into L2 without the sequencer depending on either layer.
pub trait SubmitPolicy: Send {
    /// Accepts or rejects a submission. Runs on the writer thread, so a
    /// stateful policy (e.g. one holding a cached materialized view) sees
    /// submissions in their final total order.
    ///
    /// # Errors
    ///
    /// A human-readable rejection reason, returned to the submitter.
    fn check(&mut self, sub: &Submission) -> Result<(), String>;

    /// Observes an entry that was just appended (for cached-state
    /// policies to fold). Default: ignore.
    ///
    /// `hash` is the entry's content hash, which the sequencer has just
    /// computed to answer the submitter. It is passed rather than left
    /// to the policy to recompute: a policy that indexes entries by hash
    /// would otherwise re-serialize every entry on the write path, which
    /// the allocation budget already caught once.
    fn accepted(&mut self, _entry: &OpEntry, _hash: &ContentHash) {}

    /// What the last [`SubmitPolicy::check`] identified about its
    /// submission: `(actor_id, op_type)`, for the journal.
    ///
    /// Called by the sequencer immediately after `check`, on the same
    /// thread, so a policy that already derived these while checking
    /// hands them over rather than deriving them twice. That matters:
    /// `actor_id` comes from verifying a signature, and re-verifying
    /// every op to describe it would double the cost of the one step
    /// that is genuinely expensive.
    ///
    /// The default answers nothing, which is honest for a policy that
    /// never looked. A journal then records the decision without an
    /// author, and an entry with a null `actor_id` says exactly that.
    fn subject(&self) -> (Option<String>, Option<String>) {
        (None, None)
    }

    /// Whether an entry another writer already sequenced can be folded
    /// here, asked by [`SequencerHandle::replicate`] before it appends
    /// one. Default: yes.
    ///
    /// **This is not admission, and must not become it.** The writer that
    /// sequenced the entry decided whether it was admissible; a replica
    /// that re-decided would be a second writer with its own opinion of
    /// the order, which is the thing a replica exists not to be. So no
    /// signature, grant or quota is consulted here, and
    /// [`SubmitPolicy::check`] is never called on a replicated entry.
    ///
    /// What is asked is narrower and has to be asked before the append:
    /// can this replica's projection take the entry at all. A replica
    /// built from an older release may not know an op kind the writer
    /// admitted, and appending what [`SubmitPolicy::accepted`] then cannot
    /// fold would leave a log that no longer replays. Refusing here halts
    /// replication at that entry, which is the only honest outcome: a
    /// replica that skipped what it did not understand would be a fork.
    ///
    /// # Errors
    ///
    /// Why the entry cannot be folded, returned to the replicator.
    fn replicable(&mut self, _entry: &OpEntry) -> Result<(), String> {
        Ok(())
    }
}

/// The default policy: everything is admitted (localhost/dev shape).
pub struct AdmitAll;

impl SubmitPolicy for AdmitAll {
    fn check(&mut self, _sub: &Submission) -> Result<(), String> {
        Ok(())
    }
}

/// The sequencer's acknowledgement of a durably ordered op.
#[derive(Debug)]
pub struct Accepted {
    /// Position assigned in the total order.
    pub seq: u64,
    /// Content hash of the appended entry (the new log head).
    pub hash: ContentHash,
    /// Time from dequeue to append; the merge-decision latency the Phase-0
    /// gate measures (<100 ms target, CI excluded).
    pub decision_latency: Duration,
}

/// An entry another writer already sequenced, and the hash the page that
/// carried it claimed for it.
///
/// The claim travels beside the entry rather than being recomputed and
/// trusted: [`SequencerHandle::replicate`] refuses an entry whose content
/// does not hash to what its source said, which is SYNC.md's second check
/// made again at the one place that appends.
#[derive(Debug, Clone)]
pub struct Sequenced {
    /// The entry, exactly as its writer sequenced it: `seq`, `parent`,
    /// payload and signature untouched.
    pub entry: OpEntry,
    /// The content hash its source claimed for it.
    pub hash: ContentHash,
}

/// What a [`SequencerHandle::replicate`] call appended.
#[derive(Debug)]
pub struct Replicated {
    /// Entries appended and folded, all of the batch.
    pub appended: u64,
    /// Content hash of the last one appended, the log's new head; `None`
    /// for an empty batch.
    pub head: Option<ContentHash>,
}

/// Why a [`SequencerHandle::replicate`] call stopped.
///
/// Everything before [`ReplicateError::seq`] in the batch was appended,
/// folded and made durable; nothing at or after it was. A replica keeps
/// what it verified and stops there, rather than skipping on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicateError {
    /// The sequence number of the entry that was refused.
    pub seq: u64,
    /// Why: a position or parent that does not continue this log, a hash
    /// that is not the entry's, a projection that cannot fold it, or a
    /// storage failure.
    pub reason: String,
    /// How many entries of the batch were appended before it.
    pub appended: u64,
}

impl std::fmt::Display for ReplicateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "seq {}: {}", self.seq, self.reason)
    }
}

/// Most ops admitted before the writer stops draining and syncs.
///
/// Bounds the latency a submitter can inherit from the ops queued ahead
/// of it: without a cap, a sustained burst would keep the drain loop fed
/// and the batch would never close. 256 is a starting point chosen to sit
/// far under the 100 ms decision-latency gate at the measured per-op cost,
/// not a tuned value.
const MAX_BATCH: usize = 256;

/// Microseconds, saturating.
///
/// The journal carries an integer rather than a float so a `jq` filter
/// can compare and sum it without surprises. `u64` microseconds covers
/// half a million years; a decision that outlived that has other
/// problems.
fn as_micros(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

/// One admitted op waiting on the batch's durability barrier: where to
/// answer, what to answer with, and when it was dequeued (so the ack can
/// be measured through the barrier, not just up to the append).
type PendingAck = (mpsc::Sender<Result<Accepted, String>>, Accepted, Instant);

/// Fills `turns` with the order to serve one drained batch in: one op
/// from each actor in rotation, until every op has a turn.
///
/// The quota is what *bounds* how long one actor can make another wait.
/// This is the finer half of the same idea, and it only matters within a
/// single wake-up: when the writer surfaces to find one actor's burst
/// interleaved with somebody's single op, arrival order would serve the
/// burst first, and rotation serves the single op second instead of
/// last. Per-actor order is preserved exactly — each bucket is FIFO —
/// so an actor's own ops never overtake each other.
///
/// The single-actor case, which is most of them and every benchmark,
/// takes the identity path and allocates nothing. That is deliberate:
/// `alloc_budget` gates the per-op allocation count, and a scheduler
/// that charged every op for a fairness decision nobody needed would be
/// paying to solve a problem it does not have.
fn round_robin(
    queued: &[Option<Queued>],
    buckets: &mut Vec<(Arc<str>, std::collections::VecDeque<usize>)>,
    turns: &mut Vec<usize>,
) {
    turns.clear();
    // One actor (or none): arrival order already is round-robin.
    let mut lone = true;
    let mut first: Option<&Arc<str>> = None;
    for item in queued.iter().flatten() {
        match first {
            None => first = Some(&item.1),
            Some(seen) if Arc::ptr_eq(seen, &item.1) => {}
            Some(_) => {
                lone = false;
                break;
            }
        }
    }
    if lone {
        // Filled slots only. Arrival order is already the answer here,
        // but handing back an index to a slot that holds nothing would
        // make the caller's `take()` the only thing standing between
        // this and a panic.
        turns.extend(
            queued
                .iter()
                .enumerate()
                .filter_map(|(index, item)| item.as_ref().map(|_| index)),
        );
        return;
    }

    buckets.clear();
    for (index, item) in queued.iter().enumerate() {
        let Some((_, actor, _)) = item else { continue };
        // Pointer equality, not string equality: the same actor hands
        // back the same interned `Arc` (see [`fairness::Quotas::admit`]),
        // so this compares a word rather than a string. A key re-interned
        // mid-batch would split into two buckets, which costs that actor
        // a slightly larger share of one batch and nothing else.
        match buckets.iter_mut().find(|(key, _)| Arc::ptr_eq(key, actor)) {
            Some((_, indexes)) => indexes.push_back(index),
            None => {
                let mut indexes = std::collections::VecDeque::new();
                indexes.push_back(index);
                buckets.push((actor.clone(), indexes));
            }
        }
    }
    while turns.len() < queued.len() {
        let before = turns.len();
        for (_, indexes) in buckets.iter_mut() {
            if let Some(index) = indexes.pop_front() {
                turns.push(index);
            }
        }
        // Every bucket is empty; the remainder are `None` slots. Without
        // this the loop would spin forever on a batch containing them.
        if turns.len() == before {
            break;
        }
    }
}

// `Submit` is ~208 bytes and `Shutdown` carries none, which is what the
// lint objects to. Its premise does not hold here: these are transient
// channel messages, at most `MAX_BATCH` in flight, and `Shutdown` is sent
// exactly once per sequencer lifetime — so the "wasted" space is one
// message, once, not a cost paid per stored value.
//
// Boxing the payload to even them out would put a heap allocation on the
// write path, which is the one path this workspace measures allocations
// on (`choir-node/tests/alloc_budget.rs`). Paying that on every op to
// save 208 bytes on a message sent at shutdown is the wrong trade.
//
// It first fired when D45 added `Witness::credential_key`, taking
// `Submission` past the 200-byte default threshold.
#[allow(clippy::large_enum_variant)]
enum Command {
    /// The op, the interned actor bucket holding its quota slot (see
    /// [`fairness`]), and where to answer.
    Submit(Submission, Arc<str>, mpsc::Sender<Result<Accepted, String>>),
    /// Entries another writer already sequenced, to append as they are.
    Replicate(
        Vec<Sequenced>,
        mpsc::Sender<Result<Replicated, ReplicateError>>,
    ),
    Shutdown,
}

/// Appends a replicated batch, in order, stopping at the first entry that
/// does not continue this log. Runs on the writer thread and nowhere else.
///
/// Three checks before each append, none of them admission: the entry's
/// `seq` is this log's next position, its `parent` is this log's head, and
/// its content hashes to what its source claimed. Then the policy is asked
/// whether its projection can fold it, and only then is it appended and
/// handed to [`SubmitPolicy::accepted`], exactly as an admitted op is.
fn replicate_batch(
    log: &mut dyn OpLog,
    policy: &mut dyn SubmitPolicy,
    batch: Vec<Sequenced>,
    durability_failed: &mut bool,
    writer_flag: &AtomicBool,
) -> Result<Replicated, ReplicateError> {
    let mut appended = 0u64;
    let mut head = None;
    for Sequenced { entry, hash } in batch {
        let seq = entry.seq;
        let refuse = |reason: String| ReplicateError {
            seq,
            reason,
            appended,
        };
        if *durability_failed {
            return Err(refuse(
                "log is not durable: writer stopped accepting".to_string(),
            ));
        }
        if seq != log.len() {
            return Err(refuse(format!(
                "entry is at seq {seq}, and this log's next position is {}",
                log.len()
            )));
        }
        if entry.parent != log.head() {
            return Err(refuse(
                "entry's parent is not this log's head, so it continues another chain".to_string(),
            ));
        }
        let computed = entry.content_hash();
        if computed != hash {
            return Err(refuse(format!(
                "entry hashes to {}, and its source claimed {}",
                computed.to_hex(),
                hash.to_hex()
            )));
        }
        policy.replicable(&entry).map_err(refuse)?;
        match log.append(entry) {
            Ok(stored) => {
                // Storage first, projections second, as on the admit
                // path: the policy folds the entry the backend holds.
                let last = log
                    .last()
                    .expect("a successful append exposes its newest entry");
                policy.accepted(last, &stored);
                appended += 1;
                head = Some(stored);
            }
            Err(error) => {
                *durability_failed = true;
                writer_flag.store(true, Ordering::Relaxed);
                return Err(ReplicateError {
                    seq,
                    reason: format!("log append failed: {error:?}"),
                    appended,
                });
            }
        }
    }
    Ok(Replicated { appended, head })
}

/// One drained command awaiting its turn in the writer's round-robin.
type Queued = (Submission, Arc<str>, mpsc::Sender<Result<Accepted, String>>);

/// Cloneable client handle; one per workspace/agent.
#[derive(Clone)]
pub struct SequencerHandle {
    tx: mpsc::Sender<Command>,
    poisoned: Arc<AtomicBool>,
    quotas: fairness::Quotas,
}

impl SequencerHandle {
    /// Whether a durability barrier has failed, after which this writer
    /// refuses every submission.
    ///
    /// Exposed rather than acted on. The sequencer's job is to say that it
    /// can no longer promise durability; deciding what a *node* does about
    /// that — keep serving reads, exit so supervision restarts it, page
    /// someone — is a lifecycle policy, and a library linked by every
    /// embedder including the test suite is the wrong place to make it.
    ///
    /// The daemon polls this and exits, so launchd's `KeepAlive` restarts
    /// into the same replay path a `kill -9` already exercises. Without an
    /// observer, fail-closed is invisible to supervision: the process
    /// stays up refusing everything, and a transient fsync error becomes
    /// permanent downtime that looks like uptime.
    #[must_use]
    pub fn durability_failed(&self) -> bool {
        self.poisoned.load(Ordering::Relaxed)
    }

    /// Blocks until the sequencer has durably ordered the op. Unsigned
    /// convenience wrapper over [`SequencerHandle::try_submit`]; only
    /// valid under a policy that admits unsigned ops.
    ///
    /// # Panics
    ///
    /// Panics if the sequencer thread has shut down or the policy
    /// rejects the op.
    pub fn submit(&self, channel: &str, payload: Vec<u8>) -> Accepted {
        self.try_submit(channel, payload, None)
            .expect("policy admits this op")
    }

    /// Blocks until the sequencer has ordered the op or rejected it.
    ///
    /// # Errors
    ///
    /// The policy's rejection reason, or a [`fairness`] rejection naming
    /// the quota when this actor already has too many ops awaiting a
    /// decision. The quota is checked here, on the calling thread, before
    /// anything is sent: an over-quota submitter is *answered*, never
    /// parked, so a client always has something to react to.
    ///
    /// # Panics
    ///
    /// Panics if the sequencer thread has already shut down.
    pub fn try_submit(
        &self,
        channel: &str,
        payload: Vec<u8>,
        author_sig: Option<Witness>,
    ) -> Result<Accepted, String> {
        let sub = Submission {
            channel: channel.to_string(),
            payload,
            author_sig,
        };
        let actor = self.quotas.admit(fairness::Quotas::actor_of(&sub))?;
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(Command::Submit(sub, actor, reply_tx))
            .expect("sequencer thread alive");
        reply_rx.recv().expect("sequencer replies before dropping")
    }

    /// Appends entries another writer already sequenced, in order, through
    /// this writer thread, and folds each one.
    ///
    /// This is how a replica takes a log without becoming a second writer
    /// (invariant 5): the entries keep the `seq` and `parent` their own
    /// writer gave them, and this thread is still the only thing that
    /// appends. Each must continue this log exactly (next position, this
    /// head as parent, content hashing to the claimed hash) and the
    /// policy's [`SubmitPolicy::replicable`] must say it can fold it.
    /// [`SubmitPolicy::check`] is never run: admission was the source
    /// writer's decision.
    ///
    /// The whole batch shares one durability barrier and is answered after
    /// it. Replicated entries are not admission decisions, so they are not
    /// journalled, counted against a quota, or measured by the lag meter.
    ///
    /// # Errors
    ///
    /// The first entry that failed, with how many before it were appended.
    /// Nothing after it is attempted.
    ///
    /// # Panics
    ///
    /// Panics if the sequencer thread has already shut down.
    pub fn replicate(&self, entries: Vec<Sequenced>) -> Result<Replicated, ReplicateError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(Command::Replicate(entries, reply_tx))
            .expect("sequencer thread alive");
        reply_rx.recv().expect("sequencer replies before dropping")
    }

    /// The per-actor admission quotas this handle submits through.
    #[must_use]
    pub fn quotas(&self) -> fairness::Quotas {
        self.quotas.clone()
    }

    /// Offers every submission before waiting for any reply, so the whole
    /// group is already queued when the writer next drains — and therefore
    /// shares one durability barrier instead of one each.
    ///
    /// This is the difference between an N-op request costing N fsyncs and
    /// costing `ceil(N / MAX_BATCH)`. Calling [`SequencerHandle::try_submit`]
    /// in a loop cannot achieve it: each call blocks until its own reply,
    /// so the queue is empty every time the writer looks and every op
    /// becomes its own batch.
    ///
    /// Results are returned in submission order, one per input. Each is
    /// independent: a rejection does not abort the rest, matching the
    /// per-op semantics `/api/submit-batch` already promised.
    ///
    /// # Panics
    ///
    /// Panics if the sequencer thread has already shut down.
    pub fn try_submit_many(&self, subs: Vec<Submission>) -> Vec<Result<Accepted, String>> {
        // A reply channel per op rather than one shared channel: replies
        // are not ordered with respect to each other, because a rejection
        // is answered immediately while an admitted op waits for the
        // barrier. Separate channels keep the result order matching the
        // input order by construction rather than by assumption.
        // The collect is the whole mechanism, not an accident: it forces
        // every submission to be SENT before the first reply is awaited.
        // Consumed lazily this would send one, block on its reply, send
        // the next -- exactly the per-op blocking that made the batch
        // endpoint pay one fsync per op.
        #[allow(clippy::needless_collect)]
        let waiting: Vec<Result<mpsc::Receiver<Result<Accepted, String>>, String>> = subs
            .into_iter()
            .map(|sub| {
                // An over-quota member is refused in place rather than
                // failing the group: the endpoint's documented semantics
                // are per-op, and one member's ceiling is not the
                // group's problem.
                let actor = self.quotas.admit(fairness::Quotas::actor_of(&sub))?;
                let (reply_tx, reply_rx) = mpsc::channel();
                self.tx
                    .send(Command::Submit(sub, actor, reply_tx))
                    .expect("sequencer thread alive");
                Ok(reply_rx)
            })
            .collect();
        waiting
            .into_iter()
            .map(|slot| match slot {
                Ok(rx) => rx.recv().expect("sequencer replies before dropping"),
                Err(refused) => Err(refused),
            })
            .collect()
    }
}

/// Owns the single writer thread; the only component allowed to append to
/// the repo's [`OpLog`].
pub struct Sequencer {
    tx: mpsc::Sender<Command>,
    /// Set by the writer when a durability barrier fails; read by the
    /// daemon through [`SequencerHandle::durability_failed`].
    poisoned: Arc<AtomicBool>,
    /// Written by the writer for every accepted op; read and drained by
    /// the daemon through [`Sequencer::lag`].
    lag: Arc<LagMeter>,
    /// Per-actor admission ceilings, shared with every handle and with
    /// the writer thread that releases their slots.
    quotas: fairness::Quotas,
    thread: Option<JoinHandle<Box<dyn OpLog>>>,
}

impl Sequencer {
    /// Starts the writer thread over `log` with the [`AdmitAll`] policy.
    pub fn spawn(log: Box<dyn OpLog>) -> Self {
        Self::spawn_with_policy(log, Box::new(AdmitAll))
    }

    /// Starts the writer thread over `log`; every submission passes
    /// through `policy` before it is ordered.
    ///
    /// Records nothing. Use [`Sequencer::spawn_with_journal`] to observe
    /// decisions.
    pub fn spawn_with_policy(log: Box<dyn OpLog>, policy: Box<dyn SubmitPolicy>) -> Self {
        Self::spawn_with_journal(log, policy, Box::new(journal::NullJournal))
    }

    /// [`Sequencer::spawn_with_policy`], recording every decision to
    /// `journal`.
    ///
    /// The journal is derived data: it is written after the decision is
    /// made, never consulted, and its failure cannot refuse an op. See
    /// [`journal`] for why the I/O belongs on another thread.
    pub fn spawn_with_journal(
        mut log: Box<dyn OpLog>,
        mut policy: Box<dyn SubmitPolicy>,
        journal: Box<dyn journal::Journal>,
    ) -> Self {
        let (tx, rx) = mpsc::channel::<Command>();
        let poisoned = Arc::new(AtomicBool::new(false));
        let writer_flag = poisoned.clone();
        let lag = Arc::new(LagMeter::new());
        let writer_lag = lag.clone();
        let quotas = fairness::Quotas::default();
        let writer_quotas = quotas.clone();
        let thread = std::thread::spawn(move || {
            let quotas = writer_quotas;
            // Ordered, then made durable, then acknowledged. Everything
            // admitted in one pass through the loop shares a single
            // `sync`, so the fsync cost is paid once per batch rather
            // than once per op.
            let mut acks: Vec<PendingAck> = Vec::new();
            let mut stopping = false;
            // Set by a failed durability barrier and never cleared. See
            // the refusal in the admit path below for why this is
            // one-way: the alternative is ordering ops that may not
            // survive, which is the bug this whole change exists to close.
            //
            // The sequencer publishes this state through `writer_flag`;
            // the daemon observes it and exits for supervision. The
            // library itself only refuses subsequent work, which keeps
            // the lifecycle choice with the embedder.
            let mut durability_failed = false;
            // Reused across wake-ups. A fresh buffer per batch would put
            // an allocation on the writer's hot path for no reason.
            let mut queued: Vec<Option<Queued>> = Vec::new();
            let mut turns: Vec<usize> = Vec::new();
            let mut buckets: Vec<(Arc<str>, std::collections::VecDeque<usize>)> = Vec::new();
            while let Ok(first) = rx.recv() {
                let mut cmd = Some(first);
                queued.clear();
                // A replicated batch ends the drain: the submissions
                // already queued are decided first, then the batch, so
                // arrival order between the two is kept.
                let mut replicating = None;
                // Admit the woken command, then drain whatever else is
                // already queued behind it. Nothing is waited for: an idle
                // sequencer still batches exactly one op, so a lone
                // submitter pays no added latency.
                while let Some(current) = cmd.take() {
                    match current {
                        Command::Shutdown => {
                            stopping = true;
                            break;
                        }
                        Command::Submit(sub, actor, reply) => {
                            queued.push(Some((sub, actor, reply)));
                        }
                        Command::Replicate(batch, reply) => {
                            replicating = Some((batch, reply));
                            break;
                        }
                    }
                    if queued.len() >= MAX_BATCH {
                        break;
                    }
                    cmd = rx.try_recv().ok();
                }
                // Depth is counted per wake-up rather than sampled on a
                // timer: this is the writer's own view of how much was
                // already waiting behind it, which is the number a
                // backlog question is actually asking about.
                let drained = queued.len();
                round_robin(&queued, &mut buckets, &mut turns);
                for &index in &turns {
                    let Some((sub, actor, reply)) = queued[index].take() else {
                        continue;
                    };
                    let started = Instant::now();
                    // Fail closed. Once a barrier has failed this writer
                    // can no longer promise anything, so it refuses
                    // *before* appending rather than ordering ops it
                    // cannot persist.
                    if durability_failed {
                        let _ = reply.send(Err(
                            "log is not durable: writer stopped accepting".to_string()
                        ));
                        quotas.release(&actor);
                        continue;
                    }
                    // Cloned only when something will read it:
                    // `String::new()` does not allocate, the clone does,
                    // and this is the writer's per-op path.
                    let journalling = journal.enabled();
                    let workspace = if journalling {
                        sub.channel.clone()
                    } else {
                        String::new()
                    };
                    match policy.check(&sub) {
                        // A rejection touches neither the log nor
                        // durability, so it is answered at once rather
                        // than made to wait for the batch.
                        Err(reason) => {
                            if journalling {
                                let (actor_id, op_type) = policy.subject();
                                journal.record(journal::Event::Decision {
                                    actor_id,
                                    workspace,
                                    op_type,
                                    accepted: false,
                                    reject_reason: Some(reason.clone()),
                                    seq: None,
                                    parent: None,
                                    decision_latency_us: as_micros(started.elapsed()),
                                });
                            }
                            let _ = reply.send(Err(reason));
                        }
                        Ok(()) => {
                            let seq = log.len();
                            let entry = OpEntry {
                                format_version: FORMAT_VERSION,
                                parent: log.head(),
                                seq,
                                channel: sub.channel,
                                payload: sub.payload,
                                witnesses: Vec::new(),
                                author_sig: sub.author_sig,
                            };
                            // `ContentHash::to_hex` formats each digest byte and
                            // therefore allocates repeatedly. Preserve the
                            // journal's zero-cost-disabled contract by building
                            // this display value only when a journal will use it.
                            let parent = journalling
                                .then(|| entry.parent.as_ref().map(ContentHash::to_hex))
                                .flatten();
                            match log.append(entry) {
                                Ok(hash) => {
                                    // Storage first, projections second.
                                    // `check` is deliberately read-only;
                                    // no authoritative cached state may
                                    // observe an entry the backend refused.
                                    let appended = log
                                        .last()
                                        .expect("a successful append exposes its newest entry");
                                    if journalling {
                                        let (actor_id, op_type) = policy.subject();
                                        journal.record(journal::Event::Decision {
                                            actor_id,
                                            workspace,
                                            op_type,
                                            accepted: true,
                                            reject_reason: None,
                                            seq: Some(seq),
                                            parent,
                                            decision_latency_us: as_micros(started.elapsed()),
                                        });
                                    }
                                    policy.accepted(appended, &hash);
                                    acks.push((
                                        reply,
                                        Accepted {
                                            seq,
                                            hash,
                                            decision_latency: started.elapsed(),
                                        },
                                        started,
                                    ));
                                }
                                Err(error) => {
                                    durability_failed = true;
                                    writer_flag.store(true, Ordering::Relaxed);
                                    let reason = format!("log append failed: {error:?}");
                                    if journalling {
                                        let (actor_id, op_type) = policy.subject();
                                        journal.record(journal::Event::Decision {
                                            actor_id,
                                            workspace,
                                            op_type,
                                            accepted: false,
                                            reject_reason: Some(reason.clone()),
                                            seq: None,
                                            parent,
                                            decision_latency_us: as_micros(started.elapsed()),
                                        });
                                    }
                                    let _ = reply.send(Err(reason));
                                }
                            }
                        }
                    }
                    // Decided, so the slot is no longer holding anyone
                    // up. An accepted op still waits on the shared
                    // durability barrier, but by then it is behind
                    // nobody: the quota bounds work queued *ahead* of
                    // another actor, and this op no longer is any.
                    quotas.release(&actor);
                }
                // Only when something was actually queued behind the
                // woken command. A lone submitter wakes the writer for
                // itself constantly, and recording depth 1 for each
                // would bury every interesting line under them.
                if drained > 1 && journal.enabled() {
                    journal.record(journal::Event::QueueDepth { depth: drained });
                }
                let replicated = replicating.map(|(batch, reply)| {
                    let outcome = replicate_batch(
                        log.as_mut(),
                        policy.as_mut(),
                        batch,
                        &mut durability_failed,
                        &writer_flag,
                    );
                    (reply, outcome)
                });
                let replicated_any =
                    replicated
                        .as_ref()
                        .is_some_and(|(_, outcome)| match outcome {
                            Ok(done) => done.appended > 0,
                            Err(stopped) => stopped.appended > 0,
                        });

                // The durability barrier. Submitters are told `Accepted`
                // only after this returns, so an acknowledged op has
                // reached the platter -- not merely the page cache. The
                // hook submits a ref op before git applies the ref, so
                // acknowledging early is what would let git hold a ref
                // whose authorising op does not exist.
                let durable = if acks.is_empty() && !replicated_any {
                    // Nothing was appended, so there is nothing to make
                    // durable. Skipping the barrier keeps a batch of pure
                    // rejections off the disk entirely.
                    Ok(())
                } else {
                    log.sync()
                };
                if durable.is_err() {
                    durability_failed = true;
                    // Publish before replying, so an observer that wakes
                    // on a client's error already sees the cause.
                    writer_flag.store(true, Ordering::Relaxed);
                }
                // Measured here, once the batch is durable, so every
                // accepted op is recorded at the one point every accepted
                // op passes through. A submit path added later cannot
                // forget to instrument itself.
                let batch = acks.len();
                for (reply, accepted, started) in acks.drain(..) {
                    if durable.is_ok() {
                        writer_lag.record(
                            accepted.seq,
                            accepted.decision_latency,
                            started.elapsed(),
                            batch,
                        );
                    }
                    let answer = match &durable {
                        Ok(()) => Ok(accepted),
                        // Ordered but not durable is not an acceptance.
                        // Say so, rather than acknowledge and hope.
                        //
                        // These ops are in the log and folded into the
                        // view, and cannot be taken back out: the log is
                        // append-only. So this reply leaves the daemon's
                        // view holding ops their submitters were told
                        // failed -- git will not have applied the ref its
                        // pusher was refused. That divergence is bounded
                        // to this one batch precisely because the writer
                        // now stops accepting; unbounded is what it would
                        // be if it carried on.
                        Err(e) => Err(format!("ordered but not durable: {e:?}")),
                    };
                    // Send failure just means the client gave up waiting.
                    let _ = reply.send(answer);
                }
                if let Some((reply, outcome)) = replicated {
                    // Appended but not durable is not replicated, for the
                    // reason it is not accepted above.
                    let answer = match (&durable, outcome) {
                        (Err(e), Ok(done)) => Err(ReplicateError {
                            seq: log.len().saturating_sub(done.appended),
                            reason: format!("appended but not durable: {e:?}"),
                            appended: 0,
                        }),
                        (Err(e), Err(stopped)) => Err(ReplicateError {
                            seq: log.len().saturating_sub(stopped.appended),
                            reason: format!("appended but not durable: {e:?}"),
                            appended: 0,
                        }),
                        (Ok(()), outcome) => outcome,
                    };
                    let _ = reply.send(answer);
                }
                if stopping {
                    break;
                }
            }
            // A clean shutdown must not strand the buffer.
            log.sync().ok();
            log
        });
        Self {
            tx,
            poisoned,
            lag,
            quotas,
            thread: Some(thread),
        }
    }

    /// The writer's latency record, for a daemon that wants to know
    /// whether the decision-latency gate is being met by the traffic it is
    /// actually serving rather than by the test suite.
    #[must_use]
    pub fn lag(&self) -> Arc<LagMeter> {
        self.lag.clone()
    }

    /// Creates a new client handle for a workspace.
    pub fn handle(&self) -> SequencerHandle {
        SequencerHandle {
            tx: self.tx.clone(),
            poisoned: self.poisoned.clone(),
            quotas: self.quotas.clone(),
        }
    }

    /// Stops the writer thread and returns the log for inspection.
    ///
    /// # Panics
    ///
    /// Panics if the writer thread itself panicked.
    pub fn shutdown(mut self) -> Box<dyn OpLog> {
        self.tx.send(Command::Shutdown).ok();
        self.thread
            .take()
            .expect("shutdown called once")
            .join()
            .expect("sequencer thread exits cleanly")
    }
}

#[cfg(test)]
mod tests {
    use super::{round_robin, Queued, Submission};
    use std::collections::VecDeque;
    use std::sync::mpsc;
    use std::sync::Arc;

    /// A batch as the writer sees it: actor keys in arrival order. The
    /// reply channels are never used, only carried.
    fn batch(actors: &[&str]) -> (Vec<Option<Queued>>, Vec<Arc<str>>) {
        // One interned key per distinct name, exactly as `Quotas::admit`
        // hands out -- the scheduler compares them by pointer, so a test
        // that allocated a fresh `Arc` per op would be testing nothing.
        let mut interned: Vec<Arc<str>> = Vec::new();
        let queued = actors
            .iter()
            .map(|name| {
                let key = match interned.iter().find(|k| k.as_ref() == *name) {
                    Some(k) => k.clone(),
                    None => {
                        let k: Arc<str> = Arc::from(*name);
                        interned.push(k.clone());
                        k
                    }
                };
                let (reply, _rx) = mpsc::channel();
                Some((
                    Submission {
                        channel: (*name).to_string(),
                        payload: Vec::new(),
                        author_sig: None,
                    },
                    key,
                    reply,
                ))
            })
            .collect();
        (queued, interned)
    }

    /// The order `round_robin` chose, as actor names.
    fn served(actors: &[&str]) -> Vec<String> {
        let (queued, _interned) = batch(actors);
        let mut buckets: Vec<(Arc<str>, VecDeque<usize>)> = Vec::new();
        let mut turns = Vec::new();
        round_robin(&queued, &mut buckets, &mut turns);
        turns
            .iter()
            .map(|&i| queued[i].as_ref().expect("slot filled").1.to_string())
            .collect()
    }

    #[test]
    fn one_actor_is_served_in_arrival_order() {
        let order = served(&["a", "a", "a"]);
        assert_eq!(order, ["a", "a", "a"]);
    }

    #[test]
    fn a_burst_does_not_bury_a_single_op_behind_it() {
        // The case the scheduler exists for: five ops from one actor
        // already queued when one op from someone else arrives last.
        let order = served(&["flood", "flood", "flood", "flood", "flood", "quiet"]);
        assert_eq!(
            order[1], "quiet",
            "arrival order would serve the single op last; rotation \
             serves it second: {order:?}"
        );
        assert_eq!(order.len(), 6, "and nothing is dropped: {order:?}");
    }

    #[test]
    fn an_actors_own_ops_never_overtake_each_other() {
        // Per-actor FIFO is the one thing rotation must not disturb: ops
        // from one actor build on each other, and a CAS written against
        // the previous one fails if they are reordered.
        let (queued, _interned) = batch(&["a", "b", "a", "b", "a"]);
        let mut buckets: Vec<(Arc<str>, VecDeque<usize>)> = Vec::new();
        let mut turns = Vec::new();
        round_robin(&queued, &mut buckets, &mut turns);
        let positions: Vec<usize> = turns
            .iter()
            .copied()
            .filter(|&i| queued[i].as_ref().expect("slot filled").1.as_ref() == "a")
            .collect();
        assert_eq!(
            positions,
            [0, 2, 4],
            "a's ops must be served in the order a sent them: {turns:?}"
        );
    }

    #[test]
    fn every_op_is_served_exactly_once() {
        let (queued, _interned) = batch(&["a", "b", "c", "a", "a", "c"]);
        let mut buckets: Vec<(Arc<str>, VecDeque<usize>)> = Vec::new();
        let mut turns = Vec::new();
        round_robin(&queued, &mut buckets, &mut turns);
        let mut seen = turns.clone();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), queued.len(), "no duplicates and no drops");
        assert_eq!(turns.len(), queued.len());
    }

    #[test]
    fn an_empty_batch_terminates() {
        // The rotation loop runs until every op has a turn. A batch that
        // can never fill `turns` -- empty, or holding taken slots -- is
        // the shape that would spin forever without its guard.
        let mut buckets: Vec<(Arc<str>, VecDeque<usize>)> = Vec::new();
        let mut turns = Vec::new();
        round_robin(&[], &mut buckets, &mut turns);
        assert!(turns.is_empty());

        let (mut queued, _interned) = batch(&["a", "b"]);
        queued[0] = None;
        queued[1] = None;
        round_robin(&queued, &mut buckets, &mut turns);
        assert!(turns.is_empty(), "no op left to serve: {turns:?}");
    }
}
