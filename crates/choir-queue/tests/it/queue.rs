//! Speculative merge-queue behavior (DECISIONS.md D5): green-keeping, first-class
//! conflict eviction, TCP window dynamics, retest-behind-failure.

use choir_oplog::MemLog;
use choir_queue::executor::{CiExecutor, ExecutorError, Job, Synthetic, Verdict};
use choir_queue::{run_batch, Change, MergeQueue, Rejection, DEFAULT_WINDOW};
use choir_sequencer::Sequencer;

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
    let (report, ops) = run_batch(&base(), changes, &mut Synthetic::passing());

    assert_eq!(report.merged, (0..15).collect::<Vec<u64>>());
    assert!(report.rejected.is_empty());
    assert_eq!(ops, 15, "every merge recorded through the sequencer");
    for id in 0..15 {
        assert!(report
            .final_state
            .contains(&format!("edited by change {id}")));
    }
    // Green merges grow the window, but not past the ceiling, and a
    // queue that starts at the ceiling has nowhere to grow to. This
    // asserted `DEFAULT_WINDOW + 15` while growth was unbounded, which
    // made the documented `1 + p*k` tail a statement about no `k` at
    // all. That additive increase still *works* is proved below, by
    // `ci_failure_halves_window_and_retests_behind`, where the window
    // has room underneath the ceiling to climb back through.
    assert_eq!(*report.window_trace.last().unwrap(), DEFAULT_WINDOW);
    // Assume-pass: one CI run per change, no retests needed.
    assert_eq!(report.ci_runs, 15);
}

#[test]
fn ci_failure_halves_window_and_retests_behind() {
    let changes: Vec<Change> = (0..10).map(disjoint_change).collect();
    // Change 4 always fails CI; everything else passes.
    let mut ci = Synthetic::failing_labels(&["4"]);
    let (report, ops) = run_batch(&base(), changes, &mut ci);

    assert_eq!(report.merged, vec![0, 1, 2, 3, 5, 6, 7, 8, 9]);
    assert_eq!(report.rejected, vec![(4, Rejection::CiFailure)]);
    assert_eq!(ops, 9);
    // The failed change's edit must not be in the final state.
    assert!(!report.final_state.contains("edited by change 4"));
    // Window halved after the failure, then walked back up: the four
    // greens ahead of the failure cannot lift it above the ceiling, so
    // it halves from 20 to 10 and additive increase carries it to 15.
    // Both halves of AIMD are in this one vector -- the decrease, and
    // the increase that is only observable below the ceiling.
    assert_eq!(report.window_trace, vec![10, 15]);
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

    let (report, ops) = run_batch(&base(), changes, &mut Synthetic::passing());

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
    let mut ci = Synthetic::new(|job| {
        if job.label.parse::<u64>().is_ok_and(|id| id % 8 == 7) {
            Verdict::Failed { exit_code: Some(1) }
        } else {
            Verdict::Passed
        }
    });
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
    let mut ci = Synthetic::failing_labels(&["4"]);
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

/// An executor that answers the whole call with an error.
struct Unreachable;

impl CiExecutor for Unreachable {
    fn info(&mut self) -> Result<choir_queue::executor::ExecutorInfo, ExecutorError> {
        Err(ExecutorError::Unavailable("no route to executor".into()))
    }
    fn run(&mut self, _jobs: &[Job]) -> Result<Vec<Verdict>, ExecutorError> {
        Err(ExecutorError::Unavailable("no route to executor".into()))
    }
}

/// An executor that returns fewer verdicts than it was given jobs.
struct Miscounts;

impl CiExecutor for Miscounts {
    fn info(&mut self) -> Result<choir_queue::executor::ExecutorInfo, ExecutorError> {
        Ok(choir_queue::executor::ExecutorInfo {
            name: "miscounts".into(),
            protocol: choir_queue::executor::PROTOCOL,
        })
    }
    fn run(&mut self, jobs: &[Job]) -> Result<Vec<Verdict>, ExecutorError> {
        Ok(vec![Verdict::Passed; jobs.len().saturating_sub(1)])
    }
}

fn queue_of(n: u64) -> (MergeQueue, Sequencer) {
    let mut queue = MergeQueue::new(&base());
    for c in (0..n).map(disjoint_change) {
        queue.submit(c);
    }
    (queue, Sequencer::spawn(Box::new(MemLog::new())))
}

/// The central claim of D18's seam: a provider fault is not evidence
/// about a change, so it ejects nobody.
///
/// The bool seam could not tell this from a test failure, and the queue
/// ejects on a test failure -- permanently, taking every change that
/// transitively depends on the ejected one. A VM that failed to boot
/// therefore rejected an agent's correct work.
#[test]
fn a_provider_fault_ejects_nothing_and_stalls_the_train() {
    let (mut queue, sequencer) = queue_of(10);
    let mut ci = Synthetic::new(|job| {
        if job.label == "4" {
            Verdict::Errored {
                provider: "test".into(),
                detail: "the vm did not boot".into(),
            }
        } else {
            Verdict::Passed
        }
    });
    let report = queue.drain(&mut ci, &sequencer);

    assert!(
        report.rejected.is_empty(),
        "a provider fault rejected a change: {:?}",
        report.rejected
    );
    assert!(
        report.provider_error.is_some(),
        "the stall must say why it stopped"
    );
    assert_eq!(
        report.merged,
        vec![0, 1, 2, 3],
        "the green prefix ahead of the fault should still land"
    );
    assert!(
        queue.len() >= 6,
        "everything from the faulting change on must be back in the queue, found {}",
        queue.len()
    );
}

/// The window is the response to changes that fail. None of these did,
/// so halving it would punish the queue for our own outage.
///
/// The first version of this test asked `window_trace`, which cannot
/// answer: a provider fault breaks out of the drain before the trace is
/// written, so the assertion ran over an empty vector and passed no
/// matter what the window did. A mutation that halved the window on a
/// fault survived it. Ask the queue directly, and prove the consequence
/// as well -- a halved window would need two passes to clear a queue the
/// full one clears in one.
#[test]
fn a_provider_fault_does_not_halve_the_window() {
    let (mut queue, sequencer) = queue_of(DEFAULT_WINDOW as u64);
    let mut ci = Synthetic::new(|_| Verdict::TimedOut);
    let report = queue.drain(&mut ci, &sequencer);

    assert!(
        report.merged.is_empty(),
        "nothing passed, so nothing may land"
    );
    assert!(report.rejected.is_empty(), "a timeout must not reject");
    assert_eq!(
        queue.window(),
        DEFAULT_WINDOW,
        "the window moved on a provider fault"
    );

    // And the consequence, so this does not rest on one getter: with the
    // window intact every change fits in a single pass, which is one
    // entry in the trace. Halved, it would take two.
    let recovered = queue.drain(&mut Synthetic::passing(), &sequencer);
    assert_eq!(
        recovered.merged.len(),
        DEFAULT_WINDOW,
        "the requeued changes did not all come back"
    );
    assert_eq!(
        recovered.window_trace.len(),
        1,
        "the whole queue did not fit in one pass, so the window had shrunk: {:?}",
        recovered.window_trace
    );
}

/// An executor that cannot be reached at all lands nothing, rejects
/// nothing, and leaves the queue intact for the next drain.
#[test]
fn an_unreachable_executor_leaves_the_queue_whole() {
    let (mut queue, sequencer) = queue_of(5);
    let report = queue.drain(&mut Unreachable, &sequencer);

    assert!(report.merged.is_empty());
    assert!(report.rejected.is_empty());
    assert_eq!(
        queue.len(),
        5,
        "an outage must not consume the queue, found {}",
        queue.len()
    );
    let why = report.provider_error.expect("an outage must be reported");
    assert!(why.contains("unavailable"), "unhelpful stall reason: {why}");
    assert_eq!(
        queue.window(),
        DEFAULT_WINDOW,
        "the window moved on an outage that judged nothing"
    );
}

/// Index alignment is the batch call's whole contract. A provider that
/// returns the wrong number of verdicts has attributed one change's
/// result to another, so none of them may be used.
#[test]
fn a_verdict_count_mismatch_is_refused_rather_than_zipped() {
    let (mut queue, sequencer) = queue_of(3);
    let report = queue.drain(&mut Miscounts, &sequencer);

    assert!(
        report.merged.is_empty(),
        "verdicts that could not be attributed were used anyway"
    );
    assert!(report.rejected.is_empty());
    let why = report.provider_error.expect("a miscount must be reported");
    assert!(
        why.contains("verdicts"),
        "the reason should name the miscount: {why}"
    );
    assert_eq!(
        queue.window(),
        DEFAULT_WINDOW,
        "the window moved on a batch whose verdicts were all discarded"
    );
}

/// Reading the checks the queue recorded back out of the log it wrote
/// them to, which is the only way to assert what a later reader sees.
fn checks_in(log: &dyn choir_oplog::OpLog) -> Vec<(String, String, String)> {
    let view = choir_view::View::materialize(log).expect("the log folds");
    let mut out: Vec<(String, String, String)> = view
        .checks
        .iter()
        .map(|(key, state)| {
            (
                key.clone(),
                state.status.as_str().to_string(),
                state.evidence.clone(),
            )
        })
        .collect();
    out.sort();
    out
}

fn reporter() -> choir_queue::CheckReporter {
    choir_queue::CheckReporter {
        channel: "ci".into(),
        name: "ci/build".into(),
        target_ref: Some("agents/demo.git:refs/heads/main".into()),
        seal: None,
    }
}

/// Off unless asked. The queue is a library and the identity a check is
/// reported under belongs to whoever runs it, so a default channel name
/// would put an unattributable reporter into an ordered record.
#[test]
fn a_queue_with_no_reporter_writes_no_checks() {
    let mut queue = MergeQueue::new(&base());
    for c in (0..3).map(disjoint_change) {
        queue.submit(c);
    }
    let sequencer = Sequencer::spawn(Box::new(MemLog::new()));
    let report = queue.drain(&mut Synthetic::passing(), &sequencer);
    let log = sequencer.shutdown();

    assert_eq!(report.merged.len(), 3);
    assert!(report.unreported_checks.is_empty());
    // Nothing to read: the queue never opened a reporter, so the
    // absence of checks below is the absence of a *writer*, not of a
    // result -- which is the ambiguity the "record everything" rule
    // exists to keep out of a log that does have a reporter.
    assert!(checks_in(log.as_ref()).is_empty());
}

/// Every verdict, not only the interesting ones. The map from `Verdict`
/// to `CheckStatus` is total, so recording some and dropping the rest
/// would leave a reader unable to tell "it passed" from "nobody was
/// reporting".
#[test]
fn the_queue_records_a_pass_and_a_failure_alike() {
    let mut queue = MergeQueue::new(&base());
    queue.set_check_reporter(reporter());
    for c in (0..6).map(disjoint_change) {
        queue.submit(c);
    }
    let sequencer = Sequencer::spawn(Box::new(MemLog::new()));
    let mut ci = Synthetic::failing_labels(&["4"]);
    let report = queue.drain(&mut ci, &sequencer);
    let log = sequencer.shutdown();

    assert_eq!(report.rejected, vec![(4, Rejection::CiFailure)]);
    assert!(
        report.unreported_checks.is_empty(),
        "the sequencer refused a report: {:?}",
        report.unreported_checks
    );

    let checks = checks_in(log.as_ref());
    let statuses: Vec<&str> = checks.iter().map(|(_, s, _)| s.as_str()).collect();
    assert!(
        statuses.contains(&"passed"),
        "a green run went unrecorded: {checks:?}"
    );
    assert!(
        statuses.contains(&"failed"),
        "the failure went unrecorded: {checks:?}"
    );
    assert!(
        !statuses.contains(&"errored"),
        "nothing errored, so nothing may say so: {checks:?}"
    );
}

/// The record is the executor's answer, not the queue's response to it.
///
/// A timeout is recorded `errored` even though the queue's reaction to
/// it -- stall, evict nobody -- looks nothing like a failure. Writing
/// the reaction instead of the answer would make the log agree with the
/// queue by construction and be worth nothing as evidence.
#[test]
fn a_timeout_is_recorded_as_errored_not_failed() {
    let mut queue = MergeQueue::new(&base());
    queue.set_check_reporter(reporter());
    for c in (0..3).map(disjoint_change) {
        queue.submit(c);
    }
    let sequencer = Sequencer::spawn(Box::new(MemLog::new()));
    let report = queue.drain(&mut Synthetic::new(|_| Verdict::TimedOut), &sequencer);
    let log = sequencer.shutdown();

    assert!(report.rejected.is_empty(), "a timeout must not reject");
    let checks = checks_in(log.as_ref());
    assert!(!checks.is_empty(), "the fault was recorded nowhere");
    for (key, status, _) in &checks {
        assert_eq!(
            status, "errored",
            "a provider outcome was recorded as a verdict about the commit: {key}"
        );
    }
}

/// An outage judged nobody, and says so about everybody.
///
/// This is the case the whole D18 chain was for: `provider_error` lived
/// in one report for the length of one drain, so a change whose CI
/// never ran left no trace at all. Every job in the refused batch now
/// carries the same reason.
#[test]
fn an_outage_records_the_reason_against_every_subject_it_touched() {
    let mut queue = MergeQueue::new(&base());
    queue.set_check_reporter(reporter());
    for c in (0..4).map(disjoint_change) {
        queue.submit(c);
    }
    let sequencer = Sequencer::spawn(Box::new(MemLog::new()));
    let report = queue.drain(&mut Unreachable, &sequencer);
    let log = sequencer.shutdown();

    assert!(report.merged.is_empty());
    assert!(report.rejected.is_empty());

    let checks = checks_in(log.as_ref());
    assert_eq!(
        checks.len(),
        4,
        "every subject in the refused batch needs a record: {checks:?}"
    );
    for (_, status, evidence) in &checks {
        assert_eq!(status, "errored");
        assert!(
            evidence.contains("unavailable"),
            "the record must carry why, not just that: {evidence}"
        );
    }
}

/// The payoff, and the thing this log could not express until the
/// landing became a `ViewOp`: the workspace head and the check name the
/// same commit, so a reader holding a head can ask what CI found about
/// it. Before, the head was opaque bytes and the check was about a hash
/// nothing else in the log mentioned.
#[test]
fn a_landed_head_is_the_subject_its_check_names() {
    let mut queue = MergeQueue::new(&base());
    queue.set_check_reporter(reporter());
    for c in (0..3).map(disjoint_change) {
        queue.submit(c);
    }
    let sequencer = Sequencer::spawn(Box::new(MemLog::new()));
    let report = queue.drain(&mut Synthetic::passing(), &sequencer);
    let log = sequencer.shutdown();
    assert_eq!(report.merged, vec![0, 1, 2]);

    let view = choir_view::View::materialize(log.as_ref()).expect("the log folds");
    let head = view
        .workspaces
        .get("ws-2")
        .expect("the last change landed somewhere");
    assert_eq!(
        view.checks_verdict(head),
        Some(choir_view::CheckStatus::Passed),
        "the head and the check disagree about which commit was tested"
    );

    // And the same question about a state nobody tested has no answer,
    // so the assertion above is about this subject rather than about
    // `checks_verdict` returning something for anything.
    assert_eq!(
        view.checks_verdict(&choir_hash::ContentHash::blake3(b"never built")),
        None
    );
}

/// The window is a congestion window, so it has a ceiling: additive
/// increase walks it back up after a failure and stops there.
///
/// Without the ceiling `resize(self.window + 1, ..)` runs once per
/// landed change with nothing to stop it, so a green round of width `w`
/// leaves the window at `2w` and the next at `4w`. That is not a
/// tuning detail: the cost model quoted everywhere else is `1 + p*k/2`
/// on average and `1 + p*k` at the tail, and both say nothing at all
/// unless `k` has a bound. It also sets the number of CI jobs in
/// flight at once, which is why the unbounded version was an
/// availability problem before it was a billing one.
#[test]
fn the_window_grows_back_to_its_ceiling_and_not_past_it() {
    let mut queue = MergeQueue::new(&base());
    assert_eq!(queue.window(), DEFAULT_WINDOW, "starts at the ceiling");
    assert_eq!(queue.max_window(), DEFAULT_WINDOW);

    // Lower the ceiling so there is room to observe growth beneath it,
    // then fail one change to force the multiplicative decrease.
    queue.set_max_window(8);
    assert_eq!(
        queue.window(),
        8,
        "lowering the ceiling brings the window under it now"
    );

    for id in 0..12 {
        queue.submit(disjoint_change(id));
    }
    let sequencer = Sequencer::spawn(Box::new(MemLog::new()));
    let report = queue.drain(&mut Synthetic::failing_labels(&["3"]), &sequencer);
    sequencer.shutdown();

    assert!(
        report.window_trace.iter().all(|w| *w <= 8),
        "the window must never pass its ceiling, got {:?}",
        report.window_trace
    );
    assert!(
        report.window_trace.iter().any(|w| *w < 8),
        "a CI failure must still halve it, got {:?}",
        report.window_trace
    );
    assert_eq!(
        queue.window(),
        8,
        "and additive increase must walk it back to the ceiling"
    );
}

/// Raising the ceiling is the only way past the default, and it is a
/// call rather than a field so that a cost multiplier cannot drift
/// upward by accident.
#[test]
fn the_ceiling_is_raised_only_deliberately() {
    let mut queue = MergeQueue::new(&base());
    queue.set_max_window(DEFAULT_WINDOW * 3);
    assert_eq!(queue.max_window(), DEFAULT_WINDOW * 3);
    // Raising the ceiling does not itself widen the window: only
    // landings do, one change at a time.
    assert_eq!(queue.window(), DEFAULT_WINDOW);

    for id in 0..40 {
        queue.submit(disjoint_change(id));
    }
    let sequencer = Sequencer::spawn(Box::new(MemLog::new()));
    let report = queue.drain(&mut Synthetic::passing(), &sequencer);
    sequencer.shutdown();

    assert_eq!(report.merged.len(), 40);
    assert!(
        report.window_trace.iter().all(|w| *w <= DEFAULT_WINDOW * 3),
        "the raised ceiling still bounds growth, got {:?}",
        report.window_trace
    );
    assert!(
        *report.window_trace.last().unwrap() > DEFAULT_WINDOW,
        "and growth beyond the default is reachable once it is raised, got {:?}",
        report.window_trace
    );
}
