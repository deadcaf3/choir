//! D25 emission: every ref movement the node authors ends in a
//! `RecordRefSnapshot` attesting where the refs now stand, the view
//! serves a summary of the latest one, and its canonical bytes sit in
//! `refs.snapshot` beside the op log — the detached copy a backup pulls
//! with the log, byte-identical to the payload the fold admitted.

use choir_identity::{ActorKey, Registry};
use choir_node::{Node, Platform};
use choir_oplog::FileLog;
use choir_view::RefSnapshot;

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

fn view(port: u16) -> serde_json::Value {
    let out = std::process::Command::new("curl")
        .args(["-s", &format!("http://127.0.0.1:{port}/api/view")])
        .output()
        .expect("curl runs");
    serde_json::from_slice(&out.stdout).expect("view json")
}

#[test]
fn a_push_leaves_an_attestation_in_the_view_and_beside_the_log() {
    let work = std::env::temp_dir().join(format!("choir-snap-emit-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    let log_path = work.join("ops.jsonl");
    let detached = work.join("refs.snapshot");

    let mut node = Node::bind(&work.join("repos"), 0).unwrap();
    node.enable_platform(
        Platform::start(
            Registry::new(),
            Box::new(FileLog::open(&log_path).unwrap()),
            ActorKey::generate(),
        )
        .unwrap()
        .with_log_path(log_path.clone()),
    );
    let port = node.port();
    node.create_repo("agents/demo.git").unwrap();
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }

    // Before any push: nothing to attest, and nothing pretends otherwise.
    assert!(view(port)["snapshot"].is_null());
    assert!(!detached.exists());

    let clone = work.join("clone");
    let url = format!("http://127.0.0.1:{port}/agents/demo.git");
    assert!(git(&work, &["clone", "-q", &url, clone.to_str().unwrap()])
        .status
        .success());
    std::fs::write(clone.join("f.txt"), "attested\n").unwrap();
    git(&clone, &["add", "."]);
    git(&clone, &["commit", "-q", "-m", "first"]);
    let out = git(&clone, &["push", "-q", "origin", "HEAD:main"]);
    assert!(
        out.status.success(),
        "push: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let head = String::from_utf8(git(&clone, &["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_string();

    // The push was SetRef at seq 0, so the attestation of the state it
    // left was taken at position 1 and starts the chain.
    let v = view(port);
    assert_eq!(v["snapshot"]["at_seq"], 1, "{v}");
    assert!(v["snapshot"]["prev_snapshot"].is_null(), "{v}");
    let served_id = v["snapshot"]["id"].as_str().expect("snapshot id").to_string();

    // The detached copy decodes to the same snapshot the view admitted:
    // same id means same canonical bytes, which is the whole contract.
    let bytes = std::fs::read(&detached).expect("detached refs.snapshot exists");
    let snapshot: RefSnapshot = serde_json::from_slice(&bytes).expect("canonical JSON");
    assert_eq!(snapshot.id().to_hex(), served_id);
    assert_eq!(bytes, snapshot.canonical_bytes(), "projection is byte-identical");
    assert_eq!(
        snapshot.refs["agents/demo.git:refs/heads/main"].to_hex(),
        format!("11-{head}")
    );

    // A second push advances the chain, and the detached copy follows.
    std::fs::write(clone.join("f.txt"), "attested twice\n").unwrap();
    git(&clone, &["add", "."]);
    git(&clone, &["commit", "-q", "-m", "second"]);
    assert!(git(&clone, &["push", "-q", "origin", "HEAD:main"]).status.success());
    let v = view(port);
    assert_eq!(v["snapshot"]["at_seq"], 3, "{v}");
    assert_eq!(v["snapshot"]["prev_snapshot"], served_id, "{v}");
    let bytes = std::fs::read(&detached).unwrap();
    let latest: RefSnapshot = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(latest.id().to_hex(), v["snapshot"]["id"].as_str().unwrap());
    assert_eq!(latest.prev_snapshot.as_ref().map(choir_hash::ContentHash::to_hex), Some(served_id));

    node.unblock();
    std::fs::remove_dir_all(&work).ok();
}
