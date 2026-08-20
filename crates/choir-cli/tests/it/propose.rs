//! `choir propose` against a real node, from a real clone.
//!
//! The command's whole claim is that five steps collapse into one, so
//! the test is the five-step assertion: after one invocation the change
//! exists, the objects are on the node, the revision is published and a
//! review is open on the destination ref. Each of those was a separate
//! command before, and a version of this that only checked the exit code
//! would pass while doing four fifths of the work.

use choir_identity::{ActorKey, Registry};
use choir_node::platform::hex_decode;
use choir_node::{Node, Platform};
use choir_oplog::MemLog;

fn choir(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_choir"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("choir runs")
}

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

fn git_ok(dir: &std::path::Path, args: &[&str]) -> String {
    let out = git(dir, args);
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn json(out: &std::process::Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
        panic!(
            "json stdout, got {:?} / stderr {:?}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

/// A node with one repository, one registered key, and a reviewer pool.
struct Fixture {
    _work: std::path::PathBuf,
    api: String,
    key_file: String,
    clone: std::path::PathBuf,
    /// The node's own bare repository, so a push can be checked against
    /// what actually landed rather than against its exit code.
    bare: std::path::PathBuf,
}

fn fixture(name: &str) -> Fixture {
    // Named per test rather than per process: this harness merges many
    // modules into one binary, so two tests sharing a pid-derived path
    // would share a directory.
    let work = std::env::temp_dir().join(format!("choir-propose-{name}"));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();

    let key_file = work.join("agent.key");
    let key_file = key_file.to_str().unwrap().to_string();
    let out = choir(&work, &["key", &key_file, "ana/agent"]);
    assert!(out.status.success());
    let pub_hex = String::from_utf8_lossy(&out.stdout)
        .trim()
        .rsplit(' ')
        .next()
        .expect("key hex")
        .to_string();
    let key_bytes: [u8; 32] = hex_decode(&pub_hex)
        .and_then(|b| b.try_into().ok())
        .expect("64 hex chars");
    let mut registry = Registry::new();
    registry.register(&key_bytes).unwrap();

    let mut node = Node::bind(&work.join("repos"), 0).unwrap();
    let pool = work.join("reviewers");
    // A pool with no name sharing the proposer's operator prefix, so the
    // draw has somebody eligible to pick.
    std::fs::write(&pool, "bo/bot\ncy/cyn\n").unwrap();
    node.enable_platform(
        Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate())
            .unwrap()
            .with_reviewer_pool(pool),
    );
    node.create_repo("agents/demo.git").unwrap();
    let port = node.port();
    let node = std::sync::Arc::new(node);
    std::thread::spawn(move || node.serve_forever());
    let api = format!("http://127.0.0.1:{port}");

    // Seed main, then clone it the way a contributor would.
    let url = format!("{api}/agents/demo.git");
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

    let bare = work.join("repos").join("agents").join("demo.git");
    Fixture {
        _work: work,
        api,
        key_file,
        clone,
        bare,
    }
}

fn view(fixture: &Fixture) -> serde_json::Value {
    let out = choir(&fixture.clone, &["view", &fixture.api]);
    assert!(out.status.success());
    json(&out)
}

#[test]
fn one_command_creates_pushes_checkpoints_and_requests_review() {
    let f = fixture("one-command");

    git(&f.clone, &["checkout", "-q", "-b", "fix-parser"]);
    std::fs::write(f.clone.join("f.txt"), "v2\n").unwrap();
    git(&f.clone, &["add", "."]);
    git(&f.clone, &["commit", "-q", "-m", "fix the parser"]);
    let head = git_ok(&f.clone, &["rev-parse", "HEAD"]);

    // The whole command: no api, no repo, no change id, no base, no
    // workspace name, no reviewer. Everything is inferred or derived.
    let out = choir(&f.clone, &["propose", &f.key_file, "ana/agent"]);
    assert!(
        out.status.success(),
        "propose failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let summary = json(&out);
    let change = summary["change"].as_str().expect("change id").to_string();

    // 1. The change exists and is owned by the proposer.
    let view = view(&f);
    let state = &view["changes"][&change];
    assert_eq!(state["owner"], "ana/agent");

    // 2. The objects reached the node. Checked by asking the node's own
    //    repository, not by trusting the push's exit code.
    let pushed = summary["pushed_ref"].as_str().expect("pushed ref");
    let on_node = git_ok(&f.bare, &["rev-parse", &format!("{pushed}^{{commit}}")]);
    assert_eq!(on_node, head, "the proposal ref does not point at HEAD");

    // 3. The revision is published, and it is the commit that was
    //    pushed rather than the base it started from.
    let revision = state["revision_id"].as_str().expect("revision");
    assert!(
        revision.ends_with(&head),
        "revision {revision} is not the pushed commit {head}"
    );
    assert_ne!(state["revision_id"], state["base_revision"]);

    // 4. A review is open, naming the destination branch and not the
    //    proposal's own ref.
    let review = &view["reviews"][&change];
    assert_eq!(review["target_ref"], "agents/demo:refs/heads/main");
    let reviewers = review["reviewers"].as_array().expect("drawn reviewers");
    assert!(!reviewers.is_empty(), "the node drew no reviewer");
    assert!(
        reviewers.iter().all(|r| r.as_str() != Some("ana/agent")),
        "the proposer was drawn onto their own review: {reviewers:?}"
    );
}

#[test]
fn re_proposing_after_an_amend_updates_the_same_change() {
    let f = fixture("amend");

    git(&f.clone, &["checkout", "-q", "-b", "fix-lexer"]);
    std::fs::write(f.clone.join("f.txt"), "v2\n").unwrap();
    git(&f.clone, &["add", "."]);
    git(&f.clone, &["commit", "-q", "-m", "first try"]);
    let first = choir(&f.clone, &["propose", &f.key_file, "ana/agent"]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first = json(&first);

    // Amend: a new commit id for the same unit of work. This is the case
    // that forks into a second proposal if identity comes from the
    // commit rather than the branch.
    std::fs::write(f.clone.join("f.txt"), "v3\n").unwrap();
    git(&f.clone, &["add", "."]);
    git(&f.clone, &["commit", "-q", "--amend", "-m", "second try"]);
    let amended = git_ok(&f.clone, &["rev-parse", "HEAD"]);

    let second = choir(&f.clone, &["propose", &f.key_file, "ana/agent"]);
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let second = json(&second);
    assert_eq!(
        first["change"], second["change"],
        "an amend opened a second change"
    );
    assert_ne!(first["commit"], second["commit"]);

    let view = view(&f);
    let changes = view["changes"].as_object().expect("changes");
    assert_eq!(changes.len(), 1, "expected one change, got {changes:?}");
    let revision = changes[second["change"].as_str().unwrap()]["revision_id"]
        .as_str()
        .expect("revision");
    assert!(
        revision.ends_with(&amended),
        "the change still points at the pre-amend commit"
    );

    // Append-only: the superseded revision is still fetchable from the
    // node. The op log records that checkpoint permanently, so a repo
    // that could no longer produce its objects would be a log naming a
    // revision nothing can check out.
    for step in [&first, &second] {
        let pushed = step["pushed_ref"].as_str().expect("pushed ref");
        let commit = step["commit"].as_str().expect("commit");
        assert_eq!(
            git_ok(&f.bare, &["rev-parse", &format!("{pushed}^{{commit}}")]),
            commit,
            "{pushed} no longer resolves to the revision it recorded"
        );
    }
    assert_ne!(first["pushed_ref"], second["pushed_ref"]);
}

#[test]
fn two_branches_are_two_proposals() {
    let f = fixture("two-branches");

    for (branch, body) in [("feat-a", "a\n"), ("feat-b", "b\n")] {
        git(&f.clone, &["checkout", "-q", "main"]);
        git(&f.clone, &["checkout", "-q", "-b", branch]);
        std::fs::write(f.clone.join("f.txt"), body).unwrap();
        git(&f.clone, &["add", "."]);
        git(&f.clone, &["commit", "-q", "-m", branch]);
        let out = choir(&f.clone, &["propose", &f.key_file, "ana/agent"]);
        assert!(
            out.status.success(),
            "{branch}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let view = view(&f);
    let changes = view["changes"].as_object().expect("changes");
    assert_eq!(changes.len(), 2, "expected two changes, got {changes:?}");
    // Two proposals must not have raced onto one ref: the proposal ref
    // is the derived workspace name, not the contributor's branch name.
    let workspaces: std::collections::BTreeSet<&str> = changes
        .values()
        .filter_map(|c| c["active_workspace"].as_str())
        .collect();
    assert_eq!(workspaces.len(), 2, "two changes share one workspace");
}

#[test]
fn a_detached_head_is_refused_before_anything_is_pushed() {
    let f = fixture("detached");

    let head = git_ok(&f.clone, &["rev-parse", "HEAD"]);
    git(&f.clone, &["checkout", "-q", &head]);
    let out = choir(&f.clone, &["propose", &f.key_file, "ana/agent"]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("detached"), "{stderr}");
    // Refused before the node was touched: no half-made change.
    let view = view(&f);
    assert!(
        view["changes"]
            .as_object()
            .is_none_or(serde_json::Map::is_empty),
        "a refused proposal still created a change"
    );
}

#[test]
fn a_remote_that_is_not_a_choir_url_names_the_override() {
    let f = fixture("bad-remote");
    git(
        &f.clone,
        &[
            "remote",
            "set-url",
            "origin",
            "ssh://git@example.invalid/agents/demo.git",
        ],
    );
    git(&f.clone, &["checkout", "-q", "-b", "whatever"]);
    let out = choir(&f.clone, &["propose", &f.key_file, "ana/agent"]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--api"), "{stderr}");
}
