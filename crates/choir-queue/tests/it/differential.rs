//! Executable D23 three-revision classification and exact-rate calibration.

use std::collections::BTreeMap;
use std::path::Path;

use choir_queue::differential::{
    confidence_policy, run_merged_vs_parents, Calibration, DifferentialReport, Observation,
    Verdict, CONFIDENCE_MIN_EVALUATED_MERGES,
};
use choir_queue::differential_ledger::{
    adjudicate, effective_environment, environment_hash, load_command, record_observation,
    refresh, Revisions,
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
    let env = effective_environment(&BTreeMap::new());
    let found = run_merged_vs_parents("sh", &args, &parent_a, &parent_b, &merged, &env, None).unwrap();
    assert_eq!(found.verdict, Verdict::InteractionFailure, "{found:?}");
    assert!(found.parent_a.success && found.parent_b.success);
    assert!(!found.merged.success);

    // A parent failure makes the same merged failure inconclusive instead of
    // blaming the merge. This is the control that prevents ordinary broken
    // branches from inflating the semantic-interaction count.
    std::fs::write(parent_a.join("feature-b"), "enabled\n").unwrap();
    let inconclusive =
        run_merged_vs_parents("sh", &args, &parent_a, &parent_b, &merged, &env, None).unwrap();
    assert_eq!(
        inconclusive.verdict,
        Verdict::InconclusiveParentFailure,
        "{inconclusive:?}"
    );
    std::fs::remove_dir_all(work).ok();
}

#[test]
fn a_hung_run_times_out_as_an_error_never_a_verdict() {
    let work =
        std::env::temp_dir().join(format!("choir-differential-timeout-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    let trees: Vec<_> = ["parent-a", "parent-b", "merged"]
        .iter()
        .map(|name| {
            let dir = work.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        })
        .collect();
    // The merged tree hangs; both parents return immediately. Without a
    // deadline this observation would never finish, which is exactly the
    // failure mode an unattended corpus walk cannot afford.
    std::fs::write(trees[2].join("hang"), "").unwrap();
    let script = work.join("check.sh");
    std::fs::write(&script, "#!/bin/sh\nif [ -f hang ]; then sleep 600; fi\n").unwrap();

    let args = vec![script.display().to_string()];
    let env = effective_environment(&BTreeMap::new());
    let started = std::time::Instant::now();
    let error = run_merged_vs_parents(
        "sh",
        &args,
        &trees[0],
        &trees[1],
        &trees[2],
        &env,
        Some(std::time::Duration::from_secs(1)),
    )
    .unwrap_err();
    assert!(error.contains("timed out"), "{error}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(30),
        "the deadline must actually bound the wait"
    );

    // The same deadline leaves a fast command untouched.
    let clean = run_merged_vs_parents(
        "sh",
        &args,
        &trees[0],
        &trees[1],
        &trees[0],
        &env,
        Some(std::time::Duration::from_secs(30)),
    )
    .unwrap();
    assert_eq!(clean.verdict, Verdict::Clean, "{clean:?}");
    std::fs::remove_dir_all(work).ok();
}

#[test]
fn command_timeout_is_optional_positive_and_part_of_the_snapshot() {
    let work = std::env::temp_dir().join(format!(
        "choir-differential-timeout-spec-{}",
        std::process::id()
    ));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    let command_file = work.join("command.json");

    std::fs::write(
        &command_file,
        r#"{"format_version":1,"program":"sh","args":["check.sh"]}"#,
    )
    .unwrap();
    let bare = load_command(&command_file).unwrap();
    assert_eq!(bare.timeout_seconds, None, "absent means wait forever");

    std::fs::write(
        &command_file,
        r#"{"format_version":1,"program":"sh","args":["check.sh"],"timeout_seconds":1800}"#,
    )
    .unwrap();
    let bounded = load_command(&command_file).unwrap();
    assert_eq!(bounded.timeout_seconds, Some(1800));
    assert_ne!(
        bare.snapshot_hash, bounded.snapshot_hash,
        "declaring a timeout is a command change"
    );

    for junk in [
        r#"{"format_version":1,"program":"sh","args":[],"timeout_seconds":0}"#,
        r#"{"format_version":1,"program":"sh","args":[],"timeout_seconds":"60"}"#,
        r#"{"format_version":1,"program":"sh","args":[],"timeout_seconds":-5}"#,
    ] {
        std::fs::write(&command_file, junk).unwrap();
        let error = load_command(&command_file).unwrap_err();
        assert!(error.contains("timeout_seconds"), "{junk}: {error}");
    }
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
    assert_eq!(receipt["confidence_policy"], confidence_policy());
    assert_eq!(receipt["landing_gate_enabled"], false);

    let mut confirmed = Calibration::default();
    confirmed.record(&flagged, Some(true)).unwrap();
    let receipt = confirmed.receipt();
    assert_eq!(receipt["confirmed_interactions"], 1);
    assert_eq!(receipt["spurious_failures"], 0);
}

#[test]
fn confidence_requires_2995_adjudicated_zero_spurious_observations() {
    let clean = report(Verdict::Clean);
    let flagged = report(Verdict::InteractionFailure);
    let mut calibration = Calibration::default();

    for _ in 0..CONFIDENCE_MIN_EVALUATED_MERGES - 1 {
        calibration.record(&clean, None).unwrap();
    }
    assert_eq!(calibration.receipt()["confidence_claim"], serde_json::Value::Null);

    calibration.record(&clean, None).unwrap();
    assert_eq!(calibration.receipt()["confidence_claim"], true);
    assert_eq!(
        calibration.receipt()["confidence_policy"]["minimum_evaluated_merges"],
        2_995
    );

    let mut with_spurious = Calibration::default();
    for _ in 0..CONFIDENCE_MIN_EVALUATED_MERGES - 1 {
        with_spurious.record(&clean, None).unwrap();
    }
    with_spurious.record(&flagged, Some(false)).unwrap();
    assert_eq!(with_spurious.receipt()["confidence_claim"], false);

    let mut pending = Calibration::default();
    for _ in 0..CONFIDENCE_MIN_EVALUATED_MERGES - 1 {
        pending.record(&clean, None).unwrap();
    }
    pending.record_pending(&flagged).unwrap();
    assert_eq!(pending.receipt()["confidence_claim"], serde_json::Value::Null);
}

#[test]
fn shipped_calibration_command_is_the_complete_workspace_test_gate() {
    let command_file = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts/differential-command.json");
    let command = load_command(&command_file).unwrap();
    assert_eq!(command.program, "cargo");
    assert_eq!(
        command.args,
        [
            "test",
            "--workspace",
            "--target-dir",
            "target/d23-calibration",
        ]
    );
    assert_eq!(
        command.env,
        BTreeMap::new(),
        "the shipped gate declares no environment of its own; the runs see only the pass-through list"
    );
}

#[test]
fn cargo_calibration_refuses_a_target_directory_shared_across_worktrees() {
    let work = std::env::temp_dir().join(format!(
        "choir-differential-shared-target-{}",
        std::process::id()
    ));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    let command_file = work.join("command.json");
    std::fs::write(
        &command_file,
        r#"{"format_version":1,"program":"cargo","args":["test","--workspace","--target-dir","../../target/d23-calibration"]}"#,
    )
    .unwrap();

    let error = load_command(&command_file).unwrap_err();
    assert!(
        error.contains("target directory must stay inside"),
        "{error}"
    );
    std::fs::remove_dir_all(work).ok();
}

#[test]
fn durable_calibration_stays_indeterminate_until_flags_are_adjudicated() {
    let work = std::env::temp_dir().join(format!(
        "choir-differential-ledger-{}",
        std::process::id()
    ));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    let command_file = work.join("command.json");
    std::fs::write(
        &command_file,
        r#"{"format_version":1,"program":"sh","args":["check.sh"]}"#,
    )
    .unwrap();
    let command = load_command(&command_file).unwrap();
    let state = work.join("state");
    let revisions = Revisions {
        parent_a: "a".repeat(40),
        parent_b: "b".repeat(40),
        merged: "c".repeat(40),
    };
    let abbreviated = Revisions {
        merged: "c".repeat(7),
        ..revisions.clone()
    };
    let pinned = environment_hash(&effective_environment(&command.env));
    let invalid_state = work.join("invalid-state");
    let error = record_observation(
        &invalid_state,
        &command.snapshot_hash,
        &pinned,
        &abbreviated,
        &report(Verdict::Clean),
    )
    .unwrap_err();
    assert!(error.contains("canonical Git object ids"), "{error}");
    assert!(!invalid_state.exists());

    let clean = record_observation(
        &state,
        &command.snapshot_hash,
        &pinned,
        &revisions,
        &report(Verdict::Clean),
    )
    .unwrap();
    assert_eq!(clean.observation_id, 1);
    assert_eq!(clean.calibration["target"]["met"], true);
    assert_eq!(clean.calibration["landing_gate_enabled"], false);
    assert_eq!(
        clean.calibration["confidence_claim"],
        serde_json::Value::Null
    );

    let pending = record_observation(
        &state,
        &command.snapshot_hash,
        &pinned,
        &revisions,
        &report(Verdict::InteractionFailure),
    )
    .unwrap();
    assert_eq!(pending.observation_id, 2);
    assert_eq!(pending.calibration["pending_interactions"], 1);
    assert_eq!(pending.calibration["unique_merge_commits"], 1);
    assert_eq!(
        pending.calibration["target"]["met"],
        serde_json::Value::Null,
        "an unadjudicated flag must suppress the target verdict"
    );
    assert_eq!(pending.calibration["has_conclusive_observation"], true);
    assert_eq!(pending.calibration["all_flags_adjudicated"], false);

    let receipt = adjudicate(&state, &command.snapshot_hash, 2, false).unwrap();
    assert_eq!(receipt["pending_interactions"], 0);
    assert_eq!(receipt["spurious_failures"], 1);
    assert_eq!(receipt["spurious_failure_rate"]["denominator"], 2);
    assert_eq!(receipt["target"]["met"], false);
    assert_eq!(receipt["all_flags_adjudicated"], true);
    assert!(adjudicate(&state, &command.snapshot_hash, 2, false).is_err());

    let rebuilt = refresh(&state, &command.snapshot_hash).unwrap();
    assert_eq!(rebuilt, receipt);
    let observations = std::fs::read_to_string(state.join("observations.jsonl")).unwrap();
    assert_eq!(observations.lines().count(), 2);
    assert!(observations.lines().all(|line| {
        serde_json::from_str::<serde_json::Value>(line).unwrap()["format_version"] == 1
    }));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(state.join("receipt.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    std::fs::write(
        &command_file,
        r#"{"format_version":1,"program":"sh","args":["different.sh"]}"#,
    )
    .unwrap();
    let changed = load_command(&command_file).unwrap();
    assert!(refresh(&state, &changed.snapshot_hash).is_err());
    std::fs::remove_dir_all(work).ok();
}

/// The child sees the declared environment plus the pass-through list and
/// nothing else. The leak probe is a variable cargo injects into this test
/// process; if inheritance ever comes back, the merged run fails and so does
/// this test.
#[test]
fn the_run_environment_is_the_declared_one_not_the_inherited_one() {
    assert!(
        std::env::var("CARGO_MANIFEST_DIR").is_ok(),
        "the leak probe must exist in this process for its absence in the child to mean anything"
    );
    let work = std::env::temp_dir().join(format!("choir-differential-env-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    let trees: Vec<std::path::PathBuf> = ["parent-a", "parent-b", "merged"]
        .iter()
        .map(|name| {
            let dir = work.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        })
        .collect();
    let check = work.join("check.sh");
    std::fs::write(
        &check,
        "#!/bin/sh\ntest \"${CHOIR_DIFFERENTIAL_MARKER:-}\" = calibrated || exit 1\ntest -z \"${CARGO_MANIFEST_DIR:-}\" || exit 1\n",
    )
    .unwrap();

    let mut declared = BTreeMap::new();
    declared.insert(
        "CHOIR_DIFFERENTIAL_MARKER".to_string(),
        "calibrated".to_string(),
    );
    let env = effective_environment(&declared);
    assert!(
        env.contains_key("PATH"),
        "the pass-through list is what keeps subprocess commands runnable at all"
    );
    let args = vec![check.display().to_string()];
    let report = run_merged_vs_parents("sh", &args, &trees[0], &trees[1], &trees[2], &env, None).unwrap();
    assert_eq!(report.verdict, Verdict::Clean, "{report:?}");
    std::fs::remove_dir_all(work).ok();
}

/// Same-argv was never enough: an ambient variable can change what the runs
/// measure. The first observation pins the environment in `activation.json`,
/// a drifted one is refused like a changed command snapshot, and a state
/// directory from before environment hashing adopts the current environment
/// on its next observation rather than being orphaned.
#[test]
fn observations_refuse_a_changed_environment_and_legacy_state_adopts_one() {
    let work = std::env::temp_dir().join(format!(
        "choir-differential-envhash-{}",
        std::process::id()
    ));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    let command_file = work.join("command.json");
    std::fs::write(
        &command_file,
        r#"{"format_version":1,"program":"sh","args":["check.sh"]}"#,
    )
    .unwrap();
    let command = load_command(&command_file).unwrap();
    let revisions = Revisions {
        parent_a: "a".repeat(40),
        parent_b: "b".repeat(40),
        merged: "c".repeat(40),
    };
    let pinned = environment_hash(&effective_environment(&command.env));
    let mut drifted_declared = BTreeMap::new();
    drifted_declared.insert("RUSTFLAGS".to_string(), "-Cdebug-assertions=on".to_string());
    let drifted = environment_hash(&effective_environment(&drifted_declared));
    assert_ne!(pinned, drifted);

    let state = work.join("state");
    let first = record_observation(
        &state,
        &command.snapshot_hash,
        &pinned,
        &revisions,
        &report(Verdict::Clean),
    )
    .unwrap();
    assert_eq!(first.calibration["environment_hash"], pinned.as_str());
    record_observation(
        &state,
        &command.snapshot_hash,
        &pinned,
        &revisions,
        &report(Verdict::Clean),
    )
    .unwrap();
    let error = record_observation(
        &state,
        &command.snapshot_hash,
        &drifted,
        &revisions,
        &report(Verdict::Clean),
    )
    .unwrap_err();
    assert!(error.contains("different environment"), "{error}");
    // A changed shell must not lock the operator out of adjudicating or
    // rebuilding what was already observed.
    let rebuilt = refresh(&state, &command.snapshot_hash).unwrap();
    assert_eq!(rebuilt["environment_hash"], pinned.as_str());
    assert_eq!(rebuilt["observations"], 2);

    let legacy = work.join("legacy-state");
    std::fs::create_dir_all(&legacy).unwrap();
    std::fs::write(
        legacy.join("activation.json"),
        serde_json::to_vec(&serde_json::json!({
            "format_version": 1,
            "command_snapshot_hash": command.snapshot_hash,
        }))
        .unwrap(),
    )
    .unwrap();
    let adopted = record_observation(
        &legacy,
        &command.snapshot_hash,
        &pinned,
        &revisions,
        &report(Verdict::Clean),
    )
    .unwrap();
    assert_eq!(adopted.calibration["environment_hash"], pinned.as_str());
    let activation: serde_json::Value =
        serde_json::from_slice(&std::fs::read(legacy.join("activation.json")).unwrap()).unwrap();
    assert_eq!(
        activation["environment_hash"],
        pinned.as_str(),
        "adoption must be durable, not per-process"
    );
    let error = record_observation(
        &legacy,
        &command.snapshot_hash,
        &drifted,
        &revisions,
        &report(Verdict::Clean),
    )
    .unwrap_err();
    assert!(error.contains("different environment"), "{error}");
    std::fs::remove_dir_all(work).ok();
}

#[test]
fn command_env_must_be_named_string_pairs() {
    let work = std::env::temp_dir().join(format!(
        "choir-differential-envspec-{}",
        std::process::id()
    ));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    let command_file = work.join("command.json");
    for (body, expected) in [
        (
            r#"{"format_version":1,"program":"sh","args":[],"env":{"A=B":"x"}}"#,
            "env names",
        ),
        (
            r#"{"format_version":1,"program":"sh","args":[],"env":{"A":1}}"#,
            "env values must be strings",
        ),
        (
            r#"{"format_version":1,"program":"sh","args":[],"env":[]}"#,
            "env must be an object",
        ),
    ] {
        std::fs::write(&command_file, body).unwrap();
        let error = load_command(&command_file).unwrap_err();
        assert!(error.contains(expected), "{body} -> {error}");
    }
    std::fs::write(
        &command_file,
        r#"{"format_version":1,"program":"sh","args":[],"env":{"MARKER":"yes"}}"#,
    )
    .unwrap();
    let command = load_command(&command_file).unwrap();
    assert_eq!(command.env.get("MARKER").map(String::as_str), Some("yes"));
    std::fs::remove_dir_all(work).ok();
}

/// The three runs must actually overlap in time, not merely produce the
/// same verdict as a sequential pass would.
///
/// Verdict equality cannot show this: a sequential and a concurrent
/// implementation agree on every ordinary command, which is exactly why
/// the speedup could regress to sequential without a single existing test
/// noticing. So the command itself is made to require concurrency — each
/// tree announces itself and then waits for all three announcements. Run
/// one at a time, the first invocation waits for two peers that will not
/// start until it returns, times out, and fails; the report then reads
/// `InconclusiveParentFailure` rather than `Clean`.
///
/// This is also the guard on the calibration's `independent_runs`
/// assumption: it pins that three runs happen, which a result cache would
/// quietly stop being true.
#[test]
fn the_three_revisions_are_measured_concurrently() {
    let work = std::env::temp_dir().join(format!(
        "choir-differential-parallel-{}",
        std::process::id()
    ));
    std::fs::remove_dir_all(&work).ok();
    let barrier = work.join("barrier");
    std::fs::create_dir_all(&barrier).unwrap();
    let trees: Vec<std::path::PathBuf> = ["parent-a", "parent-b", "merged"]
        .iter()
        .map(|name| {
            let dir = work.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        })
        .collect();

    let script = work.join("barrier.sh");
    std::fs::write(
        &script,
        r#"#!/bin/sh
set -eu
touch "$1/$(basename "$PWD")"
i=0
while [ "$(ls "$1" | wc -l)" -lt 3 ]; do
  i=$((i + 1))
  if [ "$i" -gt 5 ]; then exit 1; fi
  sleep 1
done
exit 0
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    let report = run_merged_vs_parents(
        &script.to_string_lossy(),
        &[barrier.to_string_lossy().into_owned()],
        &trees[0],
        &trees[1],
        &trees[2],
        &effective_environment(&BTreeMap::new()),
        None,
    )
    .expect("the barrier command spawns in all three trees");
    assert_eq!(
        report.verdict,
        Verdict::Clean,
        "all three runs must overlap; a sequential pass times out at the barrier"
    );
    assert_eq!(
        std::fs::read_dir(&barrier).unwrap().count(),
        3,
        "each tree must have been visited exactly once"
    );
    std::fs::remove_dir_all(work).ok();
}
