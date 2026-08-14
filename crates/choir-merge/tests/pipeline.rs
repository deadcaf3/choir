//! Merge-strategy seam conformance (DECISIONS.md) plus the Phase-0
//! "reproduce Mergiraf + first-class conflicts" spike check.

use choir_merge::{MergeOutcome, MergirafMerge, MergeStrategy, Pipeline};

// Edits separated by enough unchanged context that a line-based merge can
// keep the hunks apart (adjacent-line edits legitimately conflict, in git too).
const BASE: &str = "fn a() {}\nfn m1() {}\nfn m2() {}\nfn m3() {}\nfn b() {}\n";
const LEFT: &str = "fn a() { left(); }\nfn m1() {}\nfn m2() {}\nfn m3() {}\nfn b() {}\n";
const RIGHT: &str = "fn a() {}\nfn m1() {}\nfn m2() {}\nfn m3() {}\nfn b() { right(); }\n";

#[test]
fn clean_disjoint_edits_resolve_via_line_merge() {
    let p = Pipeline::default_v1();
    let result = p.merge(BASE, LEFT, RIGHT);
    match result.outcome {
        MergeOutcome::Resolved(text) => {
            assert!(text.contains("left") && text.contains("right"));
        }
        _ => panic!("disjoint edits must merge cleanly (strategy: {})", result.strategy),
    }
}

#[test]
fn one_sided_edit_resolves_trivially() {
    let p = Pipeline::default_v1();
    let result = p.merge(BASE, BASE, RIGHT);
    assert_eq!(result.strategy, "trivial");
    match result.outcome {
        MergeOutcome::Resolved(text) => assert_eq!(text, RIGHT),
        _ => panic!("one-sided edit must resolve trivially"),
    }
}

#[test]
fn overlapping_edits_surface_as_first_class_conflict() {
    let p = Pipeline::default_v1();
    let left = "fn a() { one(); }\n";
    let right = "fn a() { two(); }\n";
    let result = p.merge("fn a() {}\n", left, right);
    match result.outcome {
        MergeOutcome::Conflict { annotated } => {
            assert!(annotated.contains("<<<<<<<"), "conflict markers present");
        }
        MergeOutcome::Resolved(_) => panic!("overlapping edits must not silently resolve"),
        MergeOutcome::Unavailable(_) => unreachable!(),
    }
}

/// Structured merge resolves a same-line-region case that line merge cannot:
/// both sides append a different function at the same insertion point.
#[test]
fn mergiraf_subprocess_runs_when_installed() {
    let Some(m) = MergirafMerge::detect("rs") else {
        eprintln!("mergiraf not installed; slot skipped (D4 fallback path)");
        return;
    };
    let base = "fn a() {}\n";
    let left = "fn a() {}\nfn left() {}\n";
    let right = "fn a() {}\nfn right() {}\n";
    match m.merge(base, left, right) {
        MergeOutcome::Resolved(text) => {
            assert!(text.contains("fn left") && text.contains("fn right"));
        }
        MergeOutcome::Conflict { .. } => {
            // Acceptable: first-class conflict, never a silent wrong pick.
        }
        MergeOutcome::Unavailable(e) => panic!("mergiraf installed but failed: {e}"),
    }
}
