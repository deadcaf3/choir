//! Single-writer per-repo sequencer (plan.md D2): the Phase-0 spike.
//!
//! One OS thread owns the op log; all workspaces submit ops through cloned
//! handles and receive the assigned (seq, hash) synchronously. This is the
//! in-process second implementation required by the actor-runtime seam (D3);
//! the Rivet-backed implementation must pass the same conformance tests.

use choir_oplog::{ContentHash, OpEntry, OpLog, FORMAT_VERSION};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub struct Submission {
    pub workspace: String,
    pub payload: Vec<u8>,
}

#[derive(Debug)]
pub struct Accepted {
    pub seq: u64,
    pub hash: ContentHash,
    /// Time from dequeue to append; the merge-decision latency the Phase-0
    /// gate measures (<100 ms target, CI excluded).
    pub decision_latency: Duration,
}

enum Command {
    Submit(Submission, mpsc::Sender<Accepted>),
    Shutdown,
}

/// Cloneable client handle; one per workspace/agent.
#[derive(Clone)]
pub struct SequencerHandle {
    tx: mpsc::Sender<Command>,
}

impl SequencerHandle {
    /// Blocks until the sequencer has durably ordered the op.
    pub fn submit(&self, workspace: &str, payload: Vec<u8>) -> Accepted {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(Command::Submit(
                Submission {
                    workspace: workspace.to_string(),
                    payload,
                },
                reply_tx,
            ))
            .expect("sequencer thread alive");
        reply_rx.recv().expect("sequencer replies before dropping")
    }
}

pub struct Sequencer {
    tx: mpsc::Sender<Command>,
    thread: Option<JoinHandle<Box<dyn OpLog>>>,
}

impl Sequencer {
    pub fn spawn(mut log: Box<dyn OpLog>) -> Self {
        let (tx, rx) = mpsc::channel::<Command>();
        let thread = std::thread::spawn(move || {
            while let Ok(cmd) = rx.recv() {
                match cmd {
                    Command::Submit(sub, reply) => {
                        let started = Instant::now();
                        let seq = log.len();
                        let entry = OpEntry {
                            format_version: FORMAT_VERSION,
                            parent: log.head(),
                            seq,
                            workspace: sub.workspace,
                            payload: sub.payload,
                            witnesses: Vec::new(),
                        };
                        let hash = entry
                            .content_hash();
                        log.append(entry)
                            .expect("single writer never sees a stale head");
                        // Reply failure just means the client gave up waiting.
                        let _ = reply.send(Accepted {
                            seq,
                            hash,
                            decision_latency: started.elapsed(),
                        });
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

    pub fn handle(&self) -> SequencerHandle {
        SequencerHandle {
            tx: self.tx.clone(),
        }
    }

    /// Stops the writer thread and returns the log for inspection.
    pub fn shutdown(mut self) -> Box<dyn OpLog> {
        self.tx.send(Command::Shutdown).ok();
        self.thread
            .take()
            .expect("shutdown called once")
            .join()
            .expect("sequencer thread exits cleanly")
    }
}
