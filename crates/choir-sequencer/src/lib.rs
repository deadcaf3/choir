//! Single-writer per-repo sequencer (plan.md D2): the Phase-0 spike.
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

use choir_oplog::{ContentHash, OpEntry, OpLog, Witness, FORMAT_VERSION};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// An operation offered to the sequencer by a workspace.
pub struct Submission {
    /// Workspace (agent) submitting the op.
    pub workspace: String,
    /// Opaque operation body, stored as [`OpEntry::payload`].
    pub payload: Vec<u8>,
    /// Author signature over `(workspace, payload)`; carried into
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
    fn accepted(&mut self, _entry: &OpEntry) {}
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

/// Most ops admitted before the writer stops draining and syncs.
///
/// Bounds the latency a submitter can inherit from the ops queued ahead
/// of it: without a cap, a sustained burst would keep the drain loop fed
/// and the batch would never close. 256 is a starting point chosen to sit
/// far under the 100 ms decision-latency gate at the measured per-op cost,
/// not a tuned value.
const MAX_BATCH: usize = 256;

enum Command {
    Submit(Submission, mpsc::Sender<Result<Accepted, String>>),
    Shutdown,
}

/// Cloneable client handle; one per workspace/agent.
#[derive(Clone)]
pub struct SequencerHandle {
    tx: mpsc::Sender<Command>,
    poisoned: Arc<AtomicBool>,
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
    pub fn submit(&self, workspace: &str, payload: Vec<u8>) -> Accepted {
        self.try_submit(workspace, payload, None)
            .expect("policy admits this op")
    }

    /// Blocks until the sequencer has ordered the op or rejected it.
    ///
    /// # Errors
    ///
    /// The policy's rejection reason.
    ///
    /// # Panics
    ///
    /// Panics if the sequencer thread has already shut down.
    pub fn try_submit(
        &self,
        workspace: &str,
        payload: Vec<u8>,
        author_sig: Option<Witness>,
    ) -> Result<Accepted, String> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(Command::Submit(
                Submission {
                    workspace: workspace.to_string(),
                    payload,
                    author_sig,
                },
                reply_tx,
            ))
            .expect("sequencer thread alive");
        reply_rx.recv().expect("sequencer replies before dropping")
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
    pub fn try_submit_many(
        &self,
        subs: Vec<Submission>,
    ) -> Vec<Result<Accepted, String>> {
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
        let waiting: Vec<_> = subs
            .into_iter()
            .map(|sub| {
                let (reply_tx, reply_rx) = mpsc::channel();
                self.tx
                    .send(Command::Submit(sub, reply_tx))
                    .expect("sequencer thread alive");
                reply_rx
            })
            .collect();
        waiting
            .into_iter()
            .map(|rx| rx.recv().expect("sequencer replies before dropping"))
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
    thread: Option<JoinHandle<Box<dyn OpLog>>>,
}

impl Sequencer {
    /// Starts the writer thread over `log` with the [`AdmitAll`] policy.
    pub fn spawn(log: Box<dyn OpLog>) -> Self {
        Self::spawn_with_policy(log, Box::new(AdmitAll))
    }

    /// Starts the writer thread over `log`; every submission passes
    /// through `policy` before it is ordered.
    pub fn spawn_with_policy(mut log: Box<dyn OpLog>, mut policy: Box<dyn SubmitPolicy>) -> Self {
        let (tx, rx) = mpsc::channel::<Command>();
        let poisoned = Arc::new(AtomicBool::new(false));
        let writer_flag = poisoned.clone();
        let thread = std::thread::spawn(move || {
            // Ordered, then made durable, then acknowledged. Everything
            // admitted in one pass through the loop shares a single
            // `sync`, so the fsync cost is paid once per batch rather
            // than once per op.
            let mut acks: Vec<(mpsc::Sender<Result<Accepted, String>>, Accepted)> = Vec::new();
            let mut stopping = false;
            // Set by a failed durability barrier and never cleared. See
            // the refusal in the admit path below for why this is
            // one-way: the alternative is ordering ops that may not
            // survive, which is the bug this whole change exists to close.
            //
            // KNOWN LIMITATION, and it is a real one. This writer stays
            // alive refusing everything, which is invisible to process
            // supervision: launchd's `KeepAlive` only restarts a process
            // that *exits*, so a transient fsync error becomes permanent
            // downtime that looks like uptime. Exiting non-zero instead
            // would make it self-clearing -- launchd restarts, FileLog
            // replays from disk, and the diverged in-flight batch
            // reconciles on the way back up, which is the recovery path
            // the D20 flip already proved with `kill -9`.
            //
            // Not done here because "a storage hiccup takes the node
            // down" is an operator-visible policy decision about the
            // daemon's lifecycle, not something a sequencer library
            // should impose on every embedder including the test suite.
            // It is written up for whoever owns that call.
            let mut durability_failed = false;
            while let Ok(first) = rx.recv() {
                let mut cmd = Some(first);
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
                        Command::Submit(sub, reply) => {
                            let started = Instant::now();
                            // Fail closed. Once a barrier has failed this
                            // writer can no longer promise anything, so it
                            // refuses *before* appending rather than
                            // ordering ops it cannot persist.
                            if durability_failed {
                                let _ = reply.send(Err(
                                    "log is not durable: writer stopped accepting".to_string(),
                                ));
                                cmd = rx.try_recv().ok();
                                continue;
                            }
                            match policy.check(&sub) {
                                // A rejection touches neither the log nor
                                // durability, so it is answered at once
                                // rather than made to wait for the batch.
                                Err(reason) => {
                                    let _ = reply.send(Err(reason));
                                }
                                Ok(()) => {
                                    let seq = log.len();
                                    let entry = OpEntry {
                                        format_version: FORMAT_VERSION,
                                        parent: log.head(),
                                        seq,
                                        workspace: sub.workspace,
                                        payload: sub.payload,
                                        witnesses: Vec::new(),
                                        author_sig: sub.author_sig,
                                    };
                                    let hash = entry.content_hash();
                                    policy.accepted(&entry);
                                    log.append(entry)
                                        .expect("single writer never sees a stale head");
                                    acks.push((
                                        reply,
                                        Accepted {
                                            seq,
                                            hash,
                                            decision_latency: started.elapsed(),
                                        },
                                    ));
                                }
                            }
                        }
                    }
                    if acks.len() >= MAX_BATCH {
                        break;
                    }
                    cmd = rx.try_recv().ok();
                }

                // The durability barrier. Submitters are told `Accepted`
                // only after this returns, so an acknowledged op has
                // reached the platter -- not merely the page cache. The
                // hook submits a ref op before git applies the ref, so
                // acknowledging early is what would let git hold a ref
                // whose authorising op does not exist.
                let durable = if acks.is_empty() {
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
                for (reply, accepted) in acks.drain(..) {
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
            thread: Some(thread),
        }
    }

    /// Creates a new client handle for a workspace.
    pub fn handle(&self) -> SequencerHandle {
        SequencerHandle {
            tx: self.tx.clone(),
            poisoned: self.poisoned.clone(),
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
