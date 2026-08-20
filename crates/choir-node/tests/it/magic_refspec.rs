//! The magic refspec: `git push origin HEAD:refs/for/<branch>/<topic>`
//! opens a review, with no client but `git`.
//!
//! Every assertion here is made with git and curl only. That is the
//! point of the feature — a contributor who has installed nothing can
//! still propose — so a test that reached for the `choir` binary would
//! be testing a path this does not claim.

use choir_identity::{ActorKey, Registry};
use choir_node::{Node, Platform};
use choir_oplog::MemLog;

fn git(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
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

fn view(port: u16) -> serde_json::Value {
    let out = std::process::Command::new("curl")
        .args(["-s", &format!("http://127.0.0.1:{port}/api/view")])
        .output()
        .expect("curl runs");
    serde_json::from_slice(&out.stdout).expect("view json")
}

struct Fixture {
    work: std::path::PathBuf,
    port: u16,
    clone: std::path::PathBuf,
}

fn fixture(tag: &str) -> Fixture {
    let work = std::env::temp_dir().join(format!("choir-magic-{tag}"));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();

    let pool = work.join("reviewers");
    std::fs::write(&pool, "bo/bot\ncy/cyn\n").unwrap();
    let mut node = Node::bind(&work.join("repos"), 0).unwrap();
    node.enable_platform(
        Platform::start(
            Registry::new(),
            Box::new(MemLog::new()),
            ActorKey::generate(),
        )
        .unwrap()
        .with_reviewer_pool(pool),
    );
    let port = node.port();
    node.create_repo("agents/demo.git").unwrap();
    let node = std::sync::Arc::new(node);
    std::thread::spawn(move || node.serve_forever());

    let url = format!("http://127.0.0.1:{port}/agents/demo.git");
    let seed = work.join("seed");
    assert!(git(&work, &["clone", "-q", &url, seed.to_str().unwrap()])
        .status
        .success());
    std::fs::write(seed.join("f.txt"), "v1\n").unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-q", "-m", "first"]);
    assert!(git(&seed, &["push", "-q", "origin", "HEAD:main"])
        .status
        .success());

    let clone = work.join("clone");
    assert!(git(&work, &["clone", "-q", &url, clone.to_str().unwrap()])
        .status
        .success());
    Fixture { work, port, clone }
}

fn commit(f: &Fixture, body: &str, message: &str) -> String {
    std::fs::write(f.clone.join("f.txt"), body).unwrap();
    git(&f.clone, &["add", "."]);
    git(&f.clone, &["commit", "-q", "-m", message]);
    String::from_utf8_lossy(&git(&f.clone, &["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_string()
}

#[test]
fn one_push_opens_a_review_with_drawn_reviewers() {
    let f = fixture("opens");
    let head = commit(&f, "v2\n", "fix the parser");

    let pushed = git(
        &f.clone,
        &["push", "origin", "HEAD:refs/for/main/fix-parser"],
    );
    assert!(
        pushed.status.success(),
        "{}",
        String::from_utf8_lossy(&pushed.stderr)
    );

    let view = view(f.port);
    // The ref really exists: this node does not report a ref it did not
    // write, which is the departure from Gerrit that keeps push receipts
    // meaningful.
    assert_eq!(
        view["refs"]["agents/demo.git:refs/for/main/fix-parser"]
            .as_str()
            .map(|r| r.ends_with(&head)),
        Some(true),
        "the proposal ref is not in the view: {}",
        view["refs"]
    );

    let review = &view["reviews"]["for-main-fix-parser"];
    assert!(
        !review.is_null(),
        "no review was opened: {}",
        view["reviews"]
    );
    // The `.git` spelling is what the push hook reports as the
    // repository; `normalize_repo` folds it, so this review still
    // appears on `/r/agents/demo/reviews`.
    assert_eq!(review["target_ref"], "agents/demo.git:refs/heads/main");
    assert!(
        review["target"]
            .as_str()
            .is_some_and(|t| t.ends_with(&head)),
        "the review does not name the pushed commit"
    );
    let reviewers = review["reviewers"].as_array().expect("reviewers");
    assert!(!reviewers.is_empty(), "the node drew no reviewer");
}

#[test]
fn pushing_the_topic_again_updates_rather_than_duplicating() {
    let f = fixture("update");
    commit(&f, "v2\n", "first try");
    assert!(git(&f.clone, &["push", "origin", "HEAD:refs/for/main/fix"])
        .status
        .success());

    let second = commit(&f, "v3\n", "second try");
    let again = git(&f.clone, &["push", "origin", "HEAD:refs/for/main/fix"]);
    assert!(
        again.status.success(),
        "{}",
        String::from_utf8_lossy(&again.stderr)
    );

    let view = view(f.port);
    let reviews = view["reviews"].as_object().expect("reviews");
    assert_eq!(reviews.len(), 1, "a second push opened a second review");
    // The ref advanced, which is where a reviewer reads the current
    // commit from.
    assert!(view["refs"]["agents/demo.git:refs/for/main/fix"]
        .as_str()
        .is_some_and(|r| r.ends_with(&second)));
}

#[test]
fn two_topics_are_two_reviews_and_do_not_fight_over_one_ref() {
    let f = fixture("two-topics");
    commit(&f, "a\n", "a");
    assert!(
        git(&f.clone, &["push", "origin", "HEAD:refs/for/main/topic-a"])
            .status
            .success()
    );
    git(&f.clone, &["checkout", "-q", "-B", "b", "origin/main"]);
    commit(&f, "b\n", "b");
    let second = git(&f.clone, &["push", "origin", "HEAD:refs/for/main/topic-b"]);
    assert!(
        second.status.success(),
        "the second topic was refused: {}",
        String::from_utf8_lossy(&second.stderr)
    );

    let view = view(f.port);
    assert_eq!(view["reviews"].as_object().expect("reviews").len(), 2);
}

#[test]
fn a_push_with_no_topic_is_refused_and_creates_nothing() {
    let f = fixture("no-topic");
    commit(&f, "v2\n", "fix");
    let pushed = git(&f.clone, &["push", "origin", "HEAD:refs/for/main"]);
    assert!(
        !pushed.status.success(),
        "a topicless proposal was accepted, so every proposal onto main shares one ref"
    );
    // The reason reaches the pusher, because git relays a hook's stderr.
    let stderr = String::from_utf8_lossy(&pushed.stderr);
    assert!(
        stderr.contains("topic"),
        "the refusal did not say why: {stderr}"
    );

    // Refused whole: no ref, no review.
    let view = view(f.port);
    assert!(
        view["refs"]["agents/demo.git:refs/for/main"].is_null(),
        "the refused push still created a ref"
    );
    assert!(
        view["reviews"]
            .as_object()
            .is_none_or(serde_json::Map::is_empty),
        "the refused push still opened a review"
    );
}

#[test]
fn an_ordinary_push_still_opens_no_review() {
    // The cost of the feature on every push that is not a proposal.
    let f = fixture("ordinary");
    commit(&f, "v2\n", "direct");
    assert!(git(&f.clone, &["push", "-q", "origin", "HEAD:main"])
        .status
        .success());
    let view = view(f.port);
    assert!(
        view["reviews"]
            .as_object()
            .is_none_or(serde_json::Map::is_empty),
        "an ordinary push opened a review: {}",
        view["reviews"]
    );
    let _ = &f.work;
}
