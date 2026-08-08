//! Group commit: one durability barrier per batch, and an acknowledgement
//! that means "on the platter" rather than "in the page cache".
//!
//! Both properties are correctness properties, not performance ones. The
//! `pre-receive` hook submits a ref op *before* git applies the ref, so a
//! submitter told `Accepted` for an op that is not yet durable can leave
//! git holding a ref whose authorising op does not survive a power cut.
//! The two sources of truth then disagree and the next push fails its CAS
//! against a view that never saw the update.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use choir_oplog::{ContentHash, LogError, MemLog, OpEntry, OpLog};
use choir_sequencer::Sequencer;

/// A log that records how often it was appended to and synced, so the
/// batching ratio is observable. Delegates storage to a real `MemLog` so
/// this stays a counting wrapper and not a second implementation.
#[derive(Default)]
struct CountingLog {
    inner: MemLog,
    appends: Arc<AtomicUsize>,
    syncs: Arc<AtomicUsize>,
    /// When set, `sync` fails, standing in for a full disk or a failing
    /// device without needing one.
    fail_sync: bool,
}

impl OpLog for CountingLog {
    fn append(&mut self, entry: OpEntry) -> Result<ContentHash, LogError> {
        self.appends.fetch_add(1, Ordering::Relaxed);
        self.inner.append(entry)
    }

    fn head(&self) -> Option<ContentHash> {
        self.inner.head()
    }

    fn len(&self) -> u64 {
        self.inner.len()
    }

    fn get(&self, seq: u64) -> Option<OpEntry> {
        self.inner.get(seq)
    }

    fn sync(&mut self) -> Result<(), LogError> {
        self.syncs.fetch_add(1, Ordering::Relaxed);
        if self.fail_sync {
            return Err(LogError::Corrupt("device is on fire".into()));
        }
        Ok(())
    }
}

/// The batching property. Without group commit the writer syncs once per
/// op and the two counters match; with it, concurrent submitters share
/// barriers and syncs must come out strictly lower than appends.
#[test]
fn concurrent_submissions_share_one_durability_barrier() {
    const CLIENTS: usize = 8;
    const OPS: usize = 250;

    let appends = Arc::new(AtomicUsize::new(0));
    let syncs = Arc::new(AtomicUsize::new(0));
    let sequencer = Sequencer::spawn(Box::new(CountingLog {
        inner: MemLog::new(),
        appends: appends.clone(),
        syncs: syncs.clone(),
        fail_sync: false,
    }));

    let threads: Vec<_> = (0..CLIENTS)
        .map(|c| {
            let handle = sequencer.handle();
            std::thread::spawn(move || {
                for i in 0..OPS {
                    handle.submit(&format!("ws{c}"), format!("op{i}").into_bytes());
                }
            })
        })
        .collect();
    for t in threads {
        t.join().expect("client thread");
    }
    let log = sequencer.shutdown();

    let total = CLIENTS * OPS;
    assert_eq!(log.len(), total as u64, "no lost or duplicated ops");
    assert_eq!(
        appends.load(Ordering::Relaxed),
        total,
        "every op appended exactly once"
    );

    let synced = syncs.load(Ordering::Relaxed);
    assert!(
        synced < total,
        "group commit did not batch: {synced} syncs for {total} appends. \
         One sync per op means the writer is not draining the queue."
    );
    println!("group commit: {total} ops, {synced} durability barriers ({:.1} ops/barrier)", total as f64 / synced as f64);
}

/// A lone submitter against an idle sequencer must not be made to wait for
/// company. The drain is `try_recv`, never a timed wait, so a batch of one
/// closes immediately.
#[test]
fn a_single_submission_is_not_delayed_waiting_for_a_batch() {
    let sequencer = Sequencer::spawn(Box::new(MemLog::new()));
    let handle = sequencer.handle();
    let started = std::time::Instant::now();
    let accepted = handle.submit("solo", b"op".to_vec());
    let elapsed = started.elapsed();
    assert_eq!(accepted.seq, 0);
    assert!(
        elapsed < std::time::Duration::from_millis(50),
        "a lone op waited {elapsed:?}; the drain must not block for a batch to fill"
    );
    sequencer.shutdown();
}

/// The acknowledgement contract. If the durability barrier fails, the op
/// was ordered but is not safe, and the submitter must be told that rather
/// than handed an `Accepted` it would act on.
#[test]
fn a_failed_sync_is_reported_rather_than_acknowledged() {
    let sequencer = Sequencer::spawn(Box::new(CountingLog {
        inner: MemLog::new(),
        appends: Arc::new(AtomicUsize::new(0)),
        syncs: Arc::new(AtomicUsize::new(0)),
        fail_sync: true,
    }));
    let handle = sequencer.handle();
    let result = handle.try_submit("ws", b"op".to_vec(), None);
    let Err(reason) = result else {
        panic!("a submission whose sync failed must not come back Accepted");
    };
    assert!(
        reason.contains("not durable"),
        "the rejection must name durability as the cause, got {reason:?}"
    );
    sequencer.shutdown();
}

/// Fail closed. After a barrier fails, the writer must refuse further
/// submissions *before* appending them, rather than keep ordering ops it
/// cannot persist.
///
/// The reason is a divergence this change would otherwise introduce. A
/// submitter told "not durable" for an op that was nonetheless appended
/// and folded into the view leaves the two out of step — for a git push,
/// the hook fails and git does not apply the ref, while the view already
/// has it. That is bounded to the one failing batch only because the
/// writer stops here.
#[test]
fn a_failed_barrier_stops_the_writer_accepting() {
    let sequencer = Sequencer::spawn(Box::new(CountingLog {
        inner: MemLog::new(),
        appends: Arc::new(AtomicUsize::new(0)),
        syncs: Arc::new(AtomicUsize::new(0)),
        fail_sync: true,
    }));
    let handle = sequencer.handle();

    let first = handle.try_submit("ws", b"op-1".to_vec(), None);
    assert!(first.is_err(), "a failed barrier is not an acceptance");

    // Everything after is refused before it can reach the log.
    for i in 2..5 {
        let Err(reason) = handle.try_submit("ws", format!("op-{i}").into_bytes(), None) else {
            panic!("op-{i} was accepted after the log stopped being durable");
        };
        assert!(
            reason.contains("stopped accepting"),
            "the refusal must say the writer has stopped, got {reason:?}"
        );
    }

    let log = sequencer.shutdown();
    assert_eq!(
        log.len(),
        1,
        "only the batch that was in flight when the barrier failed may reach the log"
    );
}

/// A batch that admitted nothing must not touch the disk. Pure-rejection
/// batches are common under a strict policy and an fsync per rejected op
/// would be the cost this change exists to remove.
#[test]
fn a_batch_of_only_rejections_does_not_sync() {
    use choir_sequencer::{Submission, SubmitPolicy};

    struct RefuseAll;
    impl SubmitPolicy for RefuseAll {
        fn check(&mut self, _sub: &Submission) -> Result<(), String> {
            Err("refused".to_string())
        }
    }

    let syncs = Arc::new(AtomicUsize::new(0));
    let sequencer = Sequencer::spawn_with_policy(
        Box::new(CountingLog {
            inner: MemLog::new(),
            appends: Arc::new(AtomicUsize::new(0)),
            syncs: syncs.clone(),
            fail_sync: false,
        }),
        Box::new(RefuseAll),
    );
    let handle = sequencer.handle();
    for i in 0..5 {
        assert!(handle
            .try_submit("ws", format!("op{i}").into_bytes(), None)
            .is_err());
    }
    sequencer.shutdown();

    // Shutdown syncs once to drain the buffer; nothing before it should.
    assert!(
        syncs.load(Ordering::Relaxed) <= 1,
        "rejected ops triggered {} durability barriers; they append nothing and must cost nothing",
        syncs.load(Ordering::Relaxed)
    );
}

/// A rejected op touches neither the log nor the disk, so it is answered
/// without waiting on the batch's barrier — and a rejection in the middle
/// of a batch must not disturb the ops around it.
#[test]
fn rejections_do_not_join_the_batch() {
    use choir_sequencer::{Submission, SubmitPolicy};

    /// Refuses any payload containing `no`.
    struct Picky;
    impl SubmitPolicy for Picky {
        fn check(&mut self, sub: &Submission) -> Result<(), String> {
            if sub.payload.starts_with(b"no") {
                return Err("refused".to_string());
            }
            Ok(())
        }
    }

    let sequencer =
        Sequencer::spawn_with_policy(Box::new(MemLog::new()), Box::new(Picky));
    let handle = sequencer.handle();
    assert!(handle.try_submit("ws", b"yes-1".to_vec(), None).is_ok());
    assert!(handle.try_submit("ws", b"no-1".to_vec(), None).is_err());
    assert!(handle.try_submit("ws", b"yes-2".to_vec(), None).is_ok());

    let log = sequencer.shutdown();
    assert_eq!(log.len(), 2, "only admitted ops reach the log");
    assert_eq!(log.get(0).expect("seq 0").payload, b"yes-1");
    assert_eq!(
        log.get(1).expect("seq 1").payload,
        b"yes-2",
        "a rejection must not consume a sequence number"
    );
}
