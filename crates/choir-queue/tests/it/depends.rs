//! Explicit change dependencies in the queue: a combined
//! build failure ejects exactly the failing change and its (transitive)
//! declared dependents; independent changes land in the same drain and
//! the window is not halved. With no declarations the legacy
//! halve-and-retest path is untouched — `queue.rs` still proves it.

use choir_queue::{run_batch, Change, Rejection, DEFAULT_WINDOW};

/// 160 lines of base so per-change edits land in disjoint regions.
fn base() -> String {
    (0..160).map(|i| format!("line {i}\n")).collect()
}

/// A change editing line `3 * id`, declaring `depends`.
fn change(id: u64, depends: Vec<u64>) -> Change {
    let mut lines: Vec<String> = base().lines().map(String::from).collect();
    lines[(3 * id) as usize] = format!("line {} edited by change {id}", 3 * id);
    Change {
        id,
        workspace: format!("ws-{id}"),
        base: base(),
        proposed: lines.join("\n") + "\n",
        depends,
    }
}

/// Item C's pass/fail: A fails its combined build; B (depends on A) and
/// D (depends on B, transitively on A) are ejected with A; independent
/// C lands in the same window pass, and the window is never halved.
#[test]
fn a_failing_build_ejects_its_dependents_and_lands_the_independent_change() {
    let changes = vec![
        change(1, vec![]),  // A: will fail CI
        change(2, vec![1]), // B: depends on A
        change(3, vec![]),  // C: independent
        change(4, vec![2]), // D: depends on B, transitively on A
    ];
    let (report, ops) = run_batch(&base(), changes, &mut |c: &Change, _: &str| c.id != 1);

    assert_eq!(report.merged, vec![3], "only the independent change lands");
    assert_eq!(ops, 1);
    assert!(report.rejected.contains(&(1, Rejection::CiFailure)));
    assert!(report
        .rejected
        .contains(&(2, Rejection::DependencyEjection { on: 1 })));
    assert!(
        report
            .rejected
            .contains(&(4, Rejection::DependencyEjection { on: 1 })),
        "ejection must follow the dependency chain transitively"
    );
    assert!(report.final_state.contains("edited by change 3"));
    assert!(!report.final_state.contains("edited by change 1"));

    // The declarations name the blast radius, so the window is not
    // halved: it only grows with the landing.
    assert!(
        report.window_trace.iter().all(|w| *w >= DEFAULT_WINDOW),
        "dependency ejection must replace window halving, got {:?}",
        report.window_trace
    );
    // C landed within this drain — the same window pass, not a
    // resubmission after the failure.
    assert_eq!(*report.window_trace.last().unwrap(), DEFAULT_WINDOW + 1);
}

/// A dependent still waiting in the queue (behind the window) is ejected
/// too, not silently rebuilt on a state its dependency never reached.
#[test]
fn a_queued_dependent_behind_the_window_is_also_ejected() {
    let mut changes: Vec<Change> = vec![change(1, vec![])];
    // Fill the first window after the failing change...
    changes.extend((5..5 + (DEFAULT_WINDOW as u64 - 1)).map(|id| change(id, vec![])));
    // ...so the dependent waits beyond it.
    changes.push(change(2, vec![1]));
    let (report, _) = run_batch(&base(), changes, &mut |c: &Change, _: &str| c.id != 1);

    assert!(report
        .rejected
        .contains(&(2, Rejection::DependencyEjection { on: 1 })));
    assert!(!report.merged.contains(&2));
    assert_eq!(report.merged.len(), (DEFAULT_WINDOW - 1), "everyone else lands");
}
