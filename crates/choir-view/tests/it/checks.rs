//! Check reports as a folded operation (D49).
//!
//! The claims worth testing are the ones the op's doc commits to: the
//! latest report for a `(subject, name)` pair wins, a failure outranks a
//! run still in flight, the key cannot be forged through the check name,
//! and replay reproduces the map exactly.

use choir_hash::ContentHash;
use choir_oplog::MemLog;
use choir_view::{append_op, CheckStatus, OpKind, View, ViewError, ViewOp};

fn subject() -> ContentHash {
    ContentHash::blake3(b"the commit under test")
}

fn report(name: &str, status: CheckStatus, reporter: &str) -> ViewOp {
    ViewOp::new(OpKind::RecordCheck {
        subject: subject(),
        name: name.into(),
        status,
        evidence: String::new(),
        reporter: reporter.into(),
        target_ref: Some("demo.git:refs/heads/main".into()),
    })
}

/// A later report for the same pair replaces the earlier one, and a
/// replay of the log lands on the same map.
#[test]
fn the_latest_report_wins_and_replay_reproduces_it() {
    let mut log = MemLog::new();
    append_op(&mut log, "ci", report("build", CheckStatus::Running, "ci"))
        .expect("the first report lands");
    append_op(&mut log, "ci", report("build", CheckStatus::Passed, "ci"))
        .expect("the second report lands");

    let view = View::materialize(&log).expect("replay");
    assert_eq!(view.checks.len(), 1, "re-reporting created a second row");
    let state = &view.checks[&View::check_key(&subject(), "build")];
    assert_eq!(state.status, CheckStatus::Passed);
    assert_eq!(state.reporter, "ci");
    assert_eq!(
        view.checks_verdict(&subject()),
        Some(CheckStatus::Passed),
        "one passing check is a passing verdict"
    );
}

/// A failure outranks a run still in flight. The doc says a caller told
/// `Running` beside a failed sibling would wait for an outcome that
/// cannot arrive, so this is the assertion behind that sentence.
#[test]
fn a_failure_outranks_a_run_still_in_flight() {
    let mut log = MemLog::new();
    append_op(&mut log, "ci", report("build", CheckStatus::Failed, "ci")).expect("lands");
    append_op(&mut log, "ci", report("lint", CheckStatus::Running, "ci")).expect("lands");

    let view = View::materialize(&log).expect("replay");
    assert_eq!(view.checks_verdict(&subject()), Some(CheckStatus::Failed));

    // And with the failure removed from the picture, the same view
    // reports the wait rather than a pass.
    let mut only_running = View::default();
    only_running
        .apply(&report("lint", CheckStatus::Running, "ci"))
        .expect("lands");
    assert_eq!(
        only_running.checks_verdict(&subject()),
        Some(CheckStatus::Running)
    );
}

/// Nothing reported is `None`, which is what lets the CLI tell "no
/// runner" from "a runner promised an outcome".
#[test]
fn an_unreported_subject_has_no_verdict() {
    let view = View::default();
    assert_eq!(view.checks_verdict(&subject()), None);
    assert!(view.checks_for(&subject()).is_empty());
}

/// `checks_for` returns only this subject's rows. A prefix scan that
/// forgot the separator would also match a subject whose hex begins with
/// this one's, so the neighbour here is deliberately adjacent in key
/// order.
#[test]
fn checks_for_does_not_bleed_across_subjects() {
    let mut view = View::default();
    let other = ContentHash::blake3(b"a different commit");
    view.apply(&report("build", CheckStatus::Passed, "ci"))
        .expect("lands");
    view.apply(&ViewOp::new(OpKind::RecordCheck {
        subject: other.clone(),
        name: "build".into(),
        status: CheckStatus::Failed,
        evidence: String::new(),
        reporter: "ci".into(),
        target_ref: None,
    }))
    .expect("lands");

    let mine = view.checks_for(&subject());
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].0, "build");
    assert_eq!(mine[0].1.status, CheckStatus::Passed);
    assert_eq!(view.checks_verdict(&other), Some(CheckStatus::Failed));
}

/// The name may not carry the key separator. Without this a report named
/// `"x:build"` on one commit could address the row of another, which is
/// forgery through a map key rather than through a signature.
#[test]
fn a_check_name_may_not_carry_the_separator() {
    let mut view = View::default();
    let error = view
        .apply(&report("smuggled:build", CheckStatus::Passed, "ci"))
        .expect_err("the separator is refused");
    assert!(matches!(error, ViewError::Check(_)), "{error:?}");
    assert!(
        view.checks.is_empty(),
        "a refused op still mutated the view"
    );
}

/// An empty name or reporter is refused; both are what the row is keyed
/// and attributed by.
#[test]
fn a_check_must_name_itself_and_its_reporter() {
    let mut view = View::default();
    assert!(matches!(
        view.apply(&report("", CheckStatus::Passed, "ci")),
        Err(ViewError::Check(_))
    ));
    assert!(matches!(
        view.apply(&report("build", CheckStatus::Passed, "")),
        Err(ViewError::Check(_))
    ));
    assert!(view.checks.is_empty());
}

/// Two reporters on the same check name are two rows, not a race: the
/// key is `(subject, name)`, so this is the one case where the doc's
/// "re-reporting overwrites" needs its boundary stated. It overwrites
/// per name, and a second reporter using the same name overwrites the
/// first — which is why check names are a namespace an operator owns.
#[test]
fn the_key_is_subject_and_name_not_reporter() {
    let mut view = View::default();
    view.apply(&report("build", CheckStatus::Passed, "ci"))
        .expect("lands");
    view.apply(&report("build", CheckStatus::Failed, "someone-else"))
        .expect("lands");
    assert_eq!(view.checks.len(), 1);
    let state = &view.checks[&View::check_key(&subject(), "build")];
    assert_eq!(state.status, CheckStatus::Failed);
    assert_eq!(state.reporter, "someone-else");
}

/// The wire spelling round-trips, and an upcased status is refused
/// rather than coerced.
#[test]
fn status_parsing_is_exact() {
    for status in [
        CheckStatus::Passed,
        CheckStatus::Failed,
        CheckStatus::Running,
        CheckStatus::Errored,
    ] {
        assert_eq!(CheckStatus::parse(status.as_str()), Some(status));
    }
    assert_eq!(CheckStatus::parse("PASSED"), None);
    assert_eq!(CheckStatus::parse("pass"), None);
    assert_eq!(CheckStatus::parse(""), None);
}

/// The four-way ranking, asserted as the pairs that decide it rather
/// than as a sorted list, because a sorted list passes when two ranks
/// are swapped and the comparison is written from the same order.
///
/// `Errored` above `Running` is the D18 claim in the read path: a check
/// that could not run will not go green by being waited on, so a
/// summary of `Running` sends the caller to wait for an outcome that
/// has already failed to arrive once. `Errored` below `Failed` is the
/// same claim pointed the other way: a real red build must not be
/// hidden behind our own outage.
#[test]
fn an_errored_check_outranks_a_running_one_and_yields_to_a_failure() {
    let subject = subject();

    let mut view = View::default();
    view.apply(&report("build", CheckStatus::Errored, "ci"))
        .expect("lands");
    view.apply(&report("lint", CheckStatus::Running, "ci"))
        .expect("lands");
    assert_eq!(
        view.checks_verdict(&subject),
        Some(CheckStatus::Errored),
        "a check that cannot run was reported as one worth waiting for"
    );

    view.apply(&report("docs", CheckStatus::Failed, "ci"))
        .expect("lands");
    assert_eq!(
        view.checks_verdict(&subject),
        Some(CheckStatus::Failed),
        "a red build was hidden behind our own outage"
    );

    // And on its own it is not green, which is the assertion that would
    // survive if the variant were ever folded back into `Passed`.
    let mut alone = View::default();
    alone
        .apply(&report("build", CheckStatus::Errored, "ci"))
        .expect("lands");
    assert_eq!(alone.checks_verdict(&subject), Some(CheckStatus::Errored));
}
