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
use std::sync::mpsc;
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

enum Command {
    Submit(Submission, mpsc::Sender<Result<Accepted, String>>),
    Shutdown,
}

/// Cloneable client handle; one per workspace/agent.
#[derive(Clone)]
pub struct SequencerHandle {
    tx: mpsc::Sender<Command>,
}

impl SequencerHandle {
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
}

/// Owns the single writer thread; the only component allowed to append to
/// the repo's [`OpLog`].
pub struct Sequencer {
    tx: mpsc::Sender<Command>,
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
        let thread = std::thread::spawn(move || {
            while let Ok(cmd) = rx.recv() {
                match cmd {
                    Command::Submit(sub, reply) => {
                        let started = Instant::now();
                        if let Err(reason) = policy.check(&sub) {
                            let _ = reply.send(Err(reason));
                            continue;
                        }
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
                        // Reply failure just means the client gave up waiting.
                        let _ = reply.send(Ok(Accepted {
                            seq,
                            hash,
                            decision_latency: started.elapsed(),
                        }));
                    }
                    Command::Shutdown => break,
                }
            }
            log
        });
        Self {
            tx,
            thread: Some(thread),
        }
    }

    /// Creates a new client handle for a workspace.
    pub fn handle(&self) -> SequencerHandle {
        SequencerHandle {
            tx: self.tx.clone(),
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
