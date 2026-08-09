//! Executable D23 three-revision classification and exact-rate calibration.

use choir_queue::differential::{
    run_merged_vs_parents, Calibration, DifferentialReport, Observation, Verdict,
};

fn report(verdict: Verdict) -> DifferentialReport {
    let pass = Observation {
        success: true,
        exit_code: Some(0),
    };
    let fail = Observation {
        success: false,
        exit_code: Some(1),
    };
    match verdict {
        Verdict::Clean => DifferentialReport {
            parent_a: pass,
            parent_b: pass,
            merged: pass,
            verdict,
        },
        Verdict::InteractionFailure => DifferentialReport {
            parent_a: pass,
            parent_b: pass,
            merged: fail,
            verdict,
        },
        Verdict::InconclusiveParentFailure => DifferentialReport {
            parent_a: fail,
            parent_b: pass,
            merged: fail,
            verdict,
        },
    }
}

#[test]
fn same_command_finds_behavior_that_only_the_merge_breaks() {
    let work = std::env::temp_dir().join(format!("choir-differential-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    let parent_a = work.join("parent-a");
    let parent_b = work.join("parent-b");
    let merged = work.join("merged");
    for dir in [&parent_a, &parent_b, &merged] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(parent_a.join("feature-a"), "enabled\n").unwrap();
    std::fs::write(parent_b.join("feature-b"), "enabled\n").unwrap();
    std::fs::write(merged.join("feature-a"), "enabled\n").unwrap();
    std::fs::write(merged.join("feature-b"), "enabled\n").unwrap();
    let check = work.join("check.sh");
    std::fs::write(
        &check,
        "#!/bin/sh\nif [ -f feature-a ] && [ -f feature-b ]; then exit 1; fi\n",
    )
    .unwrap();

    let args = vec![check.display().to_string()];
    let found = run_merged_vs_parents("sh", &args, &parent_a, &parent_b, &merged).unwrap();
    assert_eq!(found.verdict, Verdict::InteractionFailure, "{found:?}");
    assert!(found.parent_a.success && found.parent_b.success);
    assert!(!found.merged.success);

    // A parent failure makes the same merged failure inconclusive instead of
    // blaming the merge. This is the control that prevents ordinary broken
    // branches from inflating the semantic-interaction count.
    std::fs::write(parent_a.join("feature-b"), "enabled\n").unwrap();
    let inconclusive = run_merged_vs_parents("sh", &args, &parent_a, &parent_b, &merged).unwrap();
    assert_eq!(
        inconclusive.verdict,
        Verdict::InconclusiveParentFailure,
        "{inconclusive:?}"
    );
    std::fs::remove_dir_all(work).ok();
}

#[test]
fn calibration_uses_exact_strict_point_one_percent_boundary() {
    let clean = report(Verdict::Clean);
    let flagged = report(Verdict::InteractionFailure);
    let inconclusive = report(Verdict::InconclusiveParentFailure);
    let mut calibration = Calibration::default();
    assert_eq!(calibration.target_met(), None);
    assert!(calibration.record(&flagged, None).is_err());
    assert!(calibration.record(&clean, Some(false)).is_err());

    for _ in 0..999 {
        calibration.record(&clean, None).unwrap();
    }
    calibration.record(&flagged, Some(false)).unwrap();
    calibration.record(&inconclusive, None).unwrap();
    assert_eq!(
        calibration.target_met(),
        Some(false),
        "one spurious failure in 1000 is exactly 0.1%, not below it"
    );
    calibration.record(&clean, None).unwrap();
    assert_eq!(calibration.target_met(), Some(true));

    let receipt = calibration.receipt();
    assert_eq!(receipt["format_version"], 1);
    assert_eq!(receipt["evaluated_merges"], 1001);
    assert_eq!(receipt["inconclusive_parent_failures"], 1);
    assert_eq!(receipt["spurious_failure_rate"]["numerator"], 1);
    assert_eq!(receipt["spurious_failure_rate"]["denominator"], 1001);
    assert_eq!(receipt["spurious_failure_rate"]["basis_points_floor"], 9);
    assert_eq!(receipt["target"]["met"], true);
    assert_eq!(receipt["target"]["comparison"], "strictly_less_than");
    assert_eq!(receipt["confidence_claim"], serde_json::Value::Null);

    let mut confirmed = Calibration::default();
    confirmed.record(&flagged, Some(true)).unwrap();
    let receipt = confirmed.receipt();
    assert_eq!(receipt["confirmed_interactions"], 1);
    assert_eq!(receipt["spurious_failures"], 0);
}
