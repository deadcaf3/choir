//! Merge-safety verdict conformance: a resolved
//! merge may only apply edits the author proposed; reverted or injected
//! lines are named as evidence.

use choir_merge::safety::{check, SafetyVerdict};
use choir_merge::{MergeOutcome, Pipeline};

const BASE: &str = "a\nb\nc\nd\ne\n";
/// Target edited `a -> a2` since the author forked.
const TARGET: &str = "a2\nb\nc\nd\ne\n";
/// The author, against BASE, edited `d -> d2`.
const PROPOSED: &str = "a\nb\nc\nd2\ne\n";

#[test]
fn correct_merge_upholds() {
    // The honest result carries both edits.
    let verdict = check(BASE, TARGET, PROPOSED, "a2\nb\nc\nd2\ne\n");
    assert_eq!(verdict, SafetyVerdict::Upholds);
}

#[test]
fn taking_the_proposal_wholesale_reverts_target_work() {
    // A strategy that "resolves" by emitting the proposal as-is undoes the
    // target's a2 — the exact silent-reversion class the check exists for.
    match check(BASE, TARGET, PROPOSED, PROPOSED) {
        SafetyVerdict::Violation(v) => {
            assert_eq!(v.reverted, vec!["a2".to_string()]);
            assert_eq!(v.injected, vec!["a".to_string()]);
        }
        SafetyVerdict::Upholds => panic!("reverting a2 must be a violation"),
    }
}

#[test]
fn inventing_content_is_injection() {
    // Result carries both edits plus a line nobody proposed.
    match check(BASE, TARGET, PROPOSED, "a2\nb\nc\nd2\ne\nllm-hallucination\n") {
        SafetyVerdict::Violation(v) => {
            assert!(v.reverted.is_empty());
            assert_eq!(v.injected, vec!["llm-hallucination".to_string()]);
        }
        SafetyVerdict::Upholds => panic!("unproposed content must be a violation"),
    }
}

#[test]
fn explicit_revert_in_the_proposal_is_not_silent() {
    // The author forked *after* a2 landed (base == target) and deliberately
    // proposes restoring a. Every landing edit is attributable, so the check
    // passes: refusing deliberate reverts is review policy, not merge safety.
    let verdict = check(TARGET, TARGET, "a\nb\nc\nd\ne\n", "a\nb\nc\nd\ne\n");
    assert_eq!(verdict, SafetyVerdict::Upholds);
}

#[test]
fn partial_application_is_within_the_proposal() {
    // Target already contains half the author's edits (superseded-exact
    // half); the landing applies only the remainder. Fewer edits than
    // proposed is containment, not a violation.
    let verdict = check(BASE, "a\nb\nc\nd2\ne\n", "a2\nb\nc\nd2\ne\n", "a2\nb\nc\nd2\ne\n");
    assert_eq!(verdict, SafetyVerdict::Upholds);
}

#[test]
fn duplicate_lines_are_counted_as_bags() {
    // base has one "x"; target added a second. A result dropping back to one
    // "x" removed an occurrence the author never touched.
    match check("x\n", "x\nx\n", "x\ny\n", "x\ny\n") {
        SafetyVerdict::Violation(v) => assert_eq!(v.reverted, vec!["x".to_string()]),
        SafetyVerdict::Upholds => panic!("dropping a duplicated occurrence must be a violation"),
    }
}

#[test]
fn boundaries_hold_on_empty_states() {
    assert_eq!(check("", "", "", ""), SafetyVerdict::Upholds);
    // Author proposes the first content ever; target still empty.
    assert_eq!(check("", "", "hello\n", "hello\n"), SafetyVerdict::Upholds);
    // Result wipes a target the author never touched.
    match check("", "hello\n", "", "") {
        // The author's proposal (empty -> empty) removes nothing, so
        // removing hello is unattributable.
        SafetyVerdict::Violation(v) => assert_eq!(v.reverted, vec!["hello".to_string()]),
        SafetyVerdict::Upholds => panic!("wiping the target must be a violation"),
    }
}

#[test]
fn every_default_pipeline_resolution_upholds() {
    // The stock strategies are honest: their resolutions must always pass
    // the check, or the check has false positives.
    let p = Pipeline::default_v1();
    let cases = [
        (BASE, TARGET, PROPOSED),
        (BASE, BASE, PROPOSED),
        (BASE, TARGET, BASE),
        (BASE, TARGET, TARGET),
    ];
    for (base, left, right) in cases {
        if let MergeOutcome::Resolved(result) = p.merge(base, left, right).outcome {
            assert_eq!(
                check(base, left, right, &result),
                SafetyVerdict::Upholds,
                "false positive on base={base:?} left={left:?} right={right:?}"
            );
        }
    }
}
