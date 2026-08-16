//! A partly accepted multi-ref push must not leave the log ahead of git.
//!
//! `pre-receive` submits one op per ref and git applies no ref until the
//! hook exits zero, so a push whose second ref is refused has the first
//! ref's op in the durable log while git has nothing. Before the hook
//! retracted them, those refs were not merely stranded but permanently
//! unpushable: the pusher's `old` is git's absent value while the view
//! holds the stranded one, so every retry loses the CAS.

use choir_identity::{ActorKey, Registry};
use choir_node::{Node, Platform};
use choir_oplog::MemLog;
use choir_view::{OpKind, ViewOp};

use crate::support::{curl, submit_body};

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
    let (_, v) = curl(&[&format!("http://127.0.0.1:{port}/api/view")]);
    v
}

#[test]
fn a_refused_push_retracts_the_refs_it_already_had_accepted() {
    let work = std::env::temp_dir().join(format!("choir-partial-push-{}", std::process::id()));
    std::fs::create_dir_all(&work).unwrap();

    let alice = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&alice.public_key_bytes()).unwrap();

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
    let bare = work.join("repos").join("agents/demo.git");

    let c1 = work.join("clone1");
    assert!(git(&work, &["clone", "-q", &url, c1.to_str().unwrap()])
        .status
        .success());
    std::fs::write(c1.join("f.txt"), "one\n").unwrap();
    git(&c1, &["add", "."]);
    git(&c1, &["commit", "-q", "-m", "first"]);

    // Put `b` in the view only, so the push's second ref loses its CAS
    // exactly as a concurrent update would have made it lose.
    let op = ViewOp::new(OpKind::SetRef {
        name: "agents/demo.git:refs/heads/b".into(),
        commit: choir_oplog::ContentHash::blake3(b"someone else's b"),
        prev: None,
    });
    let (code, resp) = curl(&[
        "-X",
        "POST",
        "-d",
        &submit_body(&alice, "alice", &op),
        &format!("http://127.0.0.1:{port}/api/submit"),
    ]);
    assert_eq!(code, 200, "{resp}");

    // One push, two refs: `a` is new and admissible, `b` is not.
    let out = git(&c1, &["push", "origin", "HEAD:a", "HEAD:b"]);
    assert!(!out.status.success(), "the push must be refused");

    // Git created neither ref, and neither did the view: the retraction
    // put `a` back where git still has it, which is nowhere.
    assert!(
        !git(&bare, &["rev-parse", "--verify", "-q", "refs/heads/a"])
            .status
            .success(),
        "git must not have created a ref from a refused push"
    );
    let v = view(port);
    assert!(
        v["refs"]["agents/demo.git:refs/heads/a"].is_null(),
        "the view still holds a ref git never created: {v}"
    );
    // `b` was never the accepted one, so the retraction must not have
    // touched the value that refused the push in the first place.
    assert!(
        v["refs"]["agents/demo.git:refs/heads/b"].is_string(),
        "the retraction reached past this push: {v}"
    );

    // The point of retracting: the pusher can still push `a` afterwards.
    // Without it every retry loses the CAS and the ref is unusable.
    let retry = git(&c1, &["push", "origin", "HEAD:a"]);
    assert!(
        retry.status.success(),
        "retry after the refused push: {}",
        String::from_utf8_lossy(&retry.stderr)
    );
    let head = String::from_utf8(git(&c1, &["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(
        view(port)["refs"]["agents/demo.git:refs/heads/a"]
            .as_str()
            .unwrap(),
        format!("11-{head}")
    );

    node.unblock();
    std::fs::remove_dir_all(&work).ok();
}
