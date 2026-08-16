//! Bridge v0 acceptance against a local upstream: initial sync lands
//! every ref in the choir view, an upstream advance CAS-updates it, an
//! upstream branch deletion deletes it, and a no-change round is a
//! no-op (idempotence).

use choir_identity::{ActorKey, Registry};
use choir_node::{Node, Platform};
use choir_oplog::MemLog;

fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args([
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
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn bridge_once(
    upstream: &str,
    mirror: &std::path::Path,
    api: &str,
    key: &std::path::Path,
    label: &str,
) -> String {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_choir-bridge"))
        .args([
            upstream,
            mirror.to_str().unwrap(),
            api,
            key.to_str().unwrap(),
            label,
            "--once",
        ])
        .output()
        .expect("bridge runs");
    assert!(
        out.status.success(),
        "bridge: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn view(port: u16) -> serde_json::Value {
    let out = std::process::Command::new("curl")
        .args(["-s", &format!("http://127.0.0.1:{port}/api/view")])
        .output()
        .expect("curl runs");
    serde_json::from_slice(&out.stdout).expect("view json")
}

#[test]
fn read_replica_tracks_upstream() {
    let work = std::env::temp_dir().join(format!("choir-bridge-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();

    // Local "GitHub": a bare upstream with two branches and a tag.
    let upstream = work.join("upstream.git");
    git(&work, &["init", "-q", "--bare", upstream.to_str().unwrap()]);
    let src = work.join("src");
    git(
        &work,
        &[
            "clone",
            "-q",
            upstream.to_str().unwrap(),
            src.to_str().unwrap(),
        ],
    );
    std::fs::write(src.join("a.txt"), "one\n").unwrap();
    git(&src, &["add", "."]);
    git(&src, &["commit", "-q", "-m", "c1"]);
    git(&src, &["push", "-q", "origin", "HEAD:main"]);
    git(&src, &["push", "-q", "origin", "HEAD:refs/heads/feature"]);
    git(&src, &["tag", "v1"]);
    git(&src, &["push", "-q", "origin", "v1"]);

    // Bridge key, pre-registered with the daemon.
    let key = ActorKey::generate();
    let key_file = work.join("bridge.key");
    std::fs::write(&key_file, key.secret_bytes()).unwrap();
    let mut registry = Registry::new();
    registry.register(&key.public_key_bytes()).unwrap();

    let mut node = Node::bind(&work.join("repos"), 0).unwrap();
    node.enable_platform(
        Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate()).unwrap(),
    );
    let port = node.port();
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }
    let api = format!("http://127.0.0.1:{port}");
    let mirror = work.join("mirror.git");
    let label = "local/up";

    // Round 1: everything lands (main, feature, v1 -- HEAD is symbolic,
    // not in for-each-ref).
    let log = bridge_once(upstream.to_str().unwrap(), &mirror, &api, &key_file, label);
    assert!(log.contains("3 set, 0 deleted, 0 unchanged"), "{log}");
    let head = git(&src, &["rev-parse", "HEAD"]).trim().to_string();
    let v = view(port);
    assert_eq!(v["refs"]["local/up:refs/heads/main"], format!("11-{head}"));
    assert_eq!(v["refs"]["local/up:refs/tags/v1"], format!("11-{head}"));

    // Round 2: no change -> no ops.
    let log = bridge_once(upstream.to_str().unwrap(), &mirror, &api, &key_file, label);
    assert!(log.contains("0 set, 0 deleted, 3 unchanged"), "{log}");

    // Round 3: advance main, delete feature.
    std::fs::write(src.join("a.txt"), "two\n").unwrap();
    git(&src, &["add", "."]);
    git(&src, &["commit", "-q", "-m", "c2"]);
    git(&src, &["push", "-q", "origin", "HEAD:main"]);
    git(&src, &["push", "-q", "origin", ":refs/heads/feature"]);
    let log = bridge_once(upstream.to_str().unwrap(), &mirror, &api, &key_file, label);
    assert!(log.contains("1 set, 1 deleted, 1 unchanged"), "{log}");
    let head2 = git(&src, &["rev-parse", "HEAD"]).trim().to_string();
    let v = view(port);
    assert_eq!(v["refs"]["local/up:refs/heads/main"], format!("11-{head2}"));
    assert!(v["refs"]["local/up:refs/heads/feature"].is_null());

    node.unblock();
    std::fs::remove_dir_all(&work).ok();
}
