//! Followers (D21): a landing reaches the remote the operator added, and
//! nothing else does.
//!
//! A node started with followers, one repository with a remote pointing
//! at a bare repository on this disk, and a real push through the node.
//! The follower is then read back with git, not inferred from a log
//! line, because the log line is written by the same thread whose work
//! is under test.

use choir_identity::{ActorKey, Registry};
use choir_node::followers::{push_all, remotes, repository_of, Outcome};
use choir_node::{Node, Platform};
use choir_oplog::MemLog;
use std::path::{Path, PathBuf};
use std::time::Duration;

const PATIENCE: Duration = Duration::from_secs(20);

fn git(dir: &Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
            "-c",
            "credential.helper=",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("git runs")
}

fn ok(out: std::process::Output) -> String {
    assert!(out.status.success(), "{out:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

struct Fixture {
    work: PathBuf,
    url: String,
    log: PathBuf,
}

/// A node with followers on, one repository, and a bare `mirror`
/// beside it that the repository names as a remote.
fn node_with_followers(label: &str) -> Fixture {
    let work = std::env::temp_dir().join(format!(
        "choir-node-followers-{label}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    let root = work.join("repos");
    let log = root.join(".choir").join("followers.jsonl");
    std::fs::create_dir_all(root.join(".choir")).unwrap();

    let mirror = work.join("mirror.git");
    ok(git(&work, &["init", "-q", "--bare", "mirror.git"]));

    let platform = Platform::start(
        Registry::new(),
        Box::new(MemLog::new()),
        ActorKey::generate(),
    )
    .unwrap()
    .with_followers(root.clone(), log.clone())
    .unwrap();
    let mut node = Node::bind(&root, 0).unwrap();
    node.create_repo("owner/repo.git").unwrap();
    node.enable_platform(platform);
    let url = format!("http://127.0.0.1:{}/owner/repo.git", node.port());
    std::thread::spawn(move || node.serve_forever());

    ok(git(
        &work,
        &[
            "--git-dir",
            root.join("owner/repo.git").to_str().unwrap(),
            "remote",
            "add",
            "mirror",
            mirror.to_str().unwrap(),
        ],
    ));
    Fixture { work, url, log }
}

/// One commit pushed to `main` through the node; its oid.
fn push_one(f: &Fixture, name: &str) -> String {
    let clone = f.work.join(name);
    ok(git(&f.work, &["clone", "-q", &f.url, name]));
    std::fs::write(clone.join("f.txt"), format!("{name}\n")).unwrap();
    ok(git(&clone, &["add", "."]));
    ok(git(&clone, &["commit", "-q", "-m", name]));
    ok(git(&clone, &["push", "-q", "origin", "HEAD:main"]));
    ok(git(&clone, &["rev-parse", "HEAD"]))
}

fn mirror_main(f: &Fixture) -> Option<String> {
    let out = git(
        &f.work,
        &[
            "--git-dir",
            f.work.join("mirror.git").to_str().unwrap(),
            "rev-parse",
            "--verify",
            "-q",
            "refs/heads/main",
        ],
    );
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn wait_for_mirror(f: &Fixture, oid: &str) {
    let deadline = std::time::Instant::now() + PATIENCE;
    loop {
        if mirror_main(f).as_deref() == Some(oid) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the mirror never received {oid}; log:\n{}",
            std::fs::read_to_string(&f.log).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn a_landing_is_pushed_to_the_repositorys_remote() {
    let f = node_with_followers("pushed");
    assert_eq!(mirror_main(&f), None, "the mirror starts empty");
    let oid = push_one(&f, "one");
    wait_for_mirror(&f, &oid);

    let records = std::fs::read_to_string(&f.log).unwrap();
    let pushed = records
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|r| r["event"] == "pushed")
        .expect("a pushed record");
    assert_eq!(pushed["repo"], "owner/repo.git");
    assert_eq!(pushed["remote"], "mirror");
}

#[test]
fn a_second_landing_follows_and_the_mirror_is_never_forced() {
    let f = node_with_followers("twice");
    let first = push_one(&f, "one");
    wait_for_mirror(&f, &first);

    // Somebody moves the mirror's main on their own. The node's next
    // push is a non-fast-forward and must refuse rather than overwrite.
    let stray = f.work.join("stray");
    ok(git(
        &f.work,
        &[
            "clone",
            "-q",
            f.work.join("mirror.git").to_str().unwrap(),
            "stray",
        ],
    ));
    std::fs::write(stray.join("g.txt"), "stray\n").unwrap();
    ok(git(&stray, &["add", "."]));
    ok(git(&stray, &["commit", "-q", "--amend", "-m", "rewritten"]));
    ok(git(&stray, &["push", "-q", "-f", "origin", "HEAD:main"]));
    let diverged = mirror_main(&f).unwrap();
    assert_ne!(diverged, first);

    let _second = push_one(&f, "two");
    let deadline = std::time::Instant::now() + PATIENCE;
    let failed = loop {
        let records = std::fs::read_to_string(&f.log).unwrap_or_default();
        if let Some(r) = records
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .find(|r| r["event"] == "failed")
        {
            break r;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no refusal recorded:\n{records}"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(failed["remote"], "mirror");
    assert_eq!(
        mirror_main(&f).unwrap(),
        diverged,
        "a follower that diverged is left as it is"
    );
}

#[test]
fn the_repository_is_the_part_of_the_key_before_the_refname() {
    assert_eq!(
        repository_of("owner/repo.git:refs/heads/main"),
        Some("owner/repo.git")
    );
    assert_eq!(repository_of("owner/repo.git:HEAD"), None);
    assert_eq!(repository_of(":refs/heads/main"), None);
}

#[test]
fn pushing_by_hand_names_a_repository_with_no_remote_and_one_that_is_not_there() {
    let work =
        std::env::temp_dir().join(format!("choir-node-followers-hand-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    ok(git(&work, &["init", "-q", "--bare", "lonely.git"]));
    assert!(remotes(&work.join("lonely.git")).is_empty());

    let outcomes = push_all(&work, "lonely.git");
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].event, "no_remote");
    assert!(outcomes[0].is_ok(), "no remote is named, not failed");

    let outcomes = push_all(&work, "absent.git");
    assert_eq!(outcomes[0].event, "no_repository");
    assert!(!outcomes[0].is_ok());

    let sample = Outcome {
        repo: "x".into(),
        remote: "m".into(),
        event: "failed",
        detail: String::new(),
    };
    assert!(!sample.is_ok());
}
