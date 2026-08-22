//! The Phase-1 merge-queue targets, as a measurement that can fail.
//!
//! The plan names four absolute targets for Phase 1 and, until this file,
//! measured none of them. `choir-spike` is the *Phase-0* gate —
//! provisioning percentiles and decision latency — and `throughput.rs`
//! reports the submit path while deliberately asserting nothing. So the
//! two targets that belong to the queue, the wedge's differentiating
//! claim, had no number attached to them at all. An unset gate is not a
//! passed gate; that is the rule the withdrawn adoption criterion is
//! written down under, and it applies here with less excuse, because
//! these criteria already exist and only needed instrumenting.
//!
//! Its own binary, not a module of `tests/it/`, for the reason every
//! other measurement here is: it asserts on wall-clock time, and the
//! merged harness runs its modules on parallel threads.
//!
//! **What "5 commits/s/repo" can honestly mean here.** CI is a
//! caller-supplied verdict (D18), and a synthetic runner returns
//! instantly, so measuring "commits/s with fake CI" would measure the
//! fake. What this measures instead is the part that is ours: the
//! queue's own cost per change — speculative merge, conflict handling,
//! and the sequencer round-trip — against the 200 ms per-change budget
//! that 5/s implies. The number that matters is how much of that budget
//! is left for CI, because CI is what actually has to fit inside it.
//!
//! Run it as a report:
//!
//! ```text
//! cargo test -p choir-queue --release --test phase1 -- --nocapture
//! ```

use choir_queue::executor::Synthetic;
use choir_queue::{run_batch, Change, Rejection};
use std::time::{Duration, Instant};

/// The plan's target, as the per-change budget it implies.
const BUDGET: Duration = Duration::from_millis(200);

/// The plan's in-flight target.
const IN_FLIGHT: u64 = 50;

/// 160 lines of base, so 50 per-change edits land in disjoint regions.
fn base() -> String {
    (0..160).map(|i| format!("line {i}\n")).collect()
}

/// A change editing line `3 * id`, disjoint from every other change.
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

/// Green-keeping at the plan's in-flight number, which is the claim
/// rather than the throughput: a red change never lands, and never takes
/// a green one down with it.
///
/// Both halves are needed. An all-green train shows the queue can carry
/// fifty; it cannot show that failure is contained, and a queue that
/// merged everything unconditionally would pass it.
#[test]
fn green_keeping_holds_at_fifty_in_flight() {
    let changes: Vec<Change> = (0..IN_FLIGHT).map(disjoint_change).collect();
    let (report, ops) = run_batch(&base(), changes, &mut Synthetic::passing());
    assert_eq!(
        report.merged.len() as u64,
        IN_FLIGHT,
        "an all-green train of {IN_FLIGHT} did not land whole: {:?}",
        report.rejected
    );
    assert_eq!(ops, IN_FLIGHT, "every merge recorded through the sequencer");
    for id in 0..IN_FLIGHT {
        assert!(
            report
                .final_state
                .contains(&format!("edited by change {id}")),
            "change {id} merged but is not in the final state"
        );
    }

    // Now with three reds scattered through the train. Green-keeping is
    // the property that the other forty-seven still land: the base a
    // reader clones stays green, and a failure costs only its author.
    let red = [7u64, 23, 44];
    let changes: Vec<Change> = (0..IN_FLIGHT).map(disjoint_change).collect();
    let (report, ops) = run_batch(
        &base(),
        changes,
        &mut Synthetic::failing_labels(&["7", "23", "44"]),
    );
    assert_eq!(
        report.merged.len() as u64,
        IN_FLIGHT - red.len() as u64,
        "green-keeping lost changes beyond the three that failed: rejected {:?}",
        report.rejected
    );
    for id in red {
        assert!(
            !report.merged.contains(&id),
            "change {id} failed CI and landed anyway"
        );
        assert!(
            report
                .rejected
                .iter()
                .any(|(rejected, why)| *rejected == id && matches!(why, Rejection::CiFailure)),
            "change {id} failed CI and was not reported as rejected: {:?}",
            report.rejected
        );
    }
    assert_eq!(
        ops,
        IN_FLIGHT - red.len() as u64,
        "a rejected change reached the sequencer"
    );
    // The reds are gone from the state as well as from the merge list.
    // Those are two different claims and only the second is about what a
    // reader would clone.
    for id in red {
        assert!(
            !report
                .final_state
                .contains(&format!("edited by change {id}")),
            "change {id} failed CI and its edit is in the final state"
        );
    }
    println!(
        "green-keeping: {}/{IN_FLIGHT} landed with {} red, {} CI runs",
        report.merged.len(),
        red.len(),
        report.ci_runs
    );
}

/// How much of a 5-commits/s budget the queue itself spends, leaving the
/// rest for CI.
///
/// Reported first and asserted second, with the assertion deliberately
/// loose. The point is not to pin the queue's cost to a number that will
/// drift with the machine; it is to fail if the queue ever grows to eat
/// a share of the budget that CI needs. A threshold at a tenth of it
/// says "this is overhead" and would still catch a regression of an
/// order of magnitude.
#[test]
// Release only, and skipped rather than loosened in debug.
//
// The gate's `--workspace` stage is a debug build, and there the same
// batch costs 22.2 ms per change against 244 us in release -- 91x, which
// is the unoptimized merge and diff, not anything this measures. A
// threshold wide enough for both would be a threshold that means nothing
// in either, and the number here is a claim about what the queue costs a
// real node.
//
// `cfg_attr` rather than a silent early return: cargo reports it as
// ignored, so a debug run says the measurement did not happen instead of
// printing a pass for a check it skipped. `[profile.release]` leaves
// `debug-assertions` at its default of off, which is what makes this
// exact.
#[cfg_attr(
    debug_assertions,
    ignore = "release-only measurement; debug is ~91x slower and measures the profile"
)]
fn the_queue_leaves_ci_almost_all_of_the_five_per_second_budget() {
    // Warm once, measure after. The first drain pays for lazily built
    // state that a sustained rate never pays again, and reporting it as
    // the sustained cost would understate the headroom.
    let warm: Vec<Change> = (0..IN_FLIGHT).map(disjoint_change).collect();
    let _ = run_batch(&base(), warm, &mut Synthetic::passing());

    let changes: Vec<Change> = (0..IN_FLIGHT).map(disjoint_change).collect();
    let started = Instant::now();
    let (report, _) = run_batch(&base(), changes, &mut Synthetic::passing());
    let elapsed = started.elapsed();
    assert_eq!(
        report.merged.len() as u64,
        IN_FLIGHT,
        "the train did not land"
    );

    let per_change = elapsed / IN_FLIGHT as u32;
    let left_for_ci = BUDGET.saturating_sub(per_change);
    let share = per_change.as_secs_f64() / BUDGET.as_secs_f64() * 100.0;
    println!("== Phase-1 merge-queue report ==");
    println!("in-flight: {IN_FLIGHT} | target: 5 commits/s/repo => {BUDGET:?} per change");
    println!("queue cost: {elapsed:?} total, {per_change:?} per change ({share:.2}% of budget)");
    println!("left for CI: {left_for_ci:?} per change");

    assert!(
        per_change * 10 < BUDGET,
        "the queue spends {per_change:?} per change, more than a tenth of the \
         {BUDGET:?} that 5 commits/s/repo allows, and CI has to fit in what is left"
    );
}
