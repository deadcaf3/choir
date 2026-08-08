//! Offline tests for the queue stage: train mechanics against local
//! repos, and the GitHub JSON parsers against canned responses. No
//! network, no App credentials.

use std::path::Path;

use choir_bridge::github::{parse_check_verdict, parse_prs, Verdict};
use choir_bridge::queue::build_train;

fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "tag.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn rev(dir: &Path, name: &str) -> String {
    let out = std::process::Command::new("git")
        .args(["rev-parse", name])
        .current_dir(dir)
        .output()
        .expect("git runs");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A repo with main plus three branches: two touch distinct files, one
/// conflicts with the first.
fn fixture(dir: &Path) {
    git(dir, &["init", "-q"]);
    std::fs::write(dir.join("base.txt"), "base\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "base"]);
    for (branch, file, content) in [
        ("pr-1", "a.txt", "one\n"),
        ("pr-2", "b.txt", "two\n"),
        ("pr-conflict", "a.txt", "clash\n"),
    ] {
        git(dir, &["checkout", "-q", "-b", branch, "main"]);
        std::fs::write(dir.join(file), content).unwrap();
        git(dir, &["add", "."]);
        git(dir, &["commit", "-q", "-m", branch]);
    }
    git(dir, &["checkout", "-q", "main"]);
}

#[test]
fn clean_train_merges_all_in_order() {
    let dir = tempdir("clean");
    fixture(&dir);
    let base = rev(&dir, "main");
    let prs = vec![(1, rev(&dir, "pr-1")), (2, rev(&dir, "pr-2"))];
    let train = build_train(&dir, &base, &prs).unwrap();
    assert_ne!(train.tip, base);
    assert!(train.entries.iter().all(|e| e.merged));
    assert_eq!(train.entries[0].note, "train position 1");
    assert_eq!(train.entries[1].note, "train position 2");
    // Tip contains both changes and descends from base.
    git(&dir, &["checkout", "-q", "--detach", &train.tip]);
    assert!(dir.join("a.txt").exists() && dir.join("b.txt").exists());
    git(&dir, &["merge-base", "--is-ancestor", &base, &train.tip]);
}

#[test]
fn conflicting_pr_is_excluded_and_train_continues() {
    let dir = tempdir("conflict");
    fixture(&dir);
    let base = rev(&dir, "main");
    let prs = vec![
        (1, rev(&dir, "pr-1")),
        (3, rev(&dir, "pr-conflict")), // touches a.txt like pr-1
        (2, rev(&dir, "pr-2")),
    ];
    let train = build_train(&dir, &base, &prs).unwrap();
    assert_eq!(
        train.entries.iter().map(|e| e.merged).collect::<Vec<_>>(),
        vec![true, false, true]
    );
    assert_eq!(train.entries[1].note, "conflicts with train");
    // The conflicting PR left no residue: tip has pr-1's a.txt content.
    git(&dir, &["checkout", "-q", "--detach", &train.tip]);
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "one\n");
}

#[test]
fn empty_pr_list_leaves_train_at_base() {
    let dir = tempdir("empty");
    fixture(&dir);
    let base = rev(&dir, "main");
    let train = build_train(&dir, &base, &[]).unwrap();
    assert_eq!(train.tip, base);
    assert!(train.entries.is_empty());
}

#[test]
fn landing_fast_forwards_and_rejects_stale_trains() {
    let dir = tempdir("land");
    fixture(&dir);
    let remote = tempdir("land-remote");
    git(&dir, &["clone", "-q", "--bare", dir.to_str().unwrap(), remote.to_str().unwrap()]);
    let url = remote.to_str().unwrap().to_string();

    let base = rev(&dir, "main");
    let train =
        build_train(&dir, &base, &[(1, rev(&dir, "pr-1")), (2, rev(&dir, "pr-2"))]).unwrap();
    choir_bridge::queue::land(&dir, &url, &train.tip, "main").unwrap();
    assert_eq!(rev(&remote, "main"), train.tip);

    // A train built from the now-stale base must be rejected, not clobber.
    let stale = build_train(&dir, &base, &[(3, rev(&dir, "pr-conflict"))]).unwrap();
    assert_ne!(stale.tip, train.tip);
    let err = choir_bridge::queue::land(&dir, &url, &stale.tip, "main").unwrap_err();
    assert!(err.contains("rejected") || err.contains("fast-forward"), "{err}");
    assert_eq!(rev(&remote, "main"), train.tip, "remote main must be untouched");
}

#[test]
fn pr_parse_orders_by_number() {
    let body = r#"[
        {"number": 7, "head": {"sha": "bbb"}},
        {"number": 3, "head": {"sha": "aaa"}}
    ]"#;
    let prs = parse_prs(body);
    assert_eq!(prs.len(), 2);
    assert_eq!((prs[0].number, prs[0].head_sha.as_str()), (3, "aaa"));
    assert_eq!((prs[1].number, prs[1].head_sha.as_str()), (7, "bbb"));
}

#[test]
fn check_verdict_mapping() {
    let runs = |items: &str| format!(r#"{{"check_runs": [{items}]}}"#);
    assert_eq!(parse_check_verdict(&runs("")), Verdict::NoRuns);
    assert_eq!(
        parse_check_verdict(&runs(r#"{"status":"in_progress","conclusion":null}"#)),
        Verdict::Pending
    );
    assert_eq!(
        parse_check_verdict(&runs(
            r#"{"status":"completed","conclusion":"success"},
               {"status":"completed","conclusion":"skipped"}"#
        )),
        Verdict::Success
    );
    assert_eq!(
        parse_check_verdict(&runs(
            r#"{"status":"completed","conclusion":"success"},
               {"status":"completed","conclusion":"failure"}"#
        )),
        Verdict::Failure
    );
    // One still running + one failed: pending wins (train not decided).
    assert_eq!(
        parse_check_verdict(&runs(
            r#"{"status":"queued","conclusion":null},
               {"status":"completed","conclusion":"failure"}"#
        )),
        Verdict::Pending
    );
}

fn tempdir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("choir-queue-test-{tag}-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
