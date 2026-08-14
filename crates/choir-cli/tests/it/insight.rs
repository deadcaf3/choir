//! `choir triage` and `choir state` against a real node (internal/oak.md
//! items 2-3): the derivations are unit-tested on a hand-written view,
//! and hand-written samples check the author's imagination, so this
//! drives the built binary through a workspace + review round and reads
//! the buckets and actions off the node's actual emissions.

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

fn action_kinds(doc: &serde_json::Value) -> Vec<String> {
    doc["actions"]
        .as_array()
        .expect("actions array")
        .iter()
        .map(|a| a["action"].as_str().expect("action name").to_string())
        .collect()
}

#[test]
fn triage_and_state_read_real_emissions() {
    let work = std::env::temp_dir().join(format!("choir-insight-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    let key_file = work.join("agent.key");
    let key_file = key_file.to_str().unwrap();

    let out = choir(&["key", key_file]);
    assert!(out.status.success());
    let pub_hex = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let key_bytes: [u8; 32] = hex_decode(&pub_hex)
        .and_then(|b| b.try_into().ok())
        .expect("64 hex chars");

    let mut registry = Registry::new();
    registry.register(&key_bytes).unwrap();
    let mut node = Node::bind(&work.join("repos"), 0).unwrap();
    let node_key_file = work.join("node.key");
    let node_key = ActorKey::generate();
    std::fs::write(&node_key_file, node_key.secret_bytes()).unwrap();
    node.enable_platform(Platform::start(registry, Box::new(MemLog::new()), node_key).unwrap());
    node.create_repo("agents/demo.git").unwrap();
    let port = node.port();
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }
    let api = format!("http://127.0.0.1:{port}");

    // Seed a commit so provisioning has a head; the push lands a SetRef,
    // so the view's refs section is a real emission too.
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

    // A bound change with no checkpoint yet: `state` for the owner must
    // recommend exactly one thing — checkpoint.
    let created = choir(&[
        "workspace", &api, "agents/demo", "insight-change",
        "--base", &head, "--owner", "cli-agent", "--key-file", key_file,
        "--change", "change-1", "--idempotency-key", "request-1",
    ]);
    assert!(created.status.success(), "{:?}", String::from_utf8_lossy(&created.stdout));
    let change_path = std::path::PathBuf::from(json(&created)["path"].as_str().unwrap());

    let state = json(&choir(&["state", &api, "cli-agent"]));
    assert_eq!(action_kinds(&state), ["checkpoint"], "{state}");
    assert_eq!(state["actions"][0]["change"], "change-1");
    assert_eq!(state["refs_visible"], true);
    assert!(state["log"]["node"].is_string(), "log echoed for signing: {state}");

    // Checkpoint a revision; `state` moves on to request-review, and
    // `triage` calls the change unreviewed.
    std::fs::write(change_path.join("f.txt"), "v2\n").unwrap();
    git(&change_path, &["add", "."]);
    git(&change_path, &["commit", "-q", "-m", "work"]);
    let oid = String::from_utf8_lossy(&git(&change_path, &["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_string();
    assert!(git(&change_path, &["push", "-q", "origin", "HEAD:refs/heads/insight-change"])
        .status
        .success());
    let checkpointed = choir(&[
        "checkpoint", &api, key_file, "cli-agent", "change-1",
        "agents/demo/insight-change", &oid,
    ]);
    assert!(checkpointed.status.success(), "{:?}", String::from_utf8_lossy(&checkpointed.stdout));

    let state = json(&choir(&["state", &api, "cli-agent"]));
    assert_eq!(action_kinds(&state), ["request-review"], "{state}");
    let triage = json(&choir(&["triage", &api]));
    assert_eq!(triage["changes"]["change-1"]["bucket"], "unreviewed", "{triage}");

    // Open a review proposing to land on main. The reviewer's state owes
    // a verdict; the owner waits; triage says awaiting-verdicts.
    let target_ref = "agents/demo.git:refs/heads/main";
    let out = choir(&[
        "review", &api, key_file, "cli-agent", "r1", &oid, "--ref", target_ref, "bot",
    ]);
    assert!(out.status.success(), "{:?}", String::from_utf8_lossy(&out.stdout));

    let bot = json(&choir(&["state", &api, "bot"]));
    assert_eq!(action_kinds(&bot), ["answer-review"], "{bot}");
    assert_eq!(bot["actions"][0]["review"], "r1");
    let owner = json(&choir(&["state", &api, "cli-agent"]));
    assert_eq!(action_kinds(&owner), Vec::<String>::new(), "{owner}");
    assert_eq!(owner["waiting"][0]["review"], "r1", "{owner}");
    let triage = json(&choir(&["triage", &api]));
    assert_eq!(triage["reviews"]["r1"]["bucket"], "awaiting-verdicts", "{triage}");
    assert_eq!(triage["changes"]["change-1"]["bucket"], "in-review");

    // Approve. Main has not moved, so the review awaits landing and the
    // owner's one recommended action is to land it.
    let out = choir(&["verdict", &api, key_file, "bot", "r1", "approve", "lgtm"]);
    assert!(out.status.success(), "{:?}", String::from_utf8_lossy(&out.stdout));
    let triage = json(&choir(&["triage", &api]));
    assert_eq!(triage["reviews"]["r1"]["bucket"], "approved-awaiting-landing", "{triage}");
    let owner = json(&choir(&["state", &api, "cli-agent"]));
    assert_eq!(action_kinds(&owner), ["land"], "{owner}");
    assert_eq!(owner["actions"][0]["review"], "r1");

    // Land it the compatibility way — push the commit to main — and the
    // review reads as landed off the node's real refs, not a hand-written
    // map. Everyone's action list drains.
    assert!(git(&change_path, &["push", "-q", "origin", "HEAD:main"]).status.success());
    let triage = json(&choir(&["triage", &api]));
    assert_eq!(triage["reviews"]["r1"]["bucket"], "landed", "{triage}");
    assert_eq!(triage["review_buckets"]["landed"], 1);
    assert_eq!(triage["reviews_omitted"], 0);
    let owner = json(&choir(&["state", &api, "cli-agent"]));
    assert_eq!(action_kinds(&owner), Vec::<String>::new(), "{owner}");

    std::fs::remove_dir_all(&work).ok();
}
