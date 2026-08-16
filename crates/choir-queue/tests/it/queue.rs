//! Speculative merge-queue behavior (DECISIONS.md D5): green-keeping, first-class
//! conflict eviction, TCP window dynamics, retest-behind-failure.

use choir_queue::{run_batch, Change, Rejection, DEFAULT_WINDOW};

/// 160 lines of base so up to 50 per-change edits land in disjoint regions.
fn base() -> String {
    (0..160).map(|i| format!("line {i}\n")).collect()
}

/// A change editing line `3 * id` (disjoint from every other change).
fn disjoint_change(id: u64) -> Change {
    let mut lines: Vec<String> = base().lines().map(String::from).collect();
    lines[(3 * id) as usize] = format!("line {} edited by change {id}", 3 * id);
    Change {
        id,
        workspace: format!("ws-{id}"),
        base: base(),
        proposed: lines.join("\n") + "\n",
        depends: vec![],
    }
}

#[test]
fn all_green_train_merges_everything_in_order() {
    let changes: Vec<Change> = (0..15).map(disjoint_change).collect();
    let (report, ops) = run_batch(&base(), changes, &mut |_: &Change, _: &str| true);

    assert_eq!(report.merged, (0..15).collect::<Vec<u64>>());
    assert!(report.rejected.is_empty());
    assert_eq!(ops, 15, "every merge recorded through the sequencer");
    for id in 0..15 {
        assert!(report
            .final_state
            .contains(&format!("edited by change {id}")));
    }
    // Green merges grow the window: 20 + 15.
    assert_eq!(*report.window_trace.last().unwrap(), DEFAULT_WINDOW + 15);
    // Assume-pass: one CI run per change, no retests needed.
    assert_eq!(report.ci_runs, 15);
}

#[test]
fn ci_failure_halves_window_and_retests_behind() {
    let changes: Vec<Change> = (0..10).map(disjoint_change).collect();
    // Change 4 always fails CI; everything else passes.
    let mut ci = |c: &Change, _: &str| c.id != 4;
    let (report, ops) = run_batch(&base(), changes, &mut ci);

    assert_eq!(report.merged, vec![0, 1, 2, 3, 5, 6, 7, 8, 9]);
    assert_eq!(report.rejected, vec![(4, Rejection::CiFailure)]);
    assert_eq!(ops, 9);
    // The failed change's edit must not be in the final state.
    assert!(!report.final_state.contains("edited by change 4"));
    // Window halved after the failure: (20 + 4 greens) / 2 = 12, then +5.
    assert_eq!(report.window_trace, vec![12, 17]);
    // Changes 5..10 were tested twice: once ahead of the failure (speculative
    // states later discarded), once after requeueing.
    assert_eq!(report.ci_runs, 10 + 5);
}

#[test]
fn conflicting_change_evicted_first_class_without_blocking() {
    let mut changes: Vec<Change> = (0..6).map(disjoint_change).collect();
    // Change 99 edits the same line as change 0: guaranteed conflict once
    // change 0's edit is in the speculative state ahead of it.
    let mut lines: Vec<String> = base().lines().map(String::from).collect();
    lines[0] = "line 0 edited by change 99 differently".to_string();
    changes.insert(
        1,
        Change {
            id: 99,
            workspace: "ws-99".into(),
            base: base(),
            proposed: lines.join("\n") + "\n",
            depends: vec![],
        },
    );

    let (report, ops) = run_batch(&base(), changes, &mut |_: &Change, _: &str| true);

    assert_eq!(report.rejected, vec![(99, Rejection::Conflict)]);
    assert_eq!(
        report.merged,
        vec![0, 1, 2, 3, 4, 5],
        "conflict did not block the train"
    );
    assert_eq!(ops, 6);
    assert!(report.final_state.contains("edited by change 0"));
    assert!(!report.final_state.contains("change 99"));
}

#[test]
fn fifty_in_flight_with_flaky_ci_keeps_main_green() {
    // Phase-1 target rehearsal: 50 in-flight changes, ~1 in 8 fails CI.
    let changes: Vec<Change> = (0..50).map(disjoint_change).collect();
    let mut ci = |c: &Change, _: &str| c.id % 8 != 7;
    let (report, ops) = run_batch(&base(), changes, &mut ci);

    let expected_fail: Vec<u64> = (0..50).filter(|id| id % 8 == 7).collect();
    let expected_merge: Vec<u64> = (0..50).filter(|id| id % 8 != 7).collect();
    assert_eq!(report.merged, expected_merge);
    assert_eq!(
        report
            .rejected
            .iter()
            .map(|(id, _)| *id)
            .collect::<Vec<_>>(),
        expected_fail
    );
    assert_eq!(ops, expected_merge.len() as u64);

    // Green main: every merged edit present, every failed edit absent.
    for id in expected_merge {
        assert!(report
            .final_state
            .contains(&format!("edited by change {id}")));
    }
    for id in expected_fail {
        assert!(!report
            .final_state
            .contains(&format!("edited by change {id}")));
    }
    // Window halved on each of the 6 failures but recovered via greens.
    assert!(report.window_trace.iter().all(|w| *w >= 1));
}

/// The window trace says the window moved; the journal says why. A size
/// alone cannot separate "a build failed" from "a train landed", and
/// that distinction is the whole reason the number is worth watching.
#[test]
fn every_window_move_is_journalled_with_its_cause() {
    use choir_oplog::MemLog;
    use choir_queue::MergeQueue;
    use choir_sequencer::journal::{Journal, MemJournal};
    use choir_sequencer::Sequencer;

    struct Handle(std::sync::Arc<MemJournal>);
    impl Journal for Handle {
        fn record(&self, event: choir_sequencer::journal::Event) {
            self.0.record(event);
        }
    }

    let recorded = std::sync::Arc::new(MemJournal::new());
    let mut queue = MergeQueue::new(&base());
    queue.set_journal(Box::new(Handle(recorded.clone())));
    for c in (0..10).map(disjoint_change) {
        queue.submit(c);
    }
    let mut ci = |c: &Change, _: &str| c.id != 4;
    let sequencer = Sequencer::spawn(Box::new(MemLog::new()));
    let report = queue.drain(&mut ci, &sequencer);
    assert_eq!(report.rejected, vec![(4, Rejection::CiFailure)]);

    let resizes: Vec<serde_json::Value> = recorded
        .lines()
        .iter()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).expect("valid JSON"))
        .filter(|v| v["kind"] == "window_resize")
        .collect();
    assert!(
        !resizes.is_empty(),
        "the window moved but nothing recorded it"
    );

    // The shrink must be attributable, not merely visible.
    let shrink = resizes
        .iter()
        .find(|v| v["to"].as_u64() < v["from"].as_u64())
        .expect("the failure halved the window");
    assert_eq!(shrink["cause"], "combined build failed");

    // And growth must not be reported with the failure's cause.
    for grew in resizes
        .iter()
        .filter(|v| v["to"].as_u64() > v["from"].as_u64())
    {
        assert_ne!(
            grew["cause"], "combined build failed",
            "a growing window was attributed to a failure"
        );
    }
}
