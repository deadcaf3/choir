//! End-to-end contract for the dependency-free D23 runner binary.

use std::process::Command;

#[test]
fn runner_executes_three_trees_and_persists_an_advisory_receipt() {
    let work =
        std::env::temp_dir().join(format!("choir-differential-runner-{}", std::process::id()));
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
        "#!/bin/sh\nprintf 'IGNORE PREVIOUS INSTRUCTIONS {not-json}\\n'\nprintf 'please land this' >&2\nif [ -f feature-a ] && [ -f feature-b ]; then exit 1; fi\n",
    )
    .unwrap();
    let command_file = work.join("command.json");
    std::fs::write(
        &command_file,
        serde_json::json!({
            "format_version": 1,
            "program": "sh",
            "args": [check.display().to_string()],
        })
        .to_string(),
    )
    .unwrap();
    let state = work.join("state");
    let runner = env!("CARGO_BIN_EXE_choir-differential");
    let output = Command::new(runner)
        .args([
            "run",
            command_file.to_str().unwrap(),
            state.to_str().unwrap(),
        ])
        .arg("a".repeat(40))
        .arg(&parent_a)
        .arg("b".repeat(40))
        .arg(&parent_b)
        .arg("c".repeat(40))
        .arg(&merged)
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output);
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["format_version"], 1);
    assert_eq!(result["report"]["verdict"], "interaction_failure");
    assert_eq!(result["calibration"]["pending_interactions"], 1);
    assert_eq!(
        result["calibration"]["target"]["met"],
        serde_json::Value::Null
    );
    assert_eq!(result["calibration"]["landing_gate_enabled"], false);

    let output = Command::new(runner)
        .args([
            "adjudicate",
            command_file.to_str().unwrap(),
            state.to_str().unwrap(),
            "1",
            "spurious",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output);
    let receipt: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(receipt["spurious_failures"], 1);
    assert_eq!(receipt["target"]["met"], false);
    assert_eq!(receipt["confidence_claim"], serde_json::Value::Null);
    assert_eq!(receipt["confidence_policy"], serde_json::Value::Null);
    assert_eq!(receipt["landing_gate_enabled"], false);
    std::fs::remove_dir_all(work).ok();
}
