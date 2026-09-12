//! `choir repo follower` (D21): the remote lands on the bare repository,
//! the marker turns into `--followers`, and a push by hand goes where
//! the daemon's would.

use choir_cli::follower;
use choir_cli::serve::{plan, Layout};
use std::path::{Path, PathBuf};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("git runs");
    assert!(out.status.success(), "{out:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A state directory holding one bare repository with a commit, listed
/// in repos.list, and a bare `mirror.git` beside it to push to.
fn state(tag: &str) -> (PathBuf, Layout) {
    let root =
        std::env::temp_dir().join(format!("choir-cli-follower-{tag}-{}", std::process::id()));
    std::fs::remove_dir_all(&root).ok();
    let state = root.join("state");
    let layout = Layout::new(&state, 8417);
    std::fs::create_dir_all(layout.repos.join("me")).expect("repos");
    std::fs::write(&layout.auth, "choir:t\n").expect("auth");
    std::fs::write(&layout.keys, "op ab\n").expect("keys");
    std::fs::write(state.join("repos.list"), "me/thing.git\n").expect("repos.list");
    git(
        &layout.repos.join("me"),
        &["init", "-q", "--bare", "thing.git"],
    );
    git(&root, &["init", "-q", "--bare", "mirror.git"]);
    let src = root.join("src");
    std::fs::create_dir_all(&src).expect("src");
    git(&src, &["init", "-q", "."]);
    std::fs::write(src.join("f"), "hi\n").expect("file");
    git(&src, &["add", "-A"]);
    git(&src, &["commit", "-qm", "one"]);
    git(
        &src,
        &[
            "push",
            "-q",
            layout.repos.join("me/thing.git").to_str().unwrap(),
            "HEAD:main",
        ],
    );
    (root, layout)
}

#[test]
fn adding_a_follower_writes_the_remote_and_the_marker_once() {
    let (root, layout) = state("add");
    let mirror = root.join("mirror.git").display().to_string();
    let first = follower::add(&layout, "me/thing", "mirror", &mirror).expect("added");
    assert!(first, "the marker was written now");
    assert!(layout.followers_marker.exists());
    assert_eq!(
        follower::list(&layout),
        vec![(
            "me/thing.git".to_string(),
            vec![("mirror".to_string(), mirror.clone())]
        )]
    );

    let error = follower::add(&layout, "me/thing.git", "mirror", &mirror).expect_err("twice");
    assert!(
        error.contains("already has a remote named mirror"),
        "{error}"
    );
    let error = follower::add(&layout, "me/other.git", "m", &mirror).expect_err("absent");
    assert!(error.contains("no repository me/other.git"), "{error}");

    let second = follower::add(&layout, "me/thing.git", "second", &mirror).expect("another");
    assert!(!second, "the marker was already there");
}

#[test]
fn the_marker_becomes_the_daemon_flag() {
    let (_root, layout) = state("flag");
    let before = plan(PathBuf::from("choir-node"), &layout, &[], &[]).expect("plans");
    assert!(!before.args.iter().any(|a| a == "--followers"));
    std::fs::write(&layout.followers_marker, "").expect("marker");
    let after = plan(PathBuf::from("choir-node"), &layout, &[], &["--x".into()]).expect("plans");
    let at = after
        .args
        .iter()
        .position(|a| a == "--followers")
        .expect("flag");
    assert!(
        at < after.args.len() - 1,
        "derived flags come before the pass-through ones"
    );
    assert!(follower::following(&layout));
}

#[test]
fn a_push_by_hand_reaches_the_follower_and_names_a_repository_without_one() {
    let (root, layout) = state("push");
    let outcomes = follower::push(&layout, None);
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].event, "no_remote");

    let mirror = root.join("mirror.git");
    follower::add(&layout, "me/thing", "mirror", mirror.to_str().unwrap()).expect("added");
    let outcomes = follower::push(&layout, Some("me/thing"));
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].event, "pushed", "{outcomes:?}");
    let want = git(
        &layout.repos.join("me/thing.git"),
        &["rev-parse", "refs/heads/main"],
    );
    let got = git(&mirror, &["rev-parse", "refs/heads/main"]);
    assert_eq!(got, want);
}

#[test]
fn the_command_lists_pushes_and_refuses_what_it_does_not_know() {
    let (root, layout) = state("command");
    let choir = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_choir"))
            .args(args)
            .arg("--state")
            .arg(&layout.state)
            .current_dir(&root)
            .output()
            .expect("runs")
    };
    let out = choir(&["repo", "follower", "list"]);
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("me/thing.git") && text.contains("no remote"),
        "{text}"
    );

    let mirror = root.join("mirror.git").display().to_string();
    let out = choir(&["repo", "follower", "add", "me/thing.git", "mirror", &mirror]);
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(text.contains("choir node restart"), "{text}");

    let out = choir(&["repo", "follower", "push"]);
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("pushed me/thing.git -> mirror"));

    let out = choir(&["repo", "follower", "forget"]);
    assert_eq!(out.status.code(), Some(2));
}
