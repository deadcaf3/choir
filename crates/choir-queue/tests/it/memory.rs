//! Resolution memory (Pijul item 1): a conflict triple resolved once —
//! recorded through item A's `Commit.resolves` link — is replayed on
//! recurrence without re-invoking the strategy pipeline, and the replay
//! is a candidate that still runs CI, never an automatic landing.

use std::collections::BTreeMap;

use choir_oplog::MemLog;
use choir_queue::memory::ResolutionMemory;
use choir_queue::{Change, MergeQueue, Rejection, DEFAULT_WINDOW};
use choir_sequencer::Sequencer;
use choir_store::{put_blob, ChunkerParams, MemStore};
use choir_view::{append_op_with_store, Commit, OpKind, TreeEntry, ViewOp};

/// The queue tip has advanced to `b`; this change was authored against
/// the older `a`, so merging it is a genuine 3-way conflict.
fn stale_change() -> Change {
    Change {
        id: 1,
        workspace: "ws-stale".into(),
        base: "a\n".into(),
        proposed: "x\n".into(),
        depends: vec![],
    }
}

fn drain(queue: &mut MergeQueue, ci: &mut dyn choir_queue::CiRunner) -> choir_queue::QueueReport {
    let sequencer = Sequencer::spawn(Box::new(MemLog::new()));
    let report = queue.drain(ci, &sequencer);
    sequencer.shutdown();
    report
}

/// Records the resolution of the `(a, b, x)` triple as item A shapes it:
/// a conflicted commit, then a linked resolution commit, both admitted
/// through the store-aware path. Returns the built memory.
fn remembered_resolution() -> ResolutionMemory {
    let mut store = MemStore::new();
    let mut log = MemLog::new();
    let addr = |store: &mut MemStore, text: &str| {
        put_blob(store, text.as_bytes(), ChunkerParams::default()).unwrap()
    };

    let mut tree = BTreeMap::new();
    tree.insert(
        "hot.txt".to_string(),
        TreeEntry::Conflict {
            base: Some(addr(&mut store, "a\n")),
            left: addr(&mut store, "b\n"),
            right: addr(&mut store, "x\n"),
        },
    );
    let conflict = Commit {
        format_version: choir_view::FORMAT_VERSION,
        parents: vec![],
        tree,
        author: "ws-stale".into(),
        message: "conflicted merge".into(),
        resolves: None,
    }
    .put(&mut store)
    .unwrap();

    let mut tree = BTreeMap::new();
    tree.insert(
        "hot.txt".to_string(),
        TreeEntry::File {
            blob: addr(&mut store, "rx\n"),
        },
    );
    let fix = Commit {
        format_version: choir_view::FORMAT_VERSION,
        parents: vec![conflict.clone()],
        tree,
        author: "ws-stale".into(),
        message: "resolve".into(),
        resolves: Some(conflict.clone()),
    }
    .put(&mut store)
    .unwrap();

    let set = |commit, prev| {
        ViewOp::new(OpKind::SetWorkspaceHead {
            workspace: "w1".into(),
            commit,
            prev,
        })
    };
    append_op_with_store(&mut log, &store, "w1", set(conflict.clone(), None)).unwrap();
    append_op_with_store(&mut log, &store, "w1", set(fix, Some(conflict))).unwrap();

    let memory = ResolutionMemory::from_log(&log, &store);
    assert_eq!(memory.len(), 1, "exactly one triple was resolved");
    memory
}

/// Item B's pass/fail: the first run conflicts (and a CI failure halves
/// the window); after the author resolves through item A's link, the
/// second run resolves the same triple from memory — with the strategy
/// pipeline provably not re-invoked for that path.
#[test]
fn second_run_replays_the_remembered_triple_without_the_pipeline() {
    // Run 1: the stale change conflicts and is evicted first-class; an
    // unrelated change fails CI so the window halves and the queue reruns.
    let mut queue = MergeQueue::new("b\n");
    queue.submit(stale_change());
    queue.submit(Change {
        id: 2,
        workspace: "ws-flaky".into(),
        base: "b\n".into(),
        proposed: "b\nf\n".into(),
        depends: vec![],
    });
    let report = drain(&mut queue, &mut |c: &Change, _: &str| c.id != 2);
    assert_eq!(report.merged, Vec::<u64>::new());
    assert!(report.rejected.contains(&(1, Rejection::Conflict)));
    assert!(report.rejected.contains(&(2, Rejection::CiFailure)));
    assert_eq!(
        report.window_trace.last(),
        Some(&(DEFAULT_WINDOW / 2)),
        "the CI failure must halve the window"
    );
    assert_eq!(report.merge_invocations, 2, "both changes hit the pipeline");
    assert!(report.replayed.is_empty(), "nothing was in memory yet");

    // The author resolves the conflict as a linked change (item A), and
    // the memory is built from those links.
    let memory = remembered_resolution();

    // Run 2: the same triple recurs; it is resolved from memory. The
    // pipeline is NOT re-invoked for that path — the invocation count,
    // not wall-clock, is the evidence.
    let mut queue = MergeQueue::new("b\n");
    queue.set_memory(memory);
    queue.submit(stale_change());
    let mut ci_runs_seen = 0usize;
    let report = drain(&mut queue, &mut |_: &Change, state: &str| {
        ci_runs_seen += 1;
        assert_eq!(state, "rx\n", "CI must judge the replayed state");
        true
    });
    assert_eq!(report.merged, vec![1]);
    assert_eq!(report.replayed, vec![1]);
    assert_eq!(report.merge_invocations, 0, "the pipeline must not run");
    assert_eq!(ci_runs_seen, 1, "the replay is a candidate: CI still runs");
    assert_eq!(report.final_state, "rx\n");
}

/// A replay is never auto-landed: when CI rejects the replayed state,
/// the change is rejected like any other candidate.
#[test]
fn a_replayed_resolution_that_fails_ci_does_not_land() {
    let mut queue = MergeQueue::new("b\n");
    queue.set_memory(remembered_resolution());
    queue.submit(stale_change());
    let report = drain(&mut queue, &mut |_: &Change, _: &str| false);
    assert_eq!(report.merged, Vec::<u64>::new());
    assert_eq!(report.rejected, vec![(1, Rejection::CiFailure)]);
    assert_eq!(report.replayed, vec![1], "replayed, then judged, then refused");
    assert_eq!(report.final_state, "b\n", "the tip must not move");
}

/// A different triple misses: memory keyed on `(a, b, x)` must not fire
/// for `(a, c, x)` — the key covers all three sides.
#[test]
fn a_different_triple_misses_the_memory() {
    let mut queue = MergeQueue::new("c\n");
    queue.set_memory(remembered_resolution());
    queue.submit(stale_change());
    let report = drain(&mut queue, &mut |_: &Change, _: &str| true);
    assert!(report.replayed.is_empty(), "no recall for a different left side");
    assert_eq!(report.rejected, vec![(1, Rejection::Conflict)]);
    assert_eq!(report.merge_invocations, 1);
}
