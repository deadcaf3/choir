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

/// Builds a repo with two independent merge commits and a runner stub that
/// reports, per observation, which parent-a directory it was handed and whether
/// that directory still held the previous observation's untracked output.
///
/// Verdict equality cannot test a worktree lifetime — a per-observation tree
/// and a held tree agree on every ordinary command — so the command itself has
/// to observe the lifetime, the same trick
/// `the_three_revisions_are_measured_concurrently` uses for concurrency.
fn two_merge_calibration_fixture(name: &str) -> (std::path::PathBuf, String, String) {
    let work = std::env::temp_dir().join(format!("choir-bridge-{name}-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "."]);
    std::fs::write(work.join("base.txt"), "base\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "base"]);

    let mut merges = Vec::new();
    for step in ["one", "two"] {
        git(&work, &["checkout", "-q", "-b", &format!("topic-{step}")]);
        std::fs::write(work.join(format!("{step}.txt")), step).unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-q", "-m", step]);
        git(&work, &["checkout", "-q", "main"]);
        std::fs::write(work.join(format!("main-{step}.txt")), step).unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-q", "-m", &format!("main {step}")]);
        git(
            &work,
            &[
                "merge",
                "-q",
                "--no-ff",
                &format!("topic-{step}"),
                "-m",
                &format!("merge {step}"),
            ],
        );
        merges.push(git(&work, &["rev-parse", "HEAD"]));
    }

    let runner = work.join("runner.sh");
    std::fs::write(
        &runner,
        r#"#!/bin/sh
set -eu
mkdir -p "$3"
printf '%s\n' "$5" >> "$3/parent-a-paths"
if [ -f "$5/carried-marker" ]; then
  printf 'present\n' >> "$3/carried"
else
  printf 'absent\n' >> "$3/carried"
fi
: > "$5/carried-marker"
test "$(git -C "$5" rev-parse HEAD)" = "$4"
test "$(git -C "$7" rev-parse HEAD)" = "$6"
test "$(git -C "$9" rev-parse HEAD)" = "$8"
printf '{"format_version":1,"observation_id":1,"merge":"%s","report":{"verdict":"clean"},"calibration":{"target":{"met":true},"pending_interactions":0,"confidence_claim":null,"confidence_policy":{"format_version":1,"method":"one_sided_exact_binomial_zero_spurious","confidence":{"numerator":95,"denominator":100},"target":{"numerator":1,"denominator":1000,"comparison":"strictly_less_than"},"minimum_evaluated_merges":2995,"requires_zero_spurious_failures":true,"assumptions":["independent_runs","representative_queue_command_and_merge_population"]},"landing_gate_enabled":false}}\n' "$8"
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let second = merges.pop().unwrap();
    let first = merges.pop().unwrap();
    (work, first, second)
}

#[test]
fn calibrate_holds_one_worktree_triple_open_across_observations() {
    let (work, first, second) = two_merge_calibration_fixture("session");
    let state = work.join("state");
    let output = Command::new(env!("CARGO_BIN_EXE_choir-bridge"))
        .arg("calibrate")
        .arg(&work)
        .arg(work.join("runner.sh"))
        .arg(work.join("command.json"))
        .arg(&state)
        .arg("1")
        .arg(&first)
        .arg(&second)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");

    // Same three trees for both observations: one distinct parent-a path.
    let paths = std::fs::read_to_string(state.join("parent-a-paths")).unwrap();
    let distinct: std::collections::BTreeSet<&str> = paths.lines().collect();
    assert_eq!(paths.lines().count(), 2, "both observations must run");
    assert_eq!(
        distinct.len(),
        1,
        "a calibration run must reuse one worktree triple, got {distinct:?}"
    );

    // And the tree kept its contents, which is the part that saves the time:
    // a removed-and-re-added worktree would also reuse the path while
    // rebuilding everything.
    assert_eq!(
        std::fs::read_to_string(state.join("carried")).unwrap(),
        "absent\npresent\n",
        "the second observation must find the first observation's tree contents"
    );

    // The stub asserts each tree's HEAD equals the oid it was handed, so a
    // successful exit already proves the forced checkout moved all three trees
    // to the second merge's revisions rather than leaving them behind.
    assert!(!work.join(".choir-differential").exists());
    std::fs::remove_dir_all(work).ok();
}

/// On a corpus of consecutive first-parent merges, a merge's first parent is
/// the previous observation's merge, and the session must hand that revision
/// the tree that just ran it — same path, contents intact — so the command
/// there has nothing to rebuild. Path equality alone would also hold for a
/// remove-and-recreate, so the stub plants an untracked marker in each
/// observation's merged tree and looks for it in the next parent-a tree.
#[test]
fn calibrate_hands_the_previous_merge_tree_to_the_next_parent_a() {
    let work = std::env::temp_dir().join(format!(
        "choir-bridge-chained-{}",
        std::process::id()
    ));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "."]);
    std::fs::write(work.join("base.txt"), "base\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "base"]);

    // main advances only by merges, so merge two's first parent is merge one.
    let mut merges = Vec::new();
    for step in ["one", "two"] {
        git(&work, &["checkout", "-q", "-b", &format!("topic-{step}")]);
        std::fs::write(work.join(format!("{step}.txt")), step).unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-q", "-m", step]);
        git(&work, &["checkout", "-q", "main"]);
        git(
            &work,
            &[
                "merge",
                "-q",
                "--no-ff",
                &format!("topic-{step}"),
                "-m",
                &format!("merge {step}"),
            ],
        );
        merges.push(git(&work, &["rev-parse", "HEAD"]));
    }

    let runner = work.join("runner.sh");
    std::fs::write(
        &runner,
        r#"#!/bin/sh
set -eu
mkdir -p "$3"
printf '%s %s %s\n' "$5" "$7" "$9" >> "$3/dirs"
if [ -f "$5/chain-marker" ]; then
  printf 'present\n' >> "$3/carried-into-a"
else
  printf 'absent\n' >> "$3/carried-into-a"
fi
: > "$9/chain-marker"
test "$(git -C "$5" rev-parse HEAD)" = "$4"
test "$(git -C "$7" rev-parse HEAD)" = "$6"
test "$(git -C "$9" rev-parse HEAD)" = "$8"
printf '{"format_version":1,"observation_id":1,"merge":"%s","report":{"verdict":"clean"},"calibration":{"target":{"met":true},"pending_interactions":0,"confidence_claim":null,"confidence_policy":{"format_version":1,"method":"one_sided_exact_binomial_zero_spurious","confidence":{"numerator":95,"denominator":100},"target":{"numerator":1,"denominator":1000,"comparison":"strictly_less_than"},"minimum_evaluated_merges":2995,"requires_zero_spurious_failures":true,"assumptions":["independent_runs","representative_queue_command_and_merge_population"]},"landing_gate_enabled":false}}\n' "$8"
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
        .arg("1")
        .arg(&merges[0])
        .arg(&merges[1])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");

    let dirs = std::fs::read_to_string(state.join("dirs")).unwrap();
    let rows: Vec<Vec<&str>> = dirs
        .lines()
        .map(|line| line.split(' ').collect())
        .collect();
    assert_eq!(rows.len(), 2, "both observations must run");
    assert_eq!(
        rows[1][0], rows[0][2],
        "the second observation's parent-a tree must be the first's merged tree"
    );
    assert_eq!(
        std::fs::read_to_string(state.join("carried-into-a")).unwrap(),
        "absent\npresent\n",
        "the reused tree must keep its contents, not just its path"
    );
    assert!(!work.join(".choir-differential").exists());
    std::fs::remove_dir_all(work).ok();
}

#[test]
fn calibrate_fresh_worktrees_gives_each_observation_a_pristine_tree() {
    let (work, first, second) = two_merge_calibration_fixture("fresh");
    let state = work.join("state");
    let output = Command::new(env!("CARGO_BIN_EXE_choir-bridge"))
        .arg("calibrate")
        .arg("--fresh-worktrees")
        .arg(&work)
        .arg(work.join("runner.sh"))
        .arg(work.join("command.json"))
        .arg(&state)
        .arg("1")
        .arg(&first)
        .arg(&second)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");

    let paths = std::fs::read_to_string(state.join("parent-a-paths")).unwrap();
    let distinct: std::collections::BTreeSet<&str> = paths.lines().collect();
    assert_eq!(paths.lines().count(), 2, "both observations must run");
    assert_eq!(
        distinct.len(),
        2,
        "--fresh-worktrees must give every observation its own tree"
    );
    assert_eq!(
        std::fs::read_to_string(state.join("carried")).unwrap(),
        "absent\nabsent\n",
        "--fresh-worktrees must not carry build output between observations"
    );
    assert!(!work.join(".choir-differential").exists());
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

#[test]
fn harvest_cli_replays_enumerated_merges_and_survives_a_failing_one() {
    let (work, first, second) = two_merge_calibration_fixture("harvest-cli");

    // Re-point the fixture's runner: fail the OLDER merge, succeed on the
    // newer one. Harvest replays oldest-first, so a failure on the first
    // observation proves the loop continues rather than aborting the
    // corpus, which is exactly where it differs from calibrate.
    let runner = work.join("runner.sh");
    std::fs::write(
        &runner,
        format!(
            r#"#!/bin/sh
set -eu
mkdir -p "$3"
if [ "$8" = "{first}" ]; then exit 3; fi
printf '%s\n' "$8" >> "$3/observed"
printf '{{"format_version":1,"observation_id":1,"merge":"%s","report":{{"verdict":"clean"}},"calibration":{{"target":{{"met":true}},"pending_interactions":0,"confidence_claim":null,"confidence_policy":{{"format_version":1,"method":"one_sided_exact_binomial_zero_spurious","confidence":{{"numerator":95,"denominator":100}},"target":{{"numerator":1,"denominator":1000,"comparison":"strictly_less_than"}},"minimum_evaluated_merges":2995,"requires_zero_spurious_failures":true,"assumptions":["independent_runs","representative_queue_command_and_merge_population"]}},"landing_gate_enabled":false}}}}\n' "$8"
"#
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    let state = work.join("state");
    let output = Command::new(env!("CARGO_BIN_EXE_choir-bridge"))
        .arg("harvest")
        .arg(&work)
        .arg(&runner)
        .arg(work.join("command.json"))
        .arg(&state)
        .output()
        .unwrap();
    // One failure out of two is a partial harvest, not a failed one.
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("harvest: 1 observed, 1 failed, 0 skipped, 2 enumerated"),
        "{stdout}"
    );
    assert_eq!(
        std::fs::read_to_string(state.join("observed")).unwrap(),
        format!("{second}\n")
    );
    assert!(!work.join(".choir-differential").exists());

    // --limit 1 keeps only the newest merge: the failing older one is
    // never enumerated, so the harvest is clean.
    let output = Command::new(env!("CARGO_BIN_EXE_choir-bridge"))
        .arg("harvest")
        .arg("--limit")
        .arg("1")
        .arg(&work)
        .arg(&runner)
        .arg(work.join("command.json"))
        .arg(&state)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("harvest: 1 observed, 0 failed, 0 skipped, 1 enumerated"),
        "{stdout}"
    );

    // Every merge failing is a failed harvest: exit 1, not a quiet 0.
    std::fs::write(&runner, "#!/bin/sh\nexit 3\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let output = Command::new(env!("CARGO_BIN_EXE_choir-bridge"))
        .arg("harvest")
        .arg(&work)
        .arg(&runner)
        .arg(work.join("command.json"))
        .arg(&state)
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");

    std::fs::remove_dir_all(work).ok();
}

#[test]
fn harvest_skips_merges_already_in_the_ledger() {
    let (work, first, second) = two_merge_calibration_fixture("harvest-skip");

    // This stub keeps an observation ledger the way the real runner does:
    // one row per run whose `revisions.merged` is the merge it was handed.
    // That ledger is exactly what an incremental re-run consults.
    let runner = work.join("runner.sh");
    std::fs::write(
        &runner,
        r#"#!/bin/sh
set -eu
mkdir -p "$3"
printf '{"format_version":1,"observation_id":1,"revisions":{"merged":"%s"}}\n' "$8" >> "$3/observations.jsonl"
printf '{"format_version":1,"observation_id":1,"merge":"%s","report":{"verdict":"clean"},"calibration":{"target":{"met":true},"pending_interactions":0,"confidence_claim":null,"confidence_policy":{"format_version":1,"method":"one_sided_exact_binomial_zero_spurious","confidence":{"numerator":95,"denominator":100},"target":{"numerator":1,"denominator":1000,"comparison":"strictly_less_than"},"minimum_evaluated_merges":2995,"requires_zero_spurious_failures":true,"assumptions":["independent_runs","representative_queue_command_and_merge_population"]},"landing_gate_enabled":false}}\n' "$8"
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    let state = work.join("state");
    let harvest = || {
        Command::new(env!("CARGO_BIN_EXE_choir-bridge"))
            .arg("harvest")
            .arg(&work)
            .arg(&runner)
            .arg(work.join("command.json"))
            .arg(&state)
            .output()
            .unwrap()
    };

    let output = harvest();
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("harvest: 2 observed, 0 failed, 0 skipped, 2 enumerated"),
        "{stdout}"
    );

    // Re-running against the same ledger replays nothing, and an
    // everything-skipped harvest is the incremental steady state: exit 0.
    let output = harvest();
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("harvest: 0 observed, 0 failed, 2 skipped, 2 enumerated"),
        "{stdout}"
    );
    let ledger = std::fs::read_to_string(state.join("observations.jsonl")).unwrap();
    assert_eq!(ledger.lines().count(), 2, "the re-run must not extend the ledger");
    assert!(ledger.contains(&first) && ledger.contains(&second));

    std::fs::remove_dir_all(work).ok();
}

#[test]
fn harvest_reproduces_an_interaction_failure_into_a_specimen() {
    let (work, first, second) = two_merge_calibration_fixture("harvest-specimen");

    // The OLDER merge reports interaction_failure on every run; the newer
    // one is clean. The harvest must rerun the flagged merge and package
    // the reproduction counts as a specimen, and must not do so for the
    // clean one.
    let runner = work.join("runner.sh");
    std::fs::write(
        &runner,
        format!(
            r#"#!/bin/sh
set -eu
mkdir -p "$3"
printf '%s\n' "$8" >> "$3/runs"
printf '%s\n' "$5" >> "$3/trees"
verdict=clean
if [ "$8" = "{first}" ]; then verdict=interaction_failure; fi
printf '{{"format_version":1,"observation_id":4,"merge":"%s","report":{{"verdict":"%s"}},"calibration":{{"target":{{"met":null}},"pending_interactions":1,"confidence_claim":null,"confidence_policy":{{"format_version":1,"method":"one_sided_exact_binomial_zero_spurious","confidence":{{"numerator":95,"denominator":100}},"target":{{"numerator":1,"denominator":1000,"comparison":"strictly_less_than"}},"minimum_evaluated_merges":2995,"requires_zero_spurious_failures":true,"assumptions":["independent_runs","representative_queue_command_and_merge_population"]}},"landing_gate_enabled":false}}}}\n' "$8" "$verdict"
"#
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    let state = work.join("state");
    let output = Command::new(env!("CARGO_BIN_EXE_choir-bridge"))
        .arg("harvest")
        .arg(&work)
        .arg(&runner)
        .arg(work.join("command.json"))
        .arg(&state)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("harvest: 2 observed, 0 failed, 0 skipped, 2 enumerated"),
        "{stdout}"
    );
    assert!(stdout.contains("specimen recorded"), "{stdout}");

    // 1 first run + 5 reproductions for the flagged merge, 1 for the clean one.
    let runs = std::fs::read_to_string(state.join("runs")).unwrap();
    assert_eq!(runs.lines().filter(|line| *line == first).count(), 6);
    assert_eq!(runs.lines().filter(|line| *line == second).count(), 1);

    // Reproductions run in fresh worktree triples: the walk's two first
    // runs share the held session tree, and each of the 5 reproductions
    // gets its own, so 6 distinct parent-a paths across 7 runs.
    let trees = std::fs::read_to_string(state.join("trees")).unwrap();
    let distinct: std::collections::BTreeSet<&str> = trees.lines().collect();
    assert_eq!(trees.lines().count(), 7, "{trees}");
    assert_eq!(
        distinct.len(),
        6,
        "reproduction runs must not share the walk's held trees: {trees}"
    );

    let specimen: serde_json::Value = serde_json::from_slice(
        &std::fs::read(state.join("specimens").join(format!("{first}.json"))).unwrap(),
    )
    .unwrap();
    assert_eq!(specimen["format_version"].as_u64(), Some(1));
    assert_eq!(specimen["merge"].as_str(), Some(first.as_str()));
    assert_eq!(specimen["runs"].as_u64(), Some(6));
    assert_eq!(specimen["interaction_failures"].as_u64(), Some(6));
    assert_eq!(specimen["run_errors"].as_u64(), Some(0));
    // The clean merge earned no specimen.
    assert!(!state.join("specimens").join(format!("{second}.json")).exists());

    std::fs::remove_dir_all(work).ok();
}
