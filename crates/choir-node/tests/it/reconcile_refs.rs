//! Startup reconciliation: the log and the bare repos must agree before
//! the node serves anything.
//!
//! The hook's retraction path covers a push refused while the daemon is
//! alive. Everything else that strands an accepted op — power loss
//! between the hook's 200 and git writing the ref, a per-ref failure
//! after `pre-receive` passed, a retraction that could not be delivered
//! — leaves the same state, and this is what reads it. The repairs are
//! deliberately asymmetric: git is the follower, so it is moved to the
//! log where it can be, and only where git *cannot* honour the log does
//! the log give way.

use choir_identity::{ActorKey, Registry};
use choir_node::{Node, Platform};
use choir_oplog::MemLog;
use choir_view::{OpKind, ViewOp};

use crate::support::{curl, submit_body};

fn git(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .args(["-c", "commit.gpgsign=false", "-c", "init.defaultBranch=main"])
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

/// A node with a repo, a clone, and one commit pushed to `main`.
struct Fixture {
    work: std::path::PathBuf,
    node: std::sync::Arc<Node>,
    port: u16,
    clone: std::path::PathBuf,
    bare: std::path::PathBuf,
    key: ActorKey,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let work = std::env::temp_dir().join(format!(
            "choir-reconcile-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&work).unwrap();

        let key = ActorKey::generate();
        let mut registry = Registry::new();
        registry.register(&key.public_key_bytes()).unwrap();

        let mut node = Node::bind(&work.join("repos"), 0).unwrap();
        node.enable_platform(
            Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate()).unwrap(),
        );
        let port = node.port();
        node.create_repo("agents/demo.git").unwrap();
        let node = std::sync::Arc::new(node);
        {
            let node = node.clone();
            std::thread::spawn(move || node.serve_forever());
        }

        let url = format!("http://127.0.0.1:{port}/agents/demo.git");
        let clone = work.join("clone");
        assert!(git(&work, &["clone", "-q", &url, clone.to_str().unwrap()]).status.success());
        std::fs::write(clone.join("f.txt"), "one\n").unwrap();
        git(&clone, &["add", "."]);
        git(&clone, &["commit", "-q", "-m", "first"]);
        assert!(git(&clone, &["push", "-q", "origin", "HEAD:main"]).status.success());

        let bare = work.join("repos").join("agents/demo.git");
        Self { work, node, port, clone, bare, key }
    }

    fn head(&self) -> String {
        String::from_utf8(git(&self.clone, &["rev-parse", "HEAD"]).stdout)
            .unwrap()
            .trim()
            .to_string()
    }

    fn view(&self) -> serde_json::Value {
        let (_, v) = curl(&[&format!("http://127.0.0.1:{}/api/view", self.port)]);
        v
    }

    fn git_ref(&self, refname: &str) -> Option<String> {
        let out = git(&self.bare, &["rev-parse", "--verify", "-q", refname]);
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.node.unblock();
        std::fs::remove_dir_all(&self.work).ok();
    }
}

/// The power-loss shape: the op is durable, git never wrote the ref, and
/// the commit is sitting in the repo because the push's objects had
/// already been migrated out of the quarantine. Git is the follower, so
/// it is moved to the log.
#[test]
fn a_ref_the_log_holds_and_git_lost_is_written_back_into_git() {
    let f = Fixture::new("behind");
    let head = f.head();

    // Delete the ref behind the node's back: the op stays in the log,
    // exactly as a crash between the hook's 200 and git's write leaves it.
    assert!(git(&f.bare, &["update-ref", "-d", "refs/heads/main"]).status.success());
    assert_eq!(f.git_ref("refs/heads/main"), None);

    let report = f.node.reconcile_refs();
    assert_eq!(report.applied, vec!["agents/demo.git:refs/heads/main".to_string()]);
    assert!(report.retracted.is_empty(), "{report:?}");
    assert!(report.unreconciled.is_empty(), "{report:?}");

    assert_eq!(f.git_ref("refs/heads/main").as_deref(), Some(head.as_str()));
    assert_eq!(
        f.view()["refs"]["agents/demo.git:refs/heads/main"].as_str(),
        Some(format!("11-{head}").as_str()),
        "the log is the source of truth and must not have moved"
    );
}

/// The refused-push shape: the log names a commit the repo does not have
/// and never will, because the objects went out with the quarantine.
/// Git cannot be moved to the log, so the log gives way — through a
/// compensating op, since the log is append-only.
#[test]
fn a_ref_naming_a_commit_git_does_not_have_is_retracted_from_the_log() {
    let f = Fixture::new("phantom");

    // A ref in the view whose commit was never admitted to the repo.
    let op = ViewOp::new(OpKind::SetRef {
        name: "agents/demo.git:refs/heads/ghost".into(),
        commit: choir_oplog::ContentHash::from_git_oid(&"a".repeat(40)).unwrap(),
        prev: None,
    });
    let (code, resp) = curl(&[
        "-X", "POST", "-d", &submit_body(&f.key, "alice", &op),
        &format!("http://127.0.0.1:{}/api/submit", f.port),
    ]);
    assert_eq!(code, 200, "{resp}");

    let report = f.node.reconcile_refs();
    assert_eq!(report.retracted, vec!["agents/demo.git:refs/heads/ghost".to_string()]);
    assert!(report.applied.is_empty(), "{report:?}");
    assert!(report.unreconciled.is_empty(), "{report:?}");

    let v = f.view();
    assert!(
        v["refs"]["agents/demo.git:refs/heads/ghost"].is_null(),
        "the view still names a commit git does not have: {v}"
    );
    assert_eq!(f.git_ref("refs/heads/ghost"), None);
    // The retraction is an op, not an edit: `main` is untouched and the
    // history still explains itself.
    assert!(v["refs"]["agents/demo.git:refs/heads/main"].is_string(), "{v}");
}

/// A ref git holds and the log does not is reported and left alone.
/// Adopting it would write an out-of-band `update-ref` into the signed
/// log as though someone had submitted it.
#[test]
fn a_ref_only_git_has_is_reported_never_adopted() {
    let f = Fixture::new("outofband");
    let head = f.head();
    assert!(git(&f.bare, &["update-ref", "refs/heads/smuggled", &head]).status.success());

    let report = f.node.reconcile_refs();
    assert!(report.applied.is_empty(), "{report:?}");
    assert!(report.retracted.is_empty(), "{report:?}");
    assert_eq!(
        report.unreconciled,
        vec!["agents/demo.git:refs/heads/smuggled: in git, not in the log".to_string()]
    );

    assert!(
        f.view()["refs"]["agents/demo.git:refs/heads/smuggled"].is_null(),
        "an out-of-band ref must not be laundered into the log"
    );
    assert_eq!(f.git_ref("refs/heads/smuggled").as_deref(), Some(head.as_str()));
}

/// The reason the survey exists apart from the repair: a divergence that
/// appears while the daemon is up has to be visible without restarting
/// it. Reading must also change nothing — a monitor that repairs what it
/// observes would write git refs underneath live pushes.
#[test]
fn a_live_node_reports_a_divergence_without_a_restart() {
    let f = Fixture::new("live");
    let head = f.head();
    let url = format!("http://127.0.0.1:{}/api/ref-agreement", f.port);

    let (code, agreeing) = curl(&[&url]);
    assert_eq!(code, 200, "{agreeing}");
    assert_eq!(agreeing["agree"], serde_json::json!(true), "{agreeing}");
    assert_eq!(agreeing["findings"].as_array().unwrap().len(), 0);

    // Diverge behind the node's back, with it still serving.
    assert!(git(&f.bare, &["update-ref", "-d", "refs/heads/main"]).status.success());

    let (_, seen) = curl(&[&url]);
    assert_eq!(seen["agree"], serde_json::json!(false), "{seen}");
    let findings = seen["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 1, "{seen}");
    assert_eq!(findings[0]["ref"], "agents/demo.git:refs/heads/main");
    assert_eq!(findings[0]["state"], "git_behind");
    assert_eq!(findings[0]["log_oid"], head);
    assert!(findings[0]["git_oid"].is_null(), "{seen}");

    // Reading is not repairing: the ref is still gone from git and the
    // log has not moved.
    assert_eq!(f.git_ref("refs/heads/main"), None);
    assert_eq!(
        f.view()["refs"]["agents/demo.git:refs/heads/main"].as_str(),
        Some(format!("11-{head}").as_str())
    );

    // And the repair the survey feeds still agrees with what it reported.
    let report = f.node.reconcile_refs();
    assert_eq!(report.applied, vec!["agents/demo.git:refs/heads/main".to_string()]);
    let (_, after) = curl(&[&url]);
    assert_eq!(after["agree"], serde_json::json!(true), "{after}");
}

/// The on-box follower feed pushes from the node's own bare repo, and a
/// successful push records what it sent as `refs/remotes/<follower>/*`.
/// That is the repo's private bookkeeping, not canonical state, so it
/// must not be reported at every boot — two warning lines of routine
/// noise per start is exactly where a real out-of-band ref would hide.
#[test]
fn a_remote_tracking_ref_is_not_reported_as_out_of_band() {
    let f = Fixture::new("tracking");
    let head = f.head();
    assert!(
        git(&f.bare, &["update-ref", "refs/remotes/follower/main", &head]).status.success()
    );

    let report = f.node.reconcile_refs();
    assert!(report.is_empty(), "{report:?}");

    // Only the tracking namespace is exempt: a branch smuggled in
    // beside it is still reported.
    assert!(git(&f.bare, &["update-ref", "refs/heads/smuggled", &head]).status.success());
    let report = f.node.reconcile_refs();
    assert_eq!(
        report.unreconciled,
        vec!["agents/demo.git:refs/heads/smuggled: in git, not in the log".to_string()]
    );
}

/// The ordinary start. Agreement must be silent, and must append nothing:
/// a reconciliation that writes an op every boot would grow the log with
/// uptime and make the repair itself the thing to audit.
#[test]
fn an_agreeing_node_reconciles_to_nothing() {
    let f = Fixture::new("agree");
    let before = f.view()["view_growth"]["as_of_seq"].clone();

    let report = f.node.reconcile_refs();
    assert!(report.is_empty(), "{report:?}");

    // Twice, because an idempotence bug shows up on the second pass.
    let report = f.node.reconcile_refs();
    assert!(report.is_empty(), "{report:?}");
    assert_eq!(f.view()["view_growth"]["as_of_seq"], before, "the log moved");
}
