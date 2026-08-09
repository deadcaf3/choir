//! The bridge supplies exact isolated parent/merge worktrees to D23 and
//! accepts only an explicitly advisory structured result.

use std::path::Path;
use std::process::Command;

use choir_bridge::queue::{build_train, run_train_differentials, DifferentialVerdict};

fn git(dir: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@choir.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

#[test]
fn every_train_merge_is_checked_in_three_exact_isolated_worktrees() {
    let work =
        std::env::temp_dir().join(format!("choir-bridge-differential-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "."]);
    std::fs::write(work.join("base.txt"), "base\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "base"]);
    let base = git(&work, &["rev-parse", "HEAD"]);

    git(&work, &["checkout", "-q", "-b", "one"]);
    std::fs::write(work.join("one.txt"), "one\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "one"]);
    let one = git(&work, &["rev-parse", "HEAD"]);
    git(&work, &["checkout", "-q", "main"]);
    git(&work, &["checkout", "-q", "-b", "two"]);
    std::fs::write(work.join("two.txt"), "two\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "two"]);
    let two = git(&work, &["rev-parse", "HEAD"]);
    git(&work, &["checkout", "-q", "main"]);

    let train = build_train(&work, &base, &[(1, one), (2, two)]).unwrap();
    assert!(train.entries.iter().all(|entry| entry.merge.is_some()));
    let runner = work.join("runner.sh");
    std::fs::write(
        &runner,
        r#"#!/bin/sh
set -eu
test "$1" = run
test "$(git -C "$5" rev-parse HEAD)" = "$4"
test "$(git -C "$7" rev-parse HEAD)" = "$6"
test "$(git -C "$9" rev-parse HEAD)" = "$8"
test "$5" != "$7"
test "$7" != "$9"
touch "$5/isolation-marker" "$7/isolation-marker" "$9/isolation-marker"
printf '{"format_version":1,"observation_id":7,"merge":"%s","report":{"verdict":"clean"},"calibration":{"target":{"met":true},"pending_interactions":0,"confidence_claim":true,"confidence_policy":{"format_version":1,"method":"one_sided_exact_binomial_zero_spurious","confidence":{"numerator":95,"denominator":100},"target":{"numerator":1,"denominator":1000,"comparison":"strictly_less_than"},"minimum_evaluated_merges":2995,"requires_zero_spurious_failures":true,"assumptions":["independent_runs","representative_queue_command_and_merge_population"]},"landing_gate_enabled":false}}\n' "$8"
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let results = run_train_differentials(
        &work,
        &train,
        &runner,
        Path::new("command.json"),
        Path::new("state"),
    );
    assert_eq!(results.len(), 2, "every successful train merge must run");
    for (id, result) in results {
        let outcome = result.unwrap_or_else(|error| panic!("PR {id}: {error}"));
        assert_eq!(outcome.observation_id, 7);
        assert_eq!(outcome.verdict, DifferentialVerdict::Clean);
    }
    assert!(!work.join("isolation-marker").exists());
    assert!(!work.join(".choir-differential").exists());

    std::fs::write(
        &runner,
        r#"#!/bin/sh
printf '{"format_version":1,"observation_id":8,"merge":"%s","report":{"verdict":"clean"},"calibration":{"target":{"met":true},"pending_interactions":0,"confidence_claim":null,"confidence_policy":{"format_version":1,"method":"one_sided_exact_binomial_zero_spurious","confidence":{"numerator":95,"denominator":100},"target":{"numerator":1,"denominator":1000,"comparison":"strictly_less_than"},"minimum_evaluated_merges":2995,"requires_zero_spurious_failures":true,"assumptions":["independent_runs","representative_queue_command_and_merge_population"]},"landing_gate_enabled":true}}\n' "$8"
"#,
    )
    .unwrap();
    let rejected = run_train_differentials(
        &work,
        &train,
        &runner,
        Path::new("command.json"),
        Path::new("state"),
    );
    assert!(rejected.into_iter().all(|(_, result)| result.is_err()));

    std::fs::write(
        &runner,
        r#"#!/bin/sh
printf '{"format_version":1,"observation_id":9,"merge":"%s","report":{"verdict":"clean"},"calibration":{"target":{"met":true},"pending_interactions":0,"confidence_claim":null,"confidence_policy":null,"landing_gate_enabled":false}}\n' "$8"
"#,
    )
    .unwrap();
    let stale_policy = run_train_differentials(
        &work,
        &train,
        &runner,
        Path::new("command.json"),
        Path::new("state"),
    );
    assert!(stale_policy
        .into_iter()
        .all(|(_, result)| result.is_err()));
    std::fs::remove_dir_all(work).ok();
}

#[test]
fn calibrate_cli_replays_each_merge_for_each_requested_round() {
    let work = std::env::temp_dir().join(format!(
        "choir-bridge-calibrate-{}",
        std::process::id()
    ));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "."]);
    std::fs::write(work.join("base.txt"), "base\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "base"]);

    git(&work, &["checkout", "-q", "-b", "topic"]);
    std::fs::write(work.join("topic.txt"), "topic\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "topic"]);
    git(&work, &["checkout", "-q", "main"]);
    std::fs::write(work.join("main.txt"), "main\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "main"]);
    git(&work, &["merge", "-q", "--no-ff", "topic", "-m", "merge"]);
    let merge = git(&work, &["rev-parse", "HEAD"]);

    let runner = work.join("runner.sh");
    std::fs::write(
        &runner,
        r#"#!/bin/sh
set -eu
mkdir -p "$3"
count=0
test ! -f "$3/count" || count=$(sed -n '1p' "$3/count")
count=$((count + 1))
printf '%s\n' "$count" > "$3/count"
printf '%s\n' "$8" > "$3/merge"
printf '{"format_version":1,"observation_id":%s,"merge":"%s","report":{"verdict":"clean"},"calibration":{"target":{"met":true},"pending_interactions":0,"confidence_claim":null,"confidence_policy":{"format_version":1,"method":"one_sided_exact_binomial_zero_spurious","confidence":{"numerator":95,"denominator":100},"target":{"numerator":1,"denominator":1000,"comparison":"strictly_less_than"},"minimum_evaluated_merges":2995,"requires_zero_spurious_failures":true,"assumptions":["independent_runs","representative_queue_command_and_merge_population"]},"landing_gate_enabled":false}}\n' "$count" "$8"
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let state = work.join("state");
    let output = Command::new(env!("CARGO_BIN_EXE_choir-bridge"))
        .arg("calibrate")
        .arg(&work)
        .arg(&runner)
        .arg(work.join("command.json"))
        .arg(&state)
        .arg("2")
        .arg(&merge[..7])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(std::fs::read_to_string(state.join("count")).unwrap(), "2\n");
    assert_eq!(
        std::fs::read_to_string(state.join("merge")).unwrap(),
        format!("{merge}\n")
    );
    assert!(!work.join(".choir-differential").exists());
    std::fs::remove_dir_all(work).ok();
}
