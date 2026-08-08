//! Drives the built `choir` binary against a real node: mint a key,
//! provision a workspace, run a review round end to end, and check the
//! exit-code contract (0 = accepted, 1 = rejected, 2 = usage).

use choir_identity::{ActorKey, Registry};
use choir_node::platform::hex_decode;
use choir_node::{Node, Platform};
use choir_oplog::MemLog;

fn choir(args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_choir"))
        .args(args)
        .output()
        .expect("choir runs")
}

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

fn json(out: &std::process::Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
        panic!("json stdout, got {:?}", String::from_utf8_lossy(&out.stdout))
    })
}

#[test]
fn cli_end_to_end() {
    let work = std::env::temp_dir().join(format!("choir-cli-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    let key_file = work.join("agent.key");
    let key_file = key_file.to_str().unwrap();

    // `choir key` mints the key (0600) and prints the hex public key —
    // the exact line an operator appends to the daemon's keys file.
    let out = choir(&["key", key_file]);
    assert!(out.status.success());
    let pub_hex = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let key_bytes: [u8; 32] = hex_decode(&pub_hex)
        .and_then(|b| b.try_into().ok())
        .expect("64 hex chars");
    // Same file again: same key, not a regenerate.
    let again = choir(&["key", key_file]);
    assert_eq!(String::from_utf8_lossy(&again.stdout).trim(), pub_hex);

    let mut registry = Registry::new();
    registry.register(&key_bytes).unwrap();
    let mut node = Node::bind(&work.join("repos"), 0).unwrap();
    node.enable_platform(
        Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate()).unwrap(),
    );
    node.create_repo("agents/demo.git").unwrap();
    let port = node.port();
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }
    let api = format!("http://127.0.0.1:{port}");

    // Seed a commit so provisioning has a head.
    let url = format!("{api}/agents/demo.git");
    let seed = work.join("seed");
    assert!(git(&work, &["clone", "-q", &url, seed.to_str().unwrap()]).status.success());
    std::fs::write(seed.join("f.txt"), "v1\n").unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-q", "-m", "first"]);
    assert!(git(&seed, &["push", "-q", "origin", "HEAD:main"]).status.success());
    let head = String::from_utf8_lossy(&git(&seed, &["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_string();

    // Provision a workspace through the CLI.
    let out = choir(&["workspace", &api, "agents/demo", "cli-agent"]);
    assert!(out.status.success(), "{:?}", String::from_utf8_lossy(&out.stdout));
    let ws = json(&out);
    assert_eq!(ws["head"].as_str().unwrap(), head);
    assert!(std::path::Path::new(ws["path"].as_str().unwrap()).join("f.txt").exists());
    // Rejection surfaces as exit 1 (duplicate name → 409).
    let dup = choir(&["workspace", &api, "agents/demo", "cli-agent"]);
    assert_eq!(dup.status.code(), Some(1));

    // Review round: request (sugar), pending queue, verdict, view.
    let out = choir(&["review", &api, key_file, "cli-agent", "r1", &head, "bot"]);
    assert!(out.status.success(), "{:?}", String::from_utf8_lossy(&out.stdout));
    let out = choir(&["reviews", &api, "bot"]);
    assert!(json(&out)["pending"].get("r1").is_some());
    let out = choir(&["verdict", &api, key_file, "bot", "r1", "approve", "lgtm"]);
    assert!(out.status.success(), "{:?}", String::from_utf8_lossy(&out.stdout));
    let out = choir(&["view", &api]);
    let view = json(&out);
    assert_eq!(view["reviews"]["r1"]["approved"], true, "{view}");
    assert_eq!(view["reviews"]["r1"]["verdicts"]["bot"]["note"], "lgtm");

    // Raw `submit` accepts a hand-written op (second review request).
    let target = serde_json::to_string(&choir_hash::ContentHash::from_git_oid(&head).unwrap())
        .unwrap();
    let op = format!(
        r#"{{"format_version":1,"kind":{{"RequestReview":{{"id":"r2","target":{target},"reviewers":["bot"]}}}}}}"#
    );
    let out = choir(&["submit", &api, key_file, "cli-agent", &op]);
    assert!(out.status.success(), "{:?}", String::from_utf8_lossy(&out.stdout));

    // A non-listed reviewer's verdict is rejected by view semantics → exit 1.
    let out = choir(&["verdict", &api, key_file, "stranger", "r1", "approve"]);
    assert_eq!(out.status.code(), Some(1));
    // Usage errors are exit 2, before any network traffic.
    assert_eq!(choir(&["verdict", &api]).status.code(), Some(2));
    assert_eq!(choir(&["review", &api, key_file, "c", "r3", "not-an-oid", "bot"])
        .status
        .code(), Some(2));

    node.unblock();
}
