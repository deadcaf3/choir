//! Drives the built `choir` binary against a real node: mint a key,
//! provision a workspace, run a review round end to end, and check the
//! exit-code contract (0 = accepted, 1 = rejected, 2 = usage).

use choir_identity::{ActorKey, Registry};
use choir_node::platform::hex_decode;
use choir_node::{AuthTable, Node, Platform};
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
    // With a name, the same key prints the *bound* line — what the
    // operator appends to bind this key to one review channel.
    let named = choir(&["key", key_file, "cli-agent"]);
    assert_eq!(
        String::from_utf8_lossy(&named.stdout).trim(),
        format!("cli-agent {pub_hex}")
    );

    let mut registry = Registry::new();
    registry.register(&key_bytes).unwrap();
    let mut node = Node::bind(&work.join("repos"), 0).unwrap();
    let pool = work.join("reviewers");
    std::fs::write(&pool, "bot\nana\n").unwrap();
    node.enable_platform(
        Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate())
            .unwrap()
            .with_reviewer_pool(pool),
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

    // Advanced creation binds an exact base and stable change. The same
    // CLI invocation is an idempotent retry, and checkpoints are signed by
    // the bound owner after the Git object has been pushed.
    let advanced_args = [
        "workspace",
        &api,
        "agents/demo",
        "cli-change",
        "--base",
        &head,
        "--owner",
        "cli-agent",
        "--change",
        "change-1",
        "--idempotency-key",
        "request-1",
    ];
    let created = choir(&advanced_args);
    assert!(
        created.status.success(),
        "{:?}",
        String::from_utf8_lossy(&created.stdout)
    );
    let created = json(&created);
    assert_eq!(created["created"], true);
    assert_eq!(created["change_id"], "change-1");
    let retried = choir(&advanced_args);
    assert!(retried.status.success());
    assert_eq!(json(&retried)["reused"], true);

    let change_path = std::path::PathBuf::from(created["path"].as_str().unwrap());
    std::fs::write(change_path.join("f.txt"), "checkpoint\n").unwrap();
    git(&change_path, &["add", "."]);
    git(&change_path, &["commit", "-q", "-m", "checkpoint"]);
    let checkpoint_oid = String::from_utf8_lossy(&git(&change_path, &["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_string();
    assert!(
        git(
            &change_path,
            &["push", "-q", "origin", "HEAD:refs/heads/cli-change"]
        )
        .status
        .success()
    );
    let wrong_owner = choir(&[
        "checkpoint",
        &api,
        key_file,
        "other-agent",
        "change-1",
        "agents/demo/cli-change",
        &checkpoint_oid,
    ]);
    assert_eq!(wrong_owner.status.code(), Some(1));
    assert_eq!(json(&wrong_owner)["code"], "channel_not_owned");
    let checkpointed = choir(&[
        "checkpoint",
        &api,
        key_file,
        "cli-agent",
        "change-1",
        "agents/demo/cli-change",
        &checkpoint_oid,
    ]);
    assert!(
        checkpointed.status.success(),
        "{:?}",
        String::from_utf8_lossy(&checkpointed.stdout)
    );
    let view = json(&choir(&["view", &api]));
    assert_eq!(
        view["changes"]["change-1"]["revision_id"],
        format!("11-{checkpoint_oid}")
    );

    let archive_args = [
        "workspace-archive",
        &api,
        key_file,
        "cli-agent",
        "agents/demo",
        "cli-change",
        "change-1",
        "request-1",
    ];
    let wrong_archive = choir(&[
        "workspace-archive",
        &api,
        key_file,
        "other-agent",
        "agents/demo",
        "cli-change",
        "change-1",
        "request-1",
    ]);
    assert_eq!(wrong_archive.status.code(), Some(1));
    assert!(change_path.exists(), "rejected archive must restore the live path");
    let archived = choir(&archive_args);
    assert!(archived.status.success());
    let archived = json(&archived);
    assert_eq!(archived["already_archived"], false);
    assert!(std::path::Path::new(archived["archived_path"].as_str().unwrap()).exists());
    let archived_again = choir(&archive_args);
    assert!(archived_again.status.success());
    assert_eq!(json(&archived_again)["already_archived"], true);

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

    // `review` with no reviewer names asks the node to draw them from
    // the operator's pool (D24 layer 5) — the requester never picks.
    // `--ref` records where the change wants to land, which is what
    // per-ref policy reads; it is not a reviewer name.
    let out = choir(&[
        "review", &api, key_file, "cli-agent", "r-assigned", &head,
        "--ref", "cli/demo.git:refs/heads/main",
    ]);
    assert!(out.status.success(), "{:?}", String::from_utf8_lossy(&out.stdout));
    let drawn = json(&out)["reviewers"].clone();
    assert_eq!(drawn.as_array().map(Vec::len), Some(2), "{drawn}");
    let view = json(&choir(&["view", &api]));
    assert_eq!(view["reviews"]["r-assigned"]["reviewers"], drawn, "{view}");
    assert_eq!(
        view["reviews"]["r-assigned"]["target_ref"], "cli/demo.git:refs/heads/main",
        "{view}"
    );

    // Raw `submit` accepts a hand-written op (second review request).
    let target = serde_json::to_string(&choir_hash::ContentHash::from_git_oid(&head).unwrap())
        .unwrap();
    let op = format!(
        r#"{{"format_version":1,"kind":{{"RequestReview":{{"id":"r2","target":{target},"reviewers":["bot"]}}}}}}"#
    );
    let out = choir(&["submit", &api, key_file, "cli-agent", &op]);
    assert!(out.status.success(), "{:?}", String::from_utf8_lossy(&out.stdout));

    // Intent record (D22): latest per (subject, kind) wins in the view.
    let out = choir(&["intent", &api, key_file, "cli-agent", "cli-agent", "task-spec", "add auth"]);
    assert!(out.status.success(), "{:?}", String::from_utf8_lossy(&out.stdout));
    let out = choir(&["intent", &api, key_file, "cli-agent", "cli-agent", "task-spec", "add auth v2"]);
    assert!(out.status.success());
    let view = json(&choir(&["view", &api]));
    assert_eq!(view["provenance"]["cli-agent"]["task-spec"], "add auth v2", "{view}");
    // Empty subject is a view-semantics rejection → exit 1.
    let out = choir(&["intent", &api, key_file, "cli-agent", "", "task-spec", "x"]);
    assert_eq!(out.status.code(), Some(1));

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

#[test]
fn cli_reads_auth_from_file_without_exposing_it() {
    let work = std::env::temp_dir().join(format!("choir-cli-auth-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();

    let mut auth = AuthTable::new();
    auth.insert("cli-agent".to_string(), "placeholder-token".to_string());
    let auth_file = work.join("auth");
    std::fs::write(&auth_file, "cli-agent:placeholder-token\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&auth_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    let mut node = Node::bind_with_auth(&work.join("repos"), 0, Some(auth)).unwrap();
    node.enable_platform(
        Platform::start(
            Registry::new(),
            Box::new(MemLog::new()),
            ActorKey::generate(),
        )
        .unwrap(),
    );
    let port = node.port();
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }
    let api = format!("http://127.0.0.1:{port}");

    assert_eq!(choir(&["view", &api]).status.code(), Some(1));
    let out = choir(&[
        "--auth-file",
        auth_file.to_str().unwrap(),
        "--auth-user",
        "cli-agent",
        "view",
        &api,
    ]);
    assert!(out.status.success(), "{:?}", String::from_utf8_lossy(&out.stderr));
    assert!(json(&out)["workspaces"].is_object());
    assert!(!String::from_utf8_lossy(&out.stdout).contains("placeholder-token"));
    assert!(!String::from_utf8_lossy(&out.stderr).contains("placeholder-token"));
    assert_eq!(
        choir(&["--auth-user", "cli-agent", "view", &api])
            .status
            .code(),
        Some(2)
    );

    node.unblock();
}
