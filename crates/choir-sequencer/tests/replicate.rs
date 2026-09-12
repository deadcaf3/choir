//! `SequencerHandle::replicate`: a replica takes entries another writer
//! already sequenced, through its own writer thread, without re-deciding
//! admission and without skipping anything it cannot take.
//!
//! The assertions are one private suite run against both log backends,
//! the seam pattern: replication is a property of the writer, and a writer
//! over a file has to keep it exactly as a writer over memory does.

use choir_oplog::{ContentHash, FileLog, MemLog, OpEntry, OpLog};
use choir_sequencer::{Sequenced, Sequencer, Submission, SubmitPolicy};
use std::sync::{Arc, Mutex};

/// What a replica's policy saw, shared with the test that reads it.
#[derive(Default)]
struct Seen {
    checked: usize,
    folded: Vec<(u64, ContentHash)>,
}

/// Refuses every submission, records every fold, and refuses to fold one
/// chosen seq: the three things a replica's policy is asked.
struct Replica {
    seen: Arc<Mutex<Seen>>,
    unfoldable: Option<u64>,
}

impl SubmitPolicy for Replica {
    fn check(&mut self, _sub: &Submission) -> Result<(), String> {
        self.seen.lock().unwrap().checked += 1;
        Err("not home: this log is written elsewhere".to_string())
    }

    fn accepted(&mut self, entry: &OpEntry, hash: &ContentHash) {
        self.seen
            .lock()
            .unwrap()
            .folded
            .push((entry.seq, hash.clone()));
    }

    fn replicable(&mut self, entry: &OpEntry) -> Result<(), String> {
        match self.unfoldable {
            Some(seq) if seq == entry.seq => Err(format!("cannot fold seq {seq}")),
            _ => Ok(()),
        }
    }
}

/// A home's log of `n` ops, as a page would carry it: each entry with
/// the hash its source claims, which is its successor's `parent` or the
/// log's head.
fn home(tag: &str, n: usize) -> Vec<Sequenced> {
    let sequencer = Sequencer::spawn(Box::new(MemLog::new()));
    let handle = sequencer.handle();
    for i in 0..n {
        handle.submit("agent", format!("{tag} op {i}").into_bytes());
    }
    page(sequencer.shutdown().as_ref())
}

fn page(log: &dyn OpLog) -> Vec<Sequenced> {
    (0..log.len())
        .map(|seq| Sequenced {
            entry: log.get(seq).expect("seq < len"),
            hash: log
                .get(seq + 1)
                .and_then(|next| next.parent)
                .or_else(|| log.head())
                .expect("a non-empty log has a head"),
        })
        .collect()
}

fn replica(log: Box<dyn OpLog>, unfoldable: Option<u64>) -> (Sequencer, Arc<Mutex<Seen>>) {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let sequencer = Sequencer::spawn_with_policy(
        log,
        Box::new(Replica {
            seen: seen.clone(),
            unfoldable,
        }),
    );
    (sequencer, seen)
}

/// The suite. `fresh` makes an empty log of the backend under test; it
/// is called once per replica this suite needs.
fn conformance(fresh: &dyn Fn(&str) -> Box<dyn OpLog>) {
    let source = home("a", 5);

    // Two batches, the way two pages arrive: continuity is checked
    // against this log's own head, so a page boundary is not a seam.
    let (sequencer, seen) = replica(fresh("whole"), None);
    let handle = sequencer.handle();
    let first = handle.replicate(source[..3].to_vec()).expect("first page");
    assert_eq!(first.appended, 3);
    assert_eq!(first.head.as_ref(), Some(&source[2].hash));
    let second = handle.replicate(source[3..].to_vec()).expect("second page");
    assert_eq!(second.appended, 2);
    assert_eq!(second.head.as_ref(), Some(&source[4].hash));

    // Nothing was re-decided. A submission is still refused, which is
    // what makes replicate the only way anything reaches this log.
    assert!(handle.try_submit("agent", b"mine".to_vec(), None).is_err());
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.checked, 1, "check ran on a replicated entry");
        let folded: Vec<u64> = seen.folded.iter().map(|(seq, _)| *seq).collect();
        assert_eq!(folded, [0, 1, 2, 3, 4], "every entry folded, in order");
        for ((_, hash), sequenced) in seen.folded.iter().zip(&source) {
            assert_eq!(hash, &sequenced.hash, "folded with the stored hash");
        }
    }
    let log = sequencer.shutdown();
    assert_eq!(log.len(), 5);
    for sequenced in &source {
        let held = log.get(sequenced.entry.seq).expect("held");
        assert_eq!(
            held.content_hash(),
            sequenced.hash,
            "the entry is unchanged"
        );
    }
    assert_eq!(log.head().as_ref(), Some(&source[4].hash));

    // An entry that does not start at this log's next position.
    let (sequencer, _) = replica(fresh("skip"), None);
    let refused = sequencer
        .handle()
        .replicate(source[1..].to_vec())
        .expect_err("a page that skips seq 0");
    assert_eq!((refused.seq, refused.appended), (1, 0));
    assert!(refused.reason.contains("next position is 0"), "{refused}");
    assert_eq!(sequencer.shutdown().len(), 0);

    // An entry from another chain: same position, different parent.
    let other = home("b", 2);
    let (sequencer, _) = replica(fresh("fork"), None);
    let handle = sequencer.handle();
    handle.replicate(source[..1].to_vec()).expect("seq 0");
    let refused = handle
        .replicate(other[1..].to_vec())
        .expect_err("another chain's seq 1");
    assert_eq!((refused.seq, refused.appended), (1, 0));
    assert!(refused.reason.contains("another chain"), "{refused}");
    assert_eq!(sequencer.shutdown().len(), 1);

    // An entry that is not what its page said: halted there, and the
    // prefix before it kept.
    let mut tampered = source.clone();
    tampered[2].entry.payload[0] ^= 1;
    let (sequencer, seen) = replica(fresh("tamper"), None);
    let refused = sequencer
        .handle()
        .replicate(tampered)
        .expect_err("a tampered entry");
    assert_eq!((refused.seq, refused.appended), (2, 2));
    assert!(refused.reason.contains("claimed"), "{refused}");
    assert_eq!(seen.lock().unwrap().folded.len(), 2);
    assert_eq!(sequencer.shutdown().len(), 2);

    // An entry this replica cannot fold: halted, never skipped, and
    // nothing after it attempted.
    let (sequencer, seen) = replica(fresh("unfoldable"), Some(3));
    let refused = sequencer
        .handle()
        .replicate(source.clone())
        .expect_err("an unfoldable entry");
    assert_eq!((refused.seq, refused.appended), (3, 3));
    assert_eq!(refused.reason, "cannot fold seq 3");
    assert_eq!(seen.lock().unwrap().folded.len(), 3);
    let log = sequencer.shutdown();
    assert_eq!(log.len(), 3);
    assert_eq!(log.head().as_ref(), Some(&source[2].hash));
}

#[test]
fn a_replica_over_memory_takes_the_chain_and_refuses_what_does_not_continue_it() {
    conformance(&|_| Box::new(MemLog::new()));
}

#[test]
fn a_replica_over_a_file_takes_the_chain_and_refuses_what_does_not_continue_it() {
    let dir = std::env::temp_dir().join(format!("choir-replicate-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).expect("scratch dir");
    conformance(&|name| {
        Box::new(FileLog::open(&dir.join(format!("{name}.jsonl"))).expect("open file log"))
    });
    std::fs::remove_dir_all(&dir).ok();
}

/// A replicated log is still one chain its own writer can extend: an op
/// admitted after replication lands at the next position on the
/// replicated head. This is what lets a home be restored from a seed.
#[test]
fn an_op_admitted_after_replication_continues_the_same_chain() {
    let source = home("c", 3);
    let sequencer = Sequencer::spawn(Box::new(MemLog::new()));
    let handle = sequencer.handle();
    handle.replicate(source.clone()).expect("replicated");
    let accepted = handle.submit("agent", b"next".to_vec());
    assert_eq!(accepted.seq, 3);
    let log = sequencer.shutdown();
    assert_eq!(
        log.get(3).expect("seq 3").parent.as_ref(),
        Some(&source[2].hash)
    );
}
